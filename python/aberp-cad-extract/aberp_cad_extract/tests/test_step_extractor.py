"""STEP extractor — OCCT-backed FeatureGraph correctness.

Tests skip if ``cadquery-ocp`` is not installed (the ``[step]`` extra
is opt-in). The pinned fixtures under ``tests/fixtures/`` are OCCT-
generated STEP files committed once via the helper in the PR-273
session writeup — re-generating them per test would couple the suite
to the host OCCT minor version.
"""

from __future__ import annotations

from pathlib import Path

import pytest

ocp = pytest.importorskip("OCP", reason="STEP tests require `pip install -e '.[step]'`")

from aberp_cad_extract.extractors.step import extract_step  # noqa: E402
from aberp_cad_extract.feature_graph import FeatureGraph  # noqa: E402


def test_cube_bounding_box_and_volume(step_cube_path: Path):
    fg = extract_step(step_cube_path, material_grade="6061-T6")
    assert isinstance(fg, FeatureGraph)
    bx, by, bz = fg.bounding_box_mm
    # AddOptimal_s gives a tight bbox; tolerance covers OCCT float
    # rounding without admitting the default-bbox inflation (~2e-7).
    assert bx == pytest.approx(20.0, abs=1e-3)
    assert by == pytest.approx(20.0, abs=1e-3)
    assert bz == pytest.approx(20.0, abs=1e-3)
    # 20^3 = 8000 mm^3
    assert fg.volume_mm3 == pytest.approx(8_000.0, abs=1e-3)
    assert fg.material_grade == "6061-T6"
    # v1 STEP path returns empty feature list — BREP feature mining is a
    # follow-on cut, same posture as the STL extractor.
    assert fg.features == []


def test_addendum_1_booleans_always_populated(step_cube_path: Path):
    fg = extract_step(step_cube_path, material_grade="6061-T6")
    assert isinstance(fg.requires_5_axis, bool)
    assert isinstance(fg.thin_wall_present, bool)


def test_thin_plate_flags_thin_wall(step_thin_plate_path: Path):
    fg = extract_step(step_thin_plate_path, material_grade="6061-T6")
    # 0.8 mm < the 1.5 mm threshold — thin-wall positive.
    assert fg.thin_wall_present is True
    # A mid-aspect plate fills its bbox, so the conservative 5-axis gate
    # stays False.
    assert fg.requires_5_axis is False


def test_canonical_dict_uses_wire_field_names(step_cube_path: Path):
    fg = extract_step(step_cube_path, material_grade="6061-T6")
    out = fg.to_canonical_dict()
    assert out["_schema_version"] == 1
    assert "requires_5_axis" in out
    assert "thin_wall_present" in out
    assert isinstance(out["requires_5_axis"], bool)
    assert isinstance(out["thin_wall_present"], bool)


def test_assembly_step_rejected_with_classifier_friendly_message(
    step_assembly_path: Path,
):
    with pytest.raises(ValueError) as excinfo:
        extract_step(step_assembly_path, material_grade="6061-T6")
    msg = str(excinfo.value).lower()
    # The Rust-side classifier (S290+S292) matches "step file" at the
    # extract stage → Permanent. Keep that exact substring in the message.
    assert "step file" in msg
    assert "assembly" in msg


def test_missing_file_surfaces_as_value_error(tmp_path: Path):
    ghost = tmp_path / "missing.step"
    with pytest.raises(ValueError) as excinfo:
        extract_step(ghost, material_grade="6061-T6")
    msg = str(excinfo.value).lower()
    assert "step file" in msg
    # OCCT's ReadFile returns a non-RetDone status for missing files;
    # the wrapper surfaces it with "could not be parsed".
    assert "could not be parsed" in msg


# ── PR-274 / S297 F2 — STEP unit-of-measure normalisation ────────────


def _flip_length_unit(src: Path, dst: Path, new_prefix: str) -> None:
    """Hand-edit a STEP file's ``SI_UNIT(.MILLI.,.METRE.)`` declaration.

    Replaces the first ``SI_UNIT(.MILLI.,.METRE.)`` site with
    ``SI_UNIT(.<new_prefix>.,.METRE.)`` (or with a bare ``$`` for the
    "no prefix" METRE form). Geometry coords stay untouched so we
    exercise the conversion path: the file declares a non-MM unit but
    the numeric coords are unchanged, and OCCT must convert them on
    import per ``xstep.cascade.unit``.
    """
    content = src.read_bytes().decode("latin-1")
    old = "SI_UNIT(.MILLI.,.METRE.)"
    assert old in content, "fixture format drifted — expected SI_UNIT(.MILLI.,.METRE.)"
    if new_prefix == "":  # bare METRE
        new = "SI_UNIT($,.METRE.)"
    else:
        new = f"SI_UNIT(.{new_prefix}.,.METRE.)"
    # Replace only the first SI_UNIT (the LENGTH_UNIT site, line ~409
    # in the fixture). The radian / steradian unit decls use `$` prefix
    # so they're untouched by this replace.
    content = content.replace(old, new, 1)
    dst.write_bytes(content.encode("latin-1"))


def test_step_unit_centi_metre_normalises_to_mm(tmp_path: Path, step_cube_path: Path):
    """Hand-edit the unit cube to declare CENTI METRE; OCCT must scale.

    The 20 mm cube fixture has coords ±10 (in mm); flipping the
    LENGTH_UNIT to CENTI METRE without changing coords means OCCT
    should now interpret those ±10 as ±10 cm = ±100 mm — bbox extent
    200 mm. If our ``Interface_Static.SetCVal_s("xstep.cascade.unit",
    "MM")`` ever stops working — or OCCT/OCP changes its default — the
    file would read back at ±10 mm (extent 20) and this test fails
    loud. The c1cf32 forensic case S290 was written to surface depends
    on this not silently re-opening as a wrong-units quote (review
    F2).
    """
    cm_path = tmp_path / "unit_cube_centi_metre.step"
    _flip_length_unit(step_cube_path, cm_path, "CENTI")
    fg = extract_step(cm_path, material_grade="6061-T6")
    bx, by, bz = fg.bounding_box_mm
    # 10 cm = 100 mm; coords ±10 in the file → extent 200 mm.
    assert bx == pytest.approx(200.0, abs=1e-2), f"x extent={bx}, file declared CENTI METRE"
    assert by == pytest.approx(200.0, abs=1e-2)
    assert bz == pytest.approx(200.0, abs=1e-2)
    # Volume 200^3 = 8,000,000 mm^3.
    assert fg.volume_mm3 == pytest.approx(8_000_000.0, rel=1e-3)


def test_step_cascade_unit_pinned_to_mm_after_extract(step_cube_path: Path):
    """Invariant pin: the unit-conversion parameter equals "MM" after a read.

    Direct check of the OCCT global-state knob ``extract_step`` sets.
    If a refactor ever removes or mis-names the ``SetCVal_s`` call —
    or a future ``Interface_Static`` API rename slips through — this
    pin fails immediately rather than waiting for the (rarer) METRE
    fixture round-trip to misbehave.
    """
    from OCP.Interface import Interface_Static  # noqa: E402  — gated by importorskip above

    extract_step(step_cube_path, material_grade="6061-T6")
    assert Interface_Static.CVal_s("xstep.cascade.unit") == "MM"
