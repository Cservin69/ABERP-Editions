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
