"""ADR-0112 Part B — located-hole mining, pinned against exact geometry.

Every expected number here is the number the fixture was BUILT from
(``tools/generate_step_fixtures.py`` cuts each part from written-down
dimensions via OCCT primitives + booleans), not a number measured off a
drawing or read out of a viewer. So a failure means the miner is wrong,
never that the fixture drifted.

Tolerances are 1e-6 mm — this is exact primitive geometry through an
exact kernel, so anything looser would hide a real defect.
"""

from __future__ import annotations

from pathlib import Path

import pytest

ocp = pytest.importorskip("OCP", reason="hole mining requires `pip install -e '.[step]'`")

from aberp_cad_extract.extractors.step import (  # noqa: E402
    _load_step_shape,
    _silence_stdout_fd,
    extract_step,
)
from aberp_cad_extract.feature_graph import HoleEndCondition  # noqa: E402
from aberp_cad_extract.holes import mine_cylindrical_holes  # noqa: E402

TOL = 1e-6


def _mine(path: Path):
    with _silence_stdout_fd():
        shape = _load_step_shape(str(path))
        return mine_cylindrical_holes(shape)


def _approx(value, expected):
    assert value == pytest.approx(expected, abs=TOL)


def _approx_vec(value, expected):
    assert len(value) == 3
    for got, want in zip(value, expected):
        assert got == pytest.approx(want, abs=TOL)


# ── the headline pin: count, diameters, depths, positions ────────────────


def test_plate_four_through_holes_exact(fixtures_dir: Path):
    """100x60x12 plate, four Ø8.0 through-holes at a 20 mm inset.

    The primary ADR-0112 Part B pin: count, diameter, depth, axis AND
    position, all four holes, all exact.
    """
    holes = _mine(fixtures_dir / "plate_4_through_holes.step")

    assert len(holes) == 4, f"expected exactly 4 holes, got {len(holes)}"

    expected_entries = [
        (20.0, 20.0, 0.0),
        (20.0, 40.0, 0.0),
        (80.0, 20.0, 0.0),
        (80.0, 40.0, 0.0),
    ]
    for hole, entry in zip(holes, expected_entries):
        _approx(hole.diameter_mm, 8.0)
        _approx(hole.depth_mm, 12.0)  # the plate's full thickness
        _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
        _approx_vec(hole.entry_point_mm, entry)
        assert hole.end_condition is HoleEndCondition.THROUGH
        assert hole.flat_bottom is False


def test_axis_is_a_unit_vector(fixtures_dir: Path):
    """`axis_unit` is normalised by the EXTRACTOR — the engine does not
    second-guess it, so the invariant has to hold here."""
    for name in (
        "plate_4_through_holes.step",
        "blind_hole_flat_bottom.step",
        "stepped_bore.step",
        "coaxial_split_faces.step",
    ):
        for hole in _mine(fixtures_dir / name):
            norm = sum(c * c for c in hole.axis_unit) ** 0.5
            assert norm == pytest.approx(1.0, abs=TOL), f"{name}: axis not unit"


# ── the two correctness risks the ADR named ──────────────────────────────


def test_tube_outer_diameter_is_not_reported_as_a_hole(fixtures_dir: Path):
    """Correctness risk #1: a bar OD is a full-sweep cylindrical face too.

    Ø40 x 50 tube with a Ø20 bore. Only the material side distinguishes
    the two surfaces. Reporting the OD would invent a Ø40 x 50 drilling
    operation that never happens — on every turned part in the shop.
    """
    holes = _mine(fixtures_dir / "tube_od_not_a_hole.step")

    assert len(holes) == 1, (
        "the Ø40 OUTER surface must NOT be counted as a hole; "
        f"got {[h.diameter_mm for h in holes]}"
    )
    _approx(holes[0].diameter_mm, 20.0)
    _approx(holes[0].depth_mm, 50.0)


def test_coaxial_split_faces_merge_into_one_hole(fixtures_dir: Path):
    """Correctness risk #2: one bore, severed into two faces by a slot.

    Without the coaxial merge this reports as TWO Ø9.0 holes — a 2x
    drilling over-price on that hole. The merged span must also be the
    FULL 40 mm, not either half.
    """
    holes = _mine(fixtures_dir / "coaxial_split_faces.step")

    assert len(holes) == 1, (
        "a bore split into two coaxial faces is ONE hole; "
        f"got {len(holes)} — this is the 2x over-price failure mode"
    )
    _approx(holes[0].diameter_mm, 9.0)
    _approx(holes[0].depth_mm, 40.0)  # the whole block, not one half
    _approx_vec(holes[0].entry_point_mm, (15.0, 15.0, 0.0))


def test_stepped_bore_stays_two_holes(fixtures_dir: Path):
    """The merge must NOT be over-eager: a counterbore is two operations.

    Ø6.0 through-hole with a Ø14.0 x 8.0 counterbore, coaxial but at
    different radii. Two tools, two drilling operations, two entries.
    """
    holes = _mine(fixtures_dir / "stepped_bore.step")

    assert len(holes) == 2, (
        "a counterbore is two tools and must stay two holes; "
        f"got {[h.diameter_mm for h in holes]}"
    )
    small, large = sorted(holes, key=lambda h: h.diameter_mm)
    _approx(small.diameter_mm, 6.0)
    _approx(small.depth_mm, 17.0)  # 25 block - 8 counterbore
    _approx(large.diameter_mm, 14.0)
    _approx(large.depth_mm, 8.0)
    # Same axis line, different entry heights.
    _approx_vec(small.entry_point_mm, (25.0, 25.0, 0.0))
    _approx_vec(large.entry_point_mm, (25.0, 25.0, 17.0))

    # NOTE, deliberately not asserted: the miner classifies BOTH of these
    # as THROUGH, including the counterbore. That is geometrically honest
    # — the Ø14 cylindrical face really does open to air at both ends
    # (upward to the top face, downward into the Ø6 bore) — but it is not
    # what a machinist means by "through", and Part C should not read it
    # as "no peck retract needed". Left unasserted rather than pinned so
    # this test does not lock in an interpretation Part C may need to
    # refine; flagged here so the next reader does not think it was
    # missed.


# ── end condition + flat bottom ──────────────────────────────────────────


def test_blind_flat_bottom_hole(fixtures_dir: Path):
    """40x40x30 block, Ø10.0 flat-bottomed blind bore 18 mm deep.

    Three separate things are pinned, and each one is a distinct way to
    under-price: BLIND (a blind hole mis-read as through under-counts
    peck cycles), flat_bottom (a different, slower cycle than a 118°
    point), and the ENTRY being the OPEN end with the axis pointing INTO
    the material — an entry at the closed end would put the drill inside
    the block.
    """
    holes = _mine(fixtures_dir / "blind_hole_flat_bottom.step")

    assert len(holes) == 1
    hole = holes[0]
    _approx(hole.diameter_mm, 10.0)
    _approx(hole.depth_mm, 18.0)
    assert hole.end_condition is HoleEndCondition.BLIND, (
        "a bore that terminates in material is BLIND — reading it as "
        "THROUGH under-counts peck cycles and under-prices"
    )
    assert hole.flat_bottom is True
    # Entry is the OPEN top face (z=30) and the drill travels DOWN.
    _approx_vec(hole.entry_point_mm, (20.0, 20.0, 30.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))


def test_hole_free_part_yields_no_holes(fixtures_dir: Path):
    """A solid cube has no cylindrical faces at all — empty, not an error.

    This is the case that keeps every pre-v6 stored graph honest: empty
    is the historical value, so a hole-less part must produce exactly it.
    """
    assert _mine(fixtures_dir / "unit_cube.step") == []


# ── determinism ──────────────────────────────────────────────────────────


def test_hole_order_is_deterministic(fixtures_dir: Path):
    """Emission order is CONTRACTUAL.

    OCCT's explorer order is not guaranteed stable across versions, and
    the daemon blake3-hashes the canonical encoding into
    `feature_graph_hash`. An unstable order would change that hash for an
    unchanged part, on an OCCT upgrade, silently.
    """
    path = fixtures_dir / "plate_4_through_holes.step"
    runs = [
        [
            (h.diameter_mm, h.depth_mm, tuple(h.entry_point_mm))
            for h in _mine(path)
        ]
        for _ in range(3)
    ]
    assert runs[0] == runs[1] == runs[2]
    # And it is the documented sort: entry XYZ, then diameter, then depth.
    assert runs[0] == sorted(runs[0], key=lambda r: (r[2], r[0], r[1]))


# ── wired into the extractor + the wire contract ─────────────────────────


def test_extract_step_populates_located_holes(fixtures_dir: Path):
    """The miner is actually wired into `extract_step`, not merely present."""
    fg = extract_step(fixtures_dir / "plate_4_through_holes.step", material_grade="6061-T6")
    assert fg.schema_version == 6
    assert len(fg.located_holes) == 4
    assert all(h.diameter_mm == pytest.approx(8.0, abs=TOL) for h in fg.located_holes)

    payload = fg.to_canonical_dict()
    assert "located_holes" in payload
    hole = payload["located_holes"][0]
    # The wire shape the Rust `LocatedHole` deserialises.
    assert set(hole) == {
        "diameter_mm",
        "depth_mm",
        "axis_unit",
        "entry_point_mm",
        "end_condition",
        "flat_bottom",
    }
    assert hole["end_condition"] == "through"


def test_hole_free_part_omits_the_key_entirely(fixtures_dir: Path):
    """PIN: empty `located_holes` emits NO JSON key.

    This is what keeps a hole-less part's `feature_graph_hash`
    byte-identical to its pre-v6 value — the daemon hashes this exact
    encoding. It mirrors the Rust side's
    `skip_serializing_if = "Vec::is_empty"`; both halves must agree or
    the round-trip stops being byte-identical.
    """
    fg = extract_step(fixtures_dir / "unit_cube.step", material_grade="6061-T6")
    assert fg.located_holes == []

    payload = fg.to_canonical_dict()
    assert "located_holes" not in payload, (
        "an empty located_holes must emit NO key — an empty array would "
        "silently change feature_graph_hash for every hole-less part"
    )


def test_hole_mining_failure_does_not_kill_the_extraction(
    fixtures_dir: Path, monkeypatch
):
    """PIN the deliberate failure posture (ADR-0112 B.4).

    Hole mining must NEVER fail the extraction: an empty hole list is a
    known-conservative degradation (it is exactly what every pre-v6 graph
    carries) and the part still prices at today's hole-blind number,
    rather than the quote dying. This is deliberately the OPPOSITE of the
    posture on shape errors, where failing loud is right.
    """
    import aberp_cad_extract.extractors.step as step_mod

    def boom(_shape):
        raise RuntimeError("simulated OCCT face-walk explosion")

    monkeypatch.setattr(step_mod, "mine_cylindrical_holes", boom)

    fg = extract_step(fixtures_dir / "plate_4_through_holes.step", material_grade="6061-T6")
    assert fg.located_holes == [], "a mining failure degrades to empty, never raises"
    # …and the rest of the graph is intact and priceable.
    assert fg.volume_mm3 > 0.0
    assert fg.bounding_box_mm[0] == pytest.approx(100.0, abs=1e-3)
