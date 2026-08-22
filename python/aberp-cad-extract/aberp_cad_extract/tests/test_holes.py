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

import contextlib
import math
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


def _miner_ast():
    """`holes.py` parsed, for the two N2 probes that pin a STRUCTURAL
    property of the module rather than a value it computes.

    Parsed, not grepped: the module's own prose talks about the
    `BRepClass3d_SolidClassifier` it no longer uses and about the
    `BRepAdaptor_Surface` default it deliberately avoids, and a text
    search cannot tell an explanation from a call.
    """
    import ast
    import inspect

    import aberp_cad_extract.holes as holes_mod

    return ast.parse(inspect.getsource(holes_mod))


def _imported_names(tree):
    """Every module path and symbol the miner imports."""
    import ast

    names = []
    for node in ast.walk(tree):
        if isinstance(node, ast.ImportFrom):
            names.append(node.module or "")
            names.extend(alias.name for alias in node.names)
        elif isinstance(node, ast.Import):
            names.extend(alias.name for alias in node.names)
    return names


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
        "blind_hole_drill_point.step",
        "stepped_bore.step",
        "coaxial_split_faces.step",
        "seam_split_bore.step",
        "angled_through_hole.step",
        "angled_blind_hole.step",
        "both_sides_drilled.step",
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

    # The Ø6 runs the full way out at both ends: a real through-hole,
    # entered from the bottom face.
    _approx_vec(small.entry_point_mm, (25.0, 25.0, 0.0))
    assert small.end_condition is HoleEndCondition.THROUGH

    # The counterbore is entered from the TOP (z=25) and bottoms out on
    # its own annular shoulder at z=17.
    #
    # CHANGED, and deliberately, by the ADR-0112 adversarial round: this
    # used to assert entry (25,25,17) — the CLOSED end — because the miner
    # called the counterbore THROUGH and then took its lowest point as the
    # entry. The previous revision of this test flagged that in a standing
    # NOTE ("geometrically honest ... but not what a machinist means by
    # through, and Part C should not read it as 'no peck retract
    # needed'"). Probing the whole bore cross-section rather than just the
    # axis (see `_end_is_open`) settles it: the shoulder is material, so
    # the end is closed, so the entry is the top face and the drill
    # travels DOWN. The old expectation was the defect, not the pin.
    _approx_vec(large.entry_point_mm, (25.0, 25.0, 25.0))
    _approx_vec(large.axis_unit, (0.0, 0.0, -1.0))
    assert large.end_condition is HoleEndCondition.BLIND
    assert large.flat_bottom is True


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


# ── ADR-0112 adversarial round 1 — the five blockers ─────────────────────
#
# Each of these RED-lights the miner as it was shipped, with the number
# the adversarial actually measured recorded in the failure message. A
# probe that cannot fail is worth nothing, so every one of them was run
# against the pre-fix miner and confirmed to fail before the fix landed.


def test_b1_seam_split_bore_is_still_one_hole(fixtures_dir: Path):
    """B1: a bore split into quarter-faces must not VANISH.

    The shipped miner rejected partial sweeps per face, before the coaxial
    merge that exists to rejoin exactly this. All four quarters of a plain
    Ø8 through-bore were thrown away individually and the part reported
    ZERO holes — an under-count, so an under-price, and silent. Any CAD
    export that splits cylinders at the seam (or any
    `ShapeUpgrade_ShapeDivideAngle` pass) triggers it.
    """
    holes = _mine(fixtures_dir / "seam_split_bore.step")

    assert len(holes) == 1, (
        "a bore whose cylinder is split into quarter-faces is still ONE "
        f"hole; got {len(holes)} (the shipped miner reported 0 — the hole "
        "disappeared entirely)"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    _approx(hole.depth_mm, 20.0)
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    _approx_vec(hole.entry_point_mm, (20.0, 20.0, 0.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_b2_conical_drill_point_is_a_closed_end(fixtures_dir: Path):
    """B2: a 118° drill point is a BOTTOM, not an opening.

    The shipped end probe walked the axis only. A conical point leaves the
    axis in void for the whole height of the cone, so the probe read the
    end as open and reported the hole THROUGH — with its entry at the
    CLOSED end and its axis pointing out of the part. Three separate ways
    to be wrong on one hole, and the only committed blind fixture was
    flat-bottomed, which is the single blind shape the axis probe got
    right by accident.
    """
    holes = _mine(fixtures_dir / "blind_hole_drill_point.step")

    assert len(holes) == 1
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.end_condition is HoleEndCondition.BLIND, (
        "a bore ending in a 118° conical point is BLIND; the shipped "
        "miner said THROUGH, which drops the peck cycles"
    )
    # Entry is the OPEN top face and the drill travels DOWN. The shipped
    # miner put the entry at (20,20,15) — the bore bottom, inside the
    # block — with the axis pointing up and out of the material.
    _approx_vec(hole.entry_point_mm, (20.0, 20.0, 40.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))
    # The FULL-DIAMETER depth, z=15..40. The cone below it is the drill
    # point's own 0.3·D and is not full-diameter material removal; see
    # `_true_axial_span`. Part C may want the point length as well — it is
    # 4/tan(59°) here — but it is not this number.
    _approx(hole.depth_mm, 25.0)
    assert hole.flat_bottom is False, "a 118° point is not a flat bottom"


def test_b3_coaxial_bores_do_not_merge_across_air(fixtures_dir: Path):
    """B3: two walls, two holes — never one hole through the air between.

    The shipped `_same_bore` tested radius and axis and nothing else, so
    two 10 mm walls 80 mm apart came back as ONE hole of depth 100.0. Two
    drilling operations priced as one, at a depth no drill in the shop
    has. This is the under-count direction and therefore the expensive
    one: an over-count is visible in the reasoning log, an under-count is
    not.
    """
    holes = _mine(fixtures_dir / "two_walls_far_apart.step")

    assert len(holes) == 2, (
        "two coaxial bores separated by 80 mm of air are TWO holes; "
        f"got {len(holes)} with depths {[h.depth_mm for h in holes]} "
        "(the shipped miner reported 1 hole of depth 100.0)"
    )
    for hole, z in zip(holes, (0.0, 90.0)):
        _approx(hole.diameter_mm, 8.0)
        _approx(hole.depth_mm, 10.0)  # one wall each, never the span
        _approx_vec(hole.entry_point_mm, (25.0, 20.0, z))
        assert hole.end_condition is HoleEndCondition.THROUGH


def test_b3_gap_rule_is_bracketed_from_both_sides(fixtures_dir: Path):
    """The contiguity threshold is pinned, not merely present.

    `two_walls_far_apart` alone leaves `MAX_MERGE_GAP_DIAMETERS` free
    anywhere below 10 diameters, which is no constraint worth having.
    These two fixtures squeeze it from both ends: a 4 mm slot across a Ø9
    bore (0.44 D) must still be ONE hole, and 20 mm between walls on a Ø8
    bore (2.5 D) must be TWO. Widening the constant past 2.5 or
    tightening it below 0.45 breaks one of them.
    """
    merged = _mine(fixtures_dir / "coaxial_split_faces.step")
    assert len(merged) == 1, "0.44 diameters of gap is one interrupted bore"
    _approx(merged[0].depth_mm, 40.0)

    split = _mine(fixtures_dir / "two_walls_gapped.step")
    assert len(split) == 2, "2.5 diameters of gap is two bores"
    for hole, z in zip(split, (0.0, 30.0)):
        _approx(hole.depth_mm, 10.0)
        _approx_vec(hole.entry_point_mm, (25.0, 20.0, z))


def test_b4_angled_through_hole_depth_and_entry_are_real(fixtures_dir: Path):
    """B4: depth and entry come from the geometry, not from UVBounds.

    `BRepTools.UVBounds_s` is the PARAMETRIC bounding box; for an angled
    bore it stretches to the extreme of the entry ellipse instead of
    stopping at its centre. The shipped miner reported this hole 20% too
    deep with its entry 2.08 mm clear of the part, floating in air — a
    coordinate you cannot post to a machine.

    Both expected numbers are stated by construction, not measured: a
    20 mm plate crossed at 30° is exactly 20/cos(30°) deep, and the axis
    meets z=0 at exactly x = 30 - 20·sin30 + (20/cos30)·sin30.
    """
    holes = _mine(fixtures_dir / "angled_through_hole.step")

    assert len(holes) == 1
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    _approx(hole.depth_mm, 20.0 / math.cos(math.radians(30.0)))
    entry_x = 30.0 - 20.0 * math.sin(math.radians(30.0)) + (
        20.0 / math.cos(math.radians(30.0))
    ) * math.sin(math.radians(30.0))
    _approx_vec(hole.entry_point_mm, (entry_x, 30.0, 0.0))
    # …and the entry is ON the face, not above it. Pinned separately
    # because "z is 0" is the whole point: the shipped value was -2.0828.
    assert hole.entry_point_mm[2] == pytest.approx(0.0, abs=TOL), (
        "the entry point must sit ON the part's face; the shipped miner "
        f"put it at z={hole.entry_point_mm[2]}, in mid-air"
    )
    _approx_vec(
        hole.axis_unit,
        (math.sin(math.radians(30.0)), 0.0, math.cos(math.radians(30.0))),
    )


def test_b4_angled_blind_hole_depth_and_entry_are_real(fixtures_dir: Path):
    """B4's blind arm — the worst of the family (the adversarial's +15%).

    Same defect, and here it also put the entry point ABOVE the top face.
    Built entering at exactly (20,30,40) and running exactly 20.0 mm along
    its own axis at 45°.
    """
    holes = _mine(fixtures_dir / "angled_blind_hole.step")

    assert len(holes) == 1
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    _approx(hole.depth_mm, 20.0)
    _approx_vec(hole.entry_point_mm, (20.0, 30.0, 40.0))
    k = math.sqrt(0.5)
    _approx_vec(hole.axis_unit, (k, 0.0, -k))
    assert hole.end_condition is HoleEndCondition.BLIND
    assert hole.flat_bottom is True


def test_b5_filleted_block_has_no_holes(fixtures_dir: Path):
    """B5: a fillet is not a bore. The convex arm, end-to-end.

    This is the part whose absence made the shipped partial-sweep guard
    VACUOUS: deleting that guard left the suite 57/0 green, and a plain
    filleted block then reported SIX phantom Ø10 "holes" — six drilling
    operations invented on a part that has none, on anything with rounded
    edges.

    HONEST LIMIT, because a probe that cannot fail is worth nothing and
    this one is weaker than it looks: OCCT's STEP writer NORMALISES face
    orientation, so after the round-trip none of this fixture's twelve
    fillet faces come back REVERSED and the orientation arm alone rejects
    them. No single-guard mutation reds this test. The probe that carries
    real teeth for the fillet family is
    `test_b5_concave_fillet_is_not_a_hole` (reds the moment the sweep
    union is weakened) and, for the exact shape the adversarial ran,
    `test_b5_in_memory_filleted_block_is_where_the_sweep_union_bites`
    below.
    This one stays as the end-to-end statement of the user-visible
    property: rounded edges must not become drilling operations.
    """
    holes = _mine(fixtures_dir / "filleted_block.step")

    assert holes == [], (
        "edge fillets are not holes; got "
        f"{[(h.diameter_mm, h.entry_point_mm) for h in holes]}"
    )


def test_b5_in_memory_filleted_block_is_where_the_sweep_union_bites():
    """B5: the adversarial's exact shape, before STEP flattens it.

    Built in memory rather than read from a committed fixture, and
    deliberately so — this is the ONE place the suite can present a face
    orientation that OCCT's STEP writer normalises away. Straight off
    `BRepFilletAPI_MakeFillet`, six of the twelve quarter-cylinders come
    back REVERSED, which is exactly what a bore looks like to
    `_is_bore_face`; those six were the six phantom Ø10 holes.

    REWRITTEN (ADR-0112 adversarial round 2, correction 1). Round 1 named
    this test as the place its axis-in-the-void arm was "pinned in
    memory", and that framing was wrong twice: the arm was never the only
    thing rejecting these faces, and what it really pinned elsewhere was
    a false negative (see
    `test_c1_a_bore_over_a_centre_post_is_not_dropped`). With the arm
    removed, this part is now the strongest statement of what carries the
    convex-fillet case: `_is_bore_face` waves all six REVERSED
    quarter-cylinders through, and the post-merge SWEEP UNION is the only
    thing between them and six invented drilling operations. Weaken
    `FULL_SWEEP_FRACTION` and this goes red at exactly six phantoms —
    which the committed `filleted_block.step` cannot do, because the STEP
    round-trip normalises the orientation away before the miner sees it.
    """
    from OCP.BRepAdaptor import BRepAdaptor_Surface
    from OCP.BRepFilletAPI import BRepFilletAPI_MakeFillet
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox
    from OCP.GeomAbs import GeomAbs_SurfaceType
    from OCP.gp import gp_Pnt
    from OCP.TopAbs import TopAbs_EDGE, TopAbs_FACE, TopAbs_REVERSED
    from OCP.TopExp import TopExp_Explorer
    from OCP.TopoDS import TopoDS

    from aberp_cad_extract.holes import mine_cylindrical_holes as mine

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 30.0, 20.0).Shape()
    maker = BRepFilletAPI_MakeFillet(block)
    seen = []
    explorer = TopExp_Explorer(block, TopAbs_EDGE)
    while explorer.More():
        edge = TopoDS.Edge_s(explorer.Current())
        explorer.Next()
        if not any(edge.IsSame(other) for other in seen):
            seen.append(edge)
            maker.Add(5.0, edge)
    shape = maker.Shape()

    reversed_cylinders = 0
    explorer = TopExp_Explorer(shape, TopAbs_FACE)
    while explorer.More():
        face = TopoDS.Face_s(explorer.Current())
        explorer.Next()
        if BRepAdaptor_Surface(face).GetType() != GeomAbs_SurfaceType.GeomAbs_Cylinder:
            continue
        if face.Orientation() == TopAbs_REVERSED:
            reversed_cylinders += 1

    # Guard the guard: if OCCT ever stops handing these back REVERSED this
    # probe silently stops testing anything, so say so out loud instead.
    assert reversed_cylinders == 6, (
        "this probe only means something while OCCT reports some edge "
        f"fillets as REVERSED; it reported {reversed_cylinders}, so the "
        "phantom-hole path it exists to cover is no longer reachable here"
    )

    holes = mine(shape)
    assert holes == [], (
        "edge fillets are not holes; got "
        f"{[(h.diameter_mm, h.entry_point_mm) for h in holes]}"
    )


def test_b5_concave_fillet_is_not_a_hole(fixtures_dir: Path):
    """B5: a fillet is not a bore. The concave arm — the harder one.

    A fillet in an internal corner has its axis in AIR and its material
    OUTSIDE the cylinder, which is exactly what a bore looks like to both
    of `_is_bore_face`'s tests. Only the circumferential sweep separates
    them: 90°, not 360°. So this fixture is the one that keeps the
    post-merge sweep union load-bearing, and it goes red the moment that
    union is weakened to accept a partial sweep.
    """
    holes = _mine(fixtures_dir / "concave_fillet_step.step")

    assert holes == [], (
        "an internal-corner fillet is not a hole; got "
        f"{[(h.diameter_mm, h.entry_point_mm) for h in holes]}"
    )


# ── ADR-0112 adversarial round 2 — N1 and correction 1 ───────────────────
#
# Round 1's B4 fix asked the CAP FACE where a bore really ends, which is
# right, and then only implemented it for PLANAR caps. Every fixture it
# committed had planar caps, so the suite could not see the half that was
# missing. These put a curved cap on each shape that matters. Every one
# of them was run against the round-1 miner and reported the number in
# its own failure message before the fix landed.


def test_n1_cross_drilled_shaft_measures_to_the_round_od(fixtures_dir: Path):
    """N1: a bore exiting through a CYLINDRICAL surface, measured right.

    The B4 defect, alive on a curved cap. A Ø8 cross-hole 10 mm off the
    centreline of a Ø30 bar has no planar face anywhere near it, so
    round 1's cap walk found nothing and the parametric bound stood: the
    trim curve on the OD reaches x = ±13.7477 while the bore's axis
    leaves the material at ±11.1803. Reported 27.4955 deep against a true
    22.3607 — 22.96 % of drilling nobody does — and put the entry
    2.57 mm outside the bar, a coordinate you cannot post to a machine.

    Both numbers are stated by construction, not measured: the axis runs
    at y=10 through a bar of radius 15, so it crosses the OD at
    x = ±sqrt(15² - 10²).
    """
    holes = _mine(fixtures_dir / "cross_drilled_shaft.step")

    assert len(holes) == 1, (
        "the Ø30 bar OD is not a hole and the cross-bore is; "
        f"got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    _approx(hole.depth_mm, 2.0 * math.sqrt(125.0))
    assert hole.depth_mm == pytest.approx(2.0 * math.sqrt(125.0), abs=TOL), (
        "depth must stop at the bar's surface, not at the exit curve's "
        f"extreme; got {hole.depth_mm} (round 1 reported 27.495454170)"
    )
    _approx_vec(hole.entry_point_mm, (-math.sqrt(125.0), 10.0, 30.0))
    # …and the entry is ON the bar. Pinned separately because that is the
    # whole point: round 1 put it at x=-13.7477, in mid-air.
    entry = hole.entry_point_mm
    assert math.hypot(entry[0], entry[1]) == pytest.approx(15.0, abs=TOL), (
        "the entry point must lie on the Ø30 outer surface; it is "
        f"{math.hypot(entry[0], entry[1])} from the bar axis, not 15.0"
    )
    _approx_vec(hole.axis_unit, (1.0, 0.0, 0.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_n1_blind_hole_under_a_curved_top(fixtures_dir: Path):
    """N1's blind arm: entry on a barrelled surface, depth measured from it.

    A Ø10 flat-bottomed bore dropped into an R25 D-section bar at x=10.
    The top surface there is at z = sqrt(25² - 10²); the bore's trim curve
    on it reaches z = sqrt(25² - 5²), so round 1 ran 1.58 mm long with the
    entry that far above the material.

    This also pins that the curved-cap generalisation did not cost the
    BLIND classification: the flat bottom is still a bottom and the entry
    is still the open end.
    """
    holes = _mine(fixtures_dir / "blind_hole_curved_top.step")

    assert len(holes) == 1, (
        f"the R25 barrelled top is not a hole; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    top_z = math.sqrt(525.0)
    _approx(hole.diameter_mm, 10.0)
    _approx(hole.depth_mm, top_z - 5.0)
    _approx_vec(hole.entry_point_mm, (10.0, 20.0, top_z))
    assert hole.entry_point_mm[2] == pytest.approx(top_z, abs=TOL), (
        "the entry must sit ON the curved top; round 1 put it at "
        f"z={math.sqrt(600.0)}, 1.58 mm above the material"
    )
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))
    assert hole.end_condition is HoleEndCondition.BLIND
    assert hole.flat_bottom is True


def test_n1_bore_breaking_out_through_a_fillet(fixtures_dir: Path):
    """N1's fillet arm — and correction 1's positive control.

    A Ø6 through-bore whose top end breaks out inside an R10 edge fillet.
    Two claims at once, and they pull in opposite directions:

    - the fillet is a legitimate CAP, so the depth is measured to where
      the axis leaves it (z = 20 + sqrt(75)), not to the trim curve's
      extreme, which is what round 1 reported;
    - and the bore itself must SURVIVE. A guard that rejects fillets by
      rejecting everything near one would pass the two hole-free fillet
      fixtures and silently drop this hole.
    """
    holes = _mine(fixtures_dir / "bore_into_fillet.step")

    assert len(holes) == 1, (
        "a bore that breaks out through a fillet is still a hole, and the "
        f"fillet is still not one; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 6.0)
    _approx(hole.depth_mm, 20.0 + math.sqrt(75.0))
    _approx_vec(hole.entry_point_mm, (55.0, 30.0, 0.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_c1_a_real_bore_beside_a_concave_fillet_survives(fixtures_dir: Path):
    """Correction 1: the sweep union carries the fillet defence alone now.

    Round 1's `_is_bore_face` had a second arm requiring the bore's own
    axis to be in void, sold as redundant cover against fillets. It is
    gone (see `test_c1_a_bore_over_a_centre_post_is_not_dropped` for what
    it actually did), which leaves the post-merge sweep union as the only
    thing separating an internal-corner fillet from a bore.

    So ask both halves of that on ONE part: an R5 concave fillet 1 mm
    below a genuine Ø8 through-bore. Exactly one hole — the fillet
    contributing none, the bore contributing one. Weakening
    `FULL_SWEEP_FRACTION` reds this on the phantom side; dropping the
    bore reds it on the other.
    """
    holes = _mine(fixtures_dir / "bore_beside_concave_fillet.step")

    assert len(holes) == 1, (
        "one bore, no phantom from the concave fillet beside it; got "
        f"{[(h.diameter_mm, h.entry_point_mm) for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    _approx(hole.depth_mm, 30.0)
    _approx_vec(hole.entry_point_mm, (0.0, 20.0, 25.0))
    _approx_vec(hole.axis_unit, (1.0, 0.0, 0.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_c1_a_bore_over_a_centre_post_is_not_dropped(fixtures_dir: Path):
    """Correction 1's revert-proof: the removed arm was a FALSE NEGATIVE.

    Round 1 shipped an axis-in-the-void arm in `_is_bore_face` and
    documented it as unpinnable by any valid STEP file, on the argument
    that a well-formed solid cannot present a cylindrical face that is
    REVERSED, sweeps a full 2π, and has material on its own axis.

    This part is a box, a cut and a fuse, and it does exactly that: a Ø30
    recess 20 mm deep with a Ø10 boss standing on its floor up the
    centreline. Ordinary geometry — a bored pocket around a raised spigot.
    What the arm does to it is not reject a phantom, it DROPS THE RECESS:
    zero holes reported on a part with one, an under-count, on the side
    nobody sees. Restoring the arm reds this test at `len(holes) == 0`.

    The Ø10 post's own OD must stay out of the answer, which is the
    orientation arm still doing its job on the same part.
    """
    holes = _mine(fixtures_dir / "bore_over_centre_post.step")

    assert len(holes) == 1, (
        "a Ø30 recess with a boss up its axis is one hole; got "
        f"{[h.diameter_mm for h in holes]} (restoring the axis-in-void "
        "arm reports none at all)"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 30.0)
    _approx(hole.depth_mm, 20.0)
    _approx_vec(hole.entry_point_mm, (30.0, 30.0, 40.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))
    assert hole.end_condition is HoleEndCondition.BLIND
    assert hole.flat_bottom is True


def test_n2_end_conditions_come_from_the_cap_faces_not_a_probe(fixtures_dir: Path):
    """N2: the whole through/blind vocabulary, off the cap walk alone.

    Round 1 answered "is this end open?" with 36 point-in-solid queries
    per bore — a ring of eight samples near the bore wall, at two depths,
    at both ends. That worked and it cost most of the subprocess budget
    (a 600-hole plate took 29.88 s of the wrapper's 30 s, re-measured
    against the round-1 module on this box), and the ring count was a
    free parameter no fixture pinned.

    It is now one dot product against the cap face's outward normal, and
    `BRepClass3d_SolidClassifier` is gone from the module entirely. This
    sweeps the shapes that separate the answers — a flat through-exit, a
    flat blind bottom, a conical drill point, and a curved breakout in
    both the through and the blind direction — and asserts every one
    still lands where B2 and the round-1 fixtures put it. Inverting the
    sense of `_cap_says_open` reds all five; making it always answer
    "open" reds the three blind ones.
    """
    expected = {
        "plate_4_through_holes.step": HoleEndCondition.THROUGH,
        "blind_hole_flat_bottom.step": HoleEndCondition.BLIND,
        "blind_hole_drill_point.step": HoleEndCondition.BLIND,
        "cross_drilled_shaft.step": HoleEndCondition.THROUGH,
        "blind_hole_curved_top.step": HoleEndCondition.BLIND,
    }
    for name, want in expected.items():
        for hole in _mine(fixtures_dir / name):
            assert hole.end_condition is want, (
                f"{name}: expected every hole {want}, got {hole.end_condition}"
            )

    import aberp_cad_extract.holes as holes_mod

    assert not hasattr(holes_mod, "_end_is_open"), (
        "the ring probe must be GONE, not merely unused — a dormant "
        "second answer to the same question is how the two drift apart"
    )
    assert not hasattr(holes_mod, "_axis_point_is_material"), (
        "no point-in-solid query survives in the miner"
    )


#: The committed parts on which `_root_for_end` builds an `_AxisMaterial`
#: classifier at all — exactly the parts that have a crossing the void
#: bound discards, which is the only place the material question is put.
#: Everything else (an ordinary plate, a flat-bottomed pocket, a
#: countersink, a chamfered mouth, a dome) walks its caps without one.
CLASSIFIER_PARTS = [
    "angled_far_opening_through_bore",
    "ball_nose_blind_bore",
    "ball_nose_blind_bore_d4_deep",
    "ball_nose_blind_bore_d6",
    "blind_bore_straddling_a_rounded_edge",
    "bore_into_fillet",
    "bore_straddling_a_rounded_edge",
    "bore_through_a_domed_shoulder",
    "bore_through_torus_wall",
    "cross_drilled_shaft",
    "domed_floor_pocket",
    "domed_floor_pocket_proud",
    "domed_floor_pocket_with_a_rib",
    "far_opening_through_bore",
    "far_opening_through_bore_turned",
    "far_opening_through_bore_with_a_leg",
    "nurbs_far_opening_through_bore",
    "spherical_mouth_undercut_bore",
    "spherical_mouth_undercut_bore_with_a_boss",
    "undercut_ball_seat_at_the_confusion_edge",
    "undercut_ball_seat_below_the_confusion",
    "undercut_ball_seat_blind_bore",
    "undercut_ball_seat_blind_bore_d8",
]


def test_n2_the_point_classifier_is_rationed_not_readmitted(fixtures_dir: Path):
    """N2: the classifier is back for ONE question, and pays for itself.

    Round 1 answered "is this end open?" with a ring of THIRTY-SIX
    point-in-solid queries PER BORE. N2 removed them — the cap face's own
    outward normal answers that for free — and pinned the removal as an
    import-level ban, on the reasoning that "we only call it twice now"
    decays and that anyone reaching for a point classifier again should
    have to justify bringing the dependency back rather than quietly
    extending an existing one.

    D-19 round 2 is that justification, and the ban is replaced by the
    contract it was standing in for rather than deleted. The question is
    different — "is there metal beyond this crossing, along the axis?",
    which no normal can answer, because a normal is local and this is
    not — and so is the cost. What is pinned now:

    - **openness is still not a classifier's business.** The ring probe
      and its helper stay gone, so there is exactly one answer to "is
      this end open?" and it is still the outward normal.
    - **an ordinary part never builds one.** The Ø8 four-hole plate is
      the part N2 was measured on; all four of its bores go through the
      cap walk without a single classifier being constructed, because
      the void bound never has to discard anything. So does every
      committed part but the handful in :data:`CLASSIFIER_PARTS`.
    - **at most one per bore, built lazily.** Not one per root, not one
      per cap face, and none at all for a bore that never asks.
    - **the queries are bounded by the crossings in doubt.** The
      worst committed part spends 19, against round 1's 36 on EVERY
      bore of every part; the ball nose and the undercut seats spend 4.

    D-19 round 4 split ASKED from SPENT here, as it did for the extent
    question. The cap walk reaches the same faces once per mouth EDGE, so
    on a bore whose mouth an exporter has split into many edges the same
    crossing is put to the oracle over and over —
    ``nurbs_far_opening_through_bore`` asks 488 times about two
    crossings, which is thirteen times round 1's whole-bore budget on a
    part with one hole in it. `_AxisMaterial._inside` memoises on ``t``,
    which for one bore it may: the answer is a pure function of ``t``. So
    488 questions cost 19 classifier queries, and it is the QUERIES that
    are the budget. Both are pinned, because the gap between them is the
    memo.

    Measured, not asserted, so the numbers cannot rot into prose.
    """
    import aberp_cad_extract.holes as holes_mod

    assert not hasattr(holes_mod, "_end_is_open"), (
        "the ring probe must stay gone; openness is the outward normal's"
    )
    assert not hasattr(holes_mod, "_axis_point_is_material"), (
        "no second point-in-solid path may grow beside `_AxisMaterial`"
    )

    built = []
    made = []
    original = holes_mod._AxisMaterial._inside
    original_init = holes_mod._AxisMaterial.__init__

    def counting(self, t):
        built.append(self)
        return original(self, t)

    def watching(self, shape, origin, direction):
        original_init(self, shape, origin, direction)
        made.append(self)

    def census(name):
        built.clear()
        made.clear()
        holes = _mine(fixtures_dir / name)
        # asked, oracles, and the classifier queries actually SPENT.
        return (
            holes,
            len(built),
            len({id(o) for o in built}),
            sum(oracle._queries for oracle in made),
        )

    holes_mod._AxisMaterial._inside = counting
    holes_mod._AxisMaterial.__init__ = watching
    try:
        plate, plate_queries, plate_oracles, _ = census("plate_4_through_holes.step")
        _seat, seat_queries, seat_oracles, seat_spent = census(
            "undercut_ball_seat_blind_bore.step"
        )
        corpus = {
            path.stem: census(path.name)[1:]
            for path in sorted(fixtures_dir.glob("*.step"))
        }
    finally:
        holes_mod._AxisMaterial._inside = original
        holes_mod._AxisMaterial.__init__ = original_init

    assert len(plate) == 4
    assert plate_queries == 0 and plate_oracles == 0, (
        "an ordinary plate must not touch a point classifier at all; it "
        f"spent {plate_queries} queries across {plate_oracles} oracles"
    )
    assert seat_oracles == 1, (
        f"one bore must build at most one oracle; the seat built {seat_oracles}"
    )
    assert seat_queries == seat_spent == 4, (
        f"the seat's two crossings are two probes each; got {seat_queries} "
        f"asked and {seat_spent} spent"
    )
    worst_part, (_asked, _oracles, worst) = max(
        corpus.items(), key=lambda kv: kv[1][2]
    )
    assert worst < 36, (
        f"the worst committed part ({worst_part}) spends {worst} queries — "
        "round 1 spent 36 on EVERY bore, and that budget is the whole reason "
        "N2 removed them"
    )
    assert corpus["nurbs_far_opening_through_bore"][0] == 480, (
        "the part whose mouth is split into many edges must still ASK many "
        f"times, or the memo below is not being exercised; got "
        f"{corpus['nurbs_far_opening_through_bore'][0]}. (D-19 round 5 moved "
        "this from 488: the intersection now runs in the BORE'S FRAME, so a "
        "handful of this part's B-spline roots land on the pole precisely "
        "enough to be refused as the degeneracies they are, and are never "
        "probed. No committed answer moves with it — the whole corpus is "
        "pinned field by field in `test_d19r5_the_corpus_moves_by_at_most_a_"
        "last_bit`.)"
    )
    assert corpus["nurbs_far_opening_through_bore"][2] == 10, (
        "...and must still SPEND a handful of classifier queries on it. 480 "
        "would mean `_inside`'s memo is gone; a different small number means "
        "the roots it is asked about have moved. (Round 5 moved this from 19 "
        "for the same reason it moved the 488: nine of this part's B-spline "
        "roots are degeneracies that only the in-frame intersection places "
        "accurately enough to recognise, and a refused root is never probed. "
        "The part's answer does not move.)"
    )
    loud = sorted(name for name, (_q, oracles, _s) in corpus.items() if oracles)
    assert loud == CLASSIFIER_PARTS, (
        "the set of parts that build a classifier is a property of the "
        "rule, not a budget — every one of them has a crossing the void "
        f"bound discards. Expected {CLASSIFIER_PARTS}, got {loud}"
    )


def test_n2_surface_adaptors_never_compute_a_uv_restriction(fixtures_dir: Path):
    """N2's quadratic term, pinned on the property rather than the clock.

    `BRepAdaptor_Surface(face)` defaults to `Restriction=True`, which
    calls `BRepTools::UVBounds` and walks EVERY EDGE of the face. On the
    parts that matter that is quadratic: a plate's top face carries one
    wire per hole, and the miner touches that face several times per
    bore. Measured at 3.6 ms per construction on a 2000-hole plate
    against 0.5 µs unrestricted — 29 s of a 39 s subprocess run.

    Nothing in the miner reads those bounds; the one place that wants the
    trimmed extent asks `BRepTools.UVBounds_s` directly. So the invariant
    is simply that every adaptor is built unrestricted, and it is pinned
    two ways — the helper really is unrestricted, and no call site
    bypasses it. A wall-clock assertion would be flaky and would not say
    which of those two broke.
    """
    import ast

    import aberp_cad_extract.holes as holes_mod

    constructions = [
        node
        for node in ast.walk(_miner_ast())
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "BRepAdaptor_Surface"
    ]
    assert len(constructions) == 1, (
        "every surface adaptor must go through `_adaptor`; found "
        f"{len(constructions)} construction sites, so at least one bypasses "
        "it and reintroduces the quadratic UV-bounds walk"
    )
    (only,) = constructions
    assert len(only.args) == 2 and only.args[1].value is False, (
        "`_adaptor` must pass Restriction=False explicitly; the OCCT "
        "default is True and that is the quadratic path"
    )

    with _silence_stdout_fd():
        shape = _load_step_shape(str(fixtures_dir / "plate_4_through_holes.step"))
        faces = holes_mod._collect_faces(shape)
        adaptors = [holes_mod._adaptor(f) for f in faces]
        first_u = [a.FirstUParameter() for a in adaptors]

    assert any(u < -1e50 for u in first_u), (
        "an unrestricted adaptor reports an infinite parameter range; "
        f"got {first_u}, which means the restriction is being computed"
    )


# ── ADR-0112 adversarial round 3 — blockers 1 and 2 ──────────────────────
#
# Round 2 generalised the cap walk from PLANAR caps to any cap. Two
# families were still wrong, and each is here with the number round 2
# actually reported in its own failure message. Every one of these was run
# against the round-2 miner and confirmed to fail before the fix landed.
#
# The two share a root cause worth naming, because it is what makes them
# one fix and not two. Round 2 decided which intersections counted by
# WHERE THEY FELL — a root had to lie inside the bore's parametric span.
# That test stood in for two different questions it could not actually
# answer: "is this a cap at all?" (a drill point's apex is not) and "is
# this cap this end's?" It got the first right only by luck of position,
# and the luck ran out in both directions at once. A coaxial cone at the
# MOUTH of a bore puts its apex INSIDE the span, where the test admitted
# it — blocker 1. A doubly-curved convex cap puts its crown OUTSIDE the
# span, where the test refused it — blocker 2. Asking the geometry
# instead — does the axis CROSS this surface here? — answers the first
# question properly and lets the second bound relax.


def test_r3_countersunk_through_bore_measures_the_bore_not_the_apex(
    fixtures_dir: Path,
):
    """Blocker 1: a countersunk mouth must not shorten the hole.

    A Ø8 through-bore in a 20 mm plate, countersunk 90° included at the
    top. Full diameter runs z=0..17 and the countersink opens out above
    it. The cone is COAXIAL with the bore, so the bore's axis meets its
    surface at exactly one point — the APEX, at z=13, four millimetres
    INSIDE the hole. Round 2 took that for the end of the bore and
    reported 17 mm of drilling as 13.

    Which is the same cone, in the same place, that a 118° drill point
    puts at the other end of a hole — and round 2 got THAT right, because
    a drill point's apex falls outside the bore rather than inside it.
    Position was doing work that geometry should have been doing.
    """
    holes = _mine(fixtures_dir / "countersunk_through_bore.step")

    assert len(holes) == 1, (
        "a countersunk bore is one hole and the countersink is not a "
        f"second one; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(17.0, abs=TOL), (
        "depth must run to the end of the full-diameter bore, not to the "
        f"countersink cone's apex; got {hole.depth_mm} (round 2 reported "
        "13.0 — 23.5 % of the hole missing)"
    )
    _approx_vec(hole.entry_point_mm, (20.0, 20.0, 0.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r3_countersink_angle_does_not_decide_the_end_condition(
    fixtures_dir: Path,
):
    """Blocker 1's classification arm: 120° included, same answer shape.

    Round 2 did not merely mis-measure a countersunk hole, it
    mis-CLASSIFIED it, and inconsistently: the 90° fixture above came back
    THROUGH and this one BLIND. Both are through-holes. The end condition
    is read off the cap's outward normal, and a cone's apex HAS no normal
    — approach it along a different generatrix and you get a different
    vector — so what OCCT returned there was whichever generatrix its
    intersector happened to land on. The verdict turned on solver
    internals, which is a determinism defect wearing a correctness
    defect's clothes.

    Depth is stated by construction: the full-diameter bore ends where a
    60°-half-angle cone reaching Ø14 at the top face starts.
    """
    holes = _mine(fixtures_dir / "countersunk_bore_120.step")

    assert len(holes) == 1, (
        f"one hole, countersunk; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    expected_depth = 20.0 - 3.0 / math.tan(math.radians(60.0))
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(expected_depth, abs=TOL), (
        f"got {hole.depth_mm}; round 2 reported 15.958548, the apex"
    )
    _approx_vec(hole.entry_point_mm, (20.0, 20.0, 0.0))
    assert hole.end_condition is HoleEndCondition.THROUGH, (
        "a countersunk through-hole is THROUGH at every countersink "
        "angle; round 2 called this one BLIND while calling the 90° twin "
        f"THROUGH; got {hole.end_condition}"
    )


def test_r3_chamfered_mouth_bore_keeps_its_full_depth(fixtures_dir: Path):
    """Blocker 1 without the word "countersink".

    The same defect on the commonest feature in any shop: a bore with a
    plain 45° chamfer broken at its mouth. Nothing about the failure
    needed a countersink — anything CONICAL and coaxial at the mouth
    triggered it. Full diameter runs z=0..18.5; the apex sits at 14.5.
    """
    holes = _mine(fixtures_dir / "chamfered_mouth_bore.step")

    assert len(holes) == 1, (
        f"a chamfer is not a hole; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(18.5, abs=TOL), (
        f"got {hole.depth_mm} (round 2 reported 14.5, the apex)"
    )
    _approx_vec(hole.entry_point_mm, (20.0, 20.0, 0.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r3_countersunk_blind_bore_enters_at_the_mouth(fixtures_dir: Path):
    """Blocker 1 on a blind hole, where it also moves the ENTRY POINT.

    A Ø8 flat-bottomed bore from z=6 to z=17 under a 90° countersink.
    Round 2 called it 7.0 deep and put the entry at z=13 — the cone's
    apex, a point four millimetres inside solid metal. A wrong depth is a
    wrong price; a wrong entry is a coordinate somebody posts to a
    machine.

    Also pins that the fix did not cost the BLIND classification or the
    flat-bottom detection: the countersink is at the OPEN end, and the
    open end is still where the hole is entered.
    """
    holes = _mine(fixtures_dir / "countersunk_blind_bore.step")

    assert len(holes) == 1, (
        f"one blind hole, countersunk; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(11.0, abs=TOL), (
        f"got {hole.depth_mm} (round 2 reported 7.0)"
    )
    assert hole.entry_point_mm[2] == pytest.approx(17.0, abs=TOL), (
        "the entry must be at the bore's mouth, not at the countersink "
        f"cone's apex; got z={hole.entry_point_mm[2]} (round 2 put it at "
        "13.0, inside the material)"
    )
    _approx_vec(hole.entry_point_mm, (20.0, 20.0, 17.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))
    assert hole.end_condition is HoleEndCondition.BLIND
    assert hole.flat_bottom is True


def test_r3_a_plain_bore_is_unchanged_to_the_bit():
    """Blocker 1's control: no cone, no change, exactly.

    The same 40 x 40 x 20 block and the same Ø8 through-bore as the
    countersink fixtures, with the countersink NOT cut. Its depth is
    20.0 and its entry (20,20,0), and this asserts EQUALITY rather than
    approximate equality: the crossing test added for blocker 1 must not
    move a plain bore's answer by a last bit, and `==` is the only way to
    say that.

    In memory rather than as a fixture because the point is that this
    part is the countersink fixtures minus one cut, which a committed
    STEP file would state less clearly than the four lines below.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox, BRepPrimAPI_MakeCylinder
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    bore = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(20.0, 20.0, -5.0), gp_Dir(0, 0, 1)), 4.0, 30.0
    ).Shape()
    holes = mine_cylindrical_holes(BRepAlgoAPI_Cut(block, bore).Shape())

    assert len(holes) == 1
    hole = holes[0]
    assert hole.depth_mm == 20.0, (
        f"a plain bore's depth must be exactly 20.0, not {hole.depth_mm!r}"
    )
    assert hole.entry_point_mm == [20.0, 20.0, 0.0]
    assert hole.diameter_mm == 8.0
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r3_bore_through_a_spherical_dome_reaches_the_crown(fixtures_dir: Path):
    """Blocker 2: a doubly-curved CONVEX cap, at true normal incidence.

    A Ø8 bore straight through the centre of a Ø40 ball. The axis meets
    the sphere square on at both ends, which is the worst case rather
    than a special one: normal incidence is exactly where a dome's trim
    curve falls furthest short of the crown the axis leaves through. The
    trim sits at z = ±sqrt(20² - 4²) = ±19.5959 and the material ends at
    ±20, so the truth lay 0.4 mm OUTSIDE the parametric span round 2
    required roots to be inside, and was discarded at BOTH ends.

    Both numbers are stated by construction, not measured.
    """
    holes = _mine(fixtures_dir / "bore_through_spherical_dome.step")

    assert len(holes) == 1, (
        f"the ball's own surface is not a hole; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(40.0, abs=TOL), (
        "depth must run crown to crown; got "
        f"{hole.depth_mm} (round 2 reported {2.0 * math.sqrt(384.0)})"
    )
    _approx_vec(hole.entry_point_mm, (0.0, 0.0, -20.0))
    # …and the entry is ON the sphere. Pinned separately because that is
    # the whole point: round 2 put it 0.4 mm inside solid metal.
    entry = hole.entry_point_mm
    assert math.sqrt(sum(c * c for c in entry)) == pytest.approx(20.0, abs=TOL), (
        "the entry must lie on the Ø40 surface; it is "
        f"{math.sqrt(sum(c * c for c in entry))} from the centre, not 20.0"
    )
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r3_blind_bore_under_a_dome_enters_on_the_crown(fixtures_dir: Path):
    """Blocker 2's blind arm — where the error is a coordinate.

    A Ø10 flat-bottomed bore dropped into the same Ø40 ball from the
    crown, bottoming at z=8. Round 2 reported 11.3649 deep entering at
    z=19.3649, a point inside the ball.
    """
    holes = _mine(fixtures_dir / "blind_bore_under_dome.step")

    assert len(holes) == 1, (
        f"one blind hole under a dome; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 10.0)
    assert hole.depth_mm == pytest.approx(12.0, abs=TOL), (
        "got "
        f"{hole.depth_mm} (round 2 reported {math.sqrt(375.0) - 8.0}, "
        "measuring from the trim curve instead of the crown)"
    )
    assert hole.entry_point_mm[2] == pytest.approx(20.0, abs=TOL), (
        "the entry must sit ON the crown; round 2 put it at 19.364917, "
        f"inside the ball; got z={hole.entry_point_mm[2]}"
    )
    _approx_vec(hole.entry_point_mm, (0.0, 0.0, 20.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))
    assert hole.end_condition is HoleEndCondition.BLIND
    assert hole.flat_bottom is True


def test_r3_bore_through_a_torus_wall_is_not_dropped(fixtures_dir: Path):
    """Blocker 2 at its worst: the hole DISAPPEARED.

    A Ø4 bore radially outward through the wall of a torus of major
    radius 12 and minor radius 8, so the wall runs from x=4 to x=20 along
    the bore's axis. Both caps are toroidal — in through the concave
    inner wall, out through the convex outer one — and the depth is 16.

    What round 2 did here is the reason this arm exists. It clipped the
    convex crown at x=20 for lying outside the parametric span, which
    left the far end with no root of its own; the nearest surviving root
    was the CONCAVE cap's, at x=4, the near end's. Both ends resolved to
    x=4, the bore measured zero deep, and a zero-deep bore is dropped.
    The part came back with NO HOLES AT ALL — an under-count, and a
    silent one, which is the direction nobody sees.
    """
    holes = _mine(fixtures_dir / "bore_through_torus_wall.step")

    assert len(holes) == 1, (
        "a bore through a torus wall is a hole and the torus is not; got "
        f"{[h.diameter_mm for h in holes]} (round 2 reported none at all)"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 4.0)
    assert hole.depth_mm == pytest.approx(16.0, abs=TOL), (
        f"outer surface at x=20, inner at x=4; got {hole.depth_mm}"
    )
    _approx_vec(hole.entry_point_mm, (4.0, 0.0, 0.0))
    _approx_vec(hole.axis_unit, (1.0, 0.0, 0.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r3_bore_through_a_nurbs_dome_reaches_the_crown(fixtures_dir: Path):
    """Blocker 2 on a B-SPLINE cap — the arm the analytic sphere cannot cover.

    The same ball and the same bore as the spherical fixture, with the
    solid run through ``BRepBuilderAPI_NurbsConvert`` first, which is the
    kind of surface an imported customer part actually carries.

    It is a separate test because a fix can pass the analytic sphere and
    still fail this. Both surfaces put a degenerate parametric POLE
    exactly where the bore's axis leaves them, and telling a smooth pole
    from a cone's apex is the whole of the blocker-1 test — but OCCT
    special-cases the analytic sphere and reports the right normal at its
    pole, while at the B-spline's pole it reports noise: (0,-0.214,0.977)
    and (-0.674,-0.026,0.738) at neighbouring parameters of a surface
    whose true normal is (0,0,1) all along that line, and raising the
    derivative order does not rescue it. A fix that trusts OCCT's normal
    at a pole passes the sphere and fails here.

    Looser than TOL on purpose, and only here: the surface is FITTED, so
    the crown is where the fit put it, not where the algebra does.
    """
    holes = _mine(fixtures_dir / "bore_through_nurbs_dome.step")

    assert len(holes) == 1, (
        f"one bore through a fitted dome; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(40.0, abs=1e-9), (
        "depth must run crown to crown on a fitted surface too; got "
        f"{hole.depth_mm} (round 2 reported 39.191836)"
    )
    assert hole.entry_point_mm[2] == pytest.approx(-20.0, abs=1e-9), (
        f"entry must be on the fitted crown; got z={hole.entry_point_mm[2]}"
    )
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r3_the_drill_point_and_the_countersink_are_one_rule(fixtures_dir: Path):
    """The two cones, side by side, decided the same way.

    A 118° drill point and a 90° countersink are the same surface — a
    cone coaxial with the bore — differing only in which end of the hole
    they sit at. Round 2 decided them by POSITION and so decided them
    differently: the point's apex falls below the bore and was discarded,
    the countersink's apex falls inside it and was taken for the cap.

    Both are now discarded for the same reason, that a cone's apex is a
    point the axis touches rather than a surface it crosses, and both
    holes therefore measure to the end of their full-diameter cylinder.
    This asserts the pair together, because the claim being pinned is
    that they are ONE rule and not two: it reds if a later change
    special-cases either.
    """
    point = _mine(fixtures_dir / "blind_hole_drill_point.step")
    countersink = _mine(fixtures_dir / "countersunk_through_bore.step")

    assert len(point) == 1 and len(countersink) == 1
    # The drill point's full-diameter run is z=15..40 (the cone tapers
    # below 15 to an apex at 15 - 4/tan(59°)); the countersink's is
    # z=0..17 (the cone opens above 17 from an apex at 13). Each is its
    # cylinder's own extent, and neither is moved by its cone.
    _approx(point[0].depth_mm, 25.0)
    _approx(countersink[0].depth_mm, 17.0)
    assert point[0].end_condition is HoleEndCondition.BLIND
    assert countersink[0].end_condition is HoleEndCondition.THROUGH



# ── ADR-0112 adversarial round 4: the foreign-root hijack ────────────────
#
# Round 3 unbounded the outward root and left the CROSS-FACE contest as
# "outermost cap wins". A face that merely neighbours the bore's mouth
# then wins the end whenever its UNBOUNDED carrier crosses the axis
# further out than the true cap. Each number below is the number the
# fixture was built from; the round-3 number it replaces is named in the
# failure message so a regression says what it regressed to.


def test_r4_bore_beside_a_chamfered_edge_stops_at_the_part(fixtures_dir: Path):
    """A Ø8 through-bore 2 mm inboard of a 45° chamfered part edge.

    The chamfer's plane is ``x + z = 54`` and the bore's axis is at
    x=32, so that plane crosses the axis at z=22 — 2 mm above a part
    that stops at z=20, and 2 mm outside the chamfer face's own x range
    of [34, 40]. The chamfer is a genuine NEIGHBOUR (the bore spans
    x = 28..36, so its mouth bites into it) and it owns 120° of that
    mouth; the flat top owns the other 240° and is where the axis
    actually leaves.
    """
    holes = _mine(fixtures_dir / "bore_beside_chamfered_edge.step")

    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(20.0, abs=TOL), (
        f"depth must be the plate's 20.0; got {hole.depth_mm} "
        "(round 3 reported 22.0, deeper than the part)"
    )
    _approx_vec(hole.entry_point_mm, (32.0, 20.0, 0.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r4_blind_bore_beside_a_chamfer_enters_on_the_part(fixtures_dir: Path):
    """The same chamfer on a BLIND bore, where the error is a coordinate.

    A blind hole carries its entry at the OPEN end, so hijacking that end
    moves the entry off the part: round 3 put it at z=22, two
    millimetres above the metal, which is a coordinate no machine can
    reach. Pinned as a position, not only as a depth.
    """
    holes = _mine(fixtures_dir / "blind_bore_beside_chamfered_edge.step")

    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(12.0, abs=TOL), (
        f"depth must be the drilled 12.0; got {hole.depth_mm} "
        "(round 3 reported 14.0)"
    )
    assert hole.entry_point_mm[2] == pytest.approx(20.0, abs=TOL), (
        "the entry must sit ON the top face at z=20; it is at "
        f"{hole.entry_point_mm[2]} (round 3 put it at 22.0, in mid-air)"
    )
    _approx_vec(hole.entry_point_mm, (32.0, 20.0, 20.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))
    assert hole.end_condition is HoleEndCondition.BLIND
    assert hole.flat_bottom is True


def test_r4_bore_beside_a_concave_corner_fillet_keeps_its_depth(fixtures_dir: Path):
    """A Ø14 through-bore beside a concave R6 corner fillet.

    The arm that rules out the cheap fix. Restricting round 3's
    relaxation to non-planar caps would have closed the chamfer and left
    this one open: the hijacker here IS non-planar. The fillet is the
    quarter-cylinder about x=24, z=26 spanning x in [24, 30]; the bore's
    axis is at x=21, outside that span, and the fillet's carrier cylinder
    still meets the axis at z = 26 - sqrt(27) = 20.8038.
    """
    holes = _mine(fixtures_dir / "bore_beside_concave_corner_fillet.step")

    assert len(holes) == 1, (
        f"the R6 fillet is not a hole; got {[h.diameter_mm for h in holes]}"
    )
    hole = holes[0]
    _approx(hole.diameter_mm, 14.0)
    assert hole.depth_mm == pytest.approx(20.0, abs=TOL), (
        f"depth must be the plate's 20.0; got {hole.depth_mm} "
        f"(round 3 reported {26.0 - math.sqrt(27.0)})"
    )
    _approx_vec(hole.entry_point_mm, (21.0, 20.0, 0.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r4_a_chamfer_that_really_is_the_cap_still_wins(fixtures_dir: Path):
    """The positive control, without which the fix could be a blanket ban.

    The three fixtures above all rule a chamfer or a fillet OUT of the
    contest, and a miner that simply refused every chamfer would pass
    every one of them. Here the Ø8 bore at x=32 sits entirely inside a
    14 mm chamfer spanning x in [26, 40]: the whole mouth is cut in the
    chamfer, so the chamfer owns all 360° of it and genuinely IS where
    the bore leaves. Its plane is ``x + z = 46``, so the axis leaves at
    z=14 and the hole is 14 mm deep, not the plate's 20.
    """
    holes = _mine(fixtures_dir / "bore_inside_a_chamfer.step")

    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(14.0, abs=TOL), (
        f"the chamfer IS this bore's cap; got {hole.depth_mm} — a depth of "
        "20.0 would mean ownership had been refused to a face that owns "
        "the whole mouth"
    )
    _approx_vec(hole.entry_point_mm, (32.0, 20.0, 0.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r4_ownership_is_the_only_thing_holding_the_foreign_root_out(
    fixtures_dir: Path, monkeypatch
):
    """REVERT-PROOF: concede ownership and all three go red, exactly.

    The three fixtures above would still pass against a miner that fixed
    the hijack some other way — or against one that never had it. This
    disables the ONE mechanism that closes it, by making every candidate
    claim it owns the bore's mouth, which is precisely what
    "outermost cap wins" meant before round 4. Each fixture must then
    report the round-3 number, to the bit.

    So the fixtures are pinned to the mechanism and not merely to the
    outcome: delete `_mouth_owns_axis`, or stop calling it, and this test
    is the one that says which change did it.

    Round 5 put `_mouth_rims` in FRONT of the per-face test, so both have
    to be conceded to get back to "outermost cap wins". Conceding the
    rim rule alone leaves these three correct — which is the point of
    `test_r5_the_round_4_per_face_arm_still_carries_its_own_fixtures`
    below, and is why round 4's arm is still load-bearing rather than
    dead code behind a newer rule.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(holes_mod, "_mouth_rims", lambda mouths: [])
    # Round 6 bounded the fall-back by the mouth, so that guard has to
    # be conceded too before round 4's arm can carry anything off the
    # part — see `test_r6_the_mouth_bound_is_what_stops_the_fall_back`.
    monkeypatch.setattr(
        holes_mod, "_mouth_reach", lambda mouths, origin, direction, sign: None
    )
    monkeypatch.setattr(
        holes_mod, "_mouth_owns_axis", lambda edges, origin, direction: True
    )

    hijacked = _mine(fixtures_dir / "bore_beside_chamfered_edge.step")
    assert len(hijacked) == 1
    _approx(hijacked[0].depth_mm, 22.0)

    hijacked = _mine(fixtures_dir / "blind_bore_beside_chamfered_edge.step")
    assert len(hijacked) == 1
    _approx(hijacked[0].depth_mm, 14.0)
    _approx_vec(hijacked[0].entry_point_mm, (32.0, 20.0, 22.0))

    hijacked = _mine(fixtures_dir / "bore_beside_concave_corner_fillet.step")
    assert len(hijacked) == 1
    _approx(hijacked[0].depth_mm, 26.0 - math.sqrt(27.0))


def _winning_caps(monkeypatch, path: Path):
    """Mine `path`, returning the (face, point) of every cap that WON an end.

    `_cap_says_open` is called once per winning cap and on nothing else —
    `_EndEvidence.resolve` reads it only after the contest is settled —
    so spying on it is how a test gets hold of the face the miner
    actually measured to, without reaching into the walk.
    """
    import aberp_cad_extract.holes as holes_mod

    seen = []
    original = holes_mod._cap_says_open

    def spy(face, point, outward, normal=None):
        seen.append((face, point))
        return original(face, point, outward, normal)

    monkeypatch.setattr(holes_mod, "_cap_says_open", spy)
    _mine(path)
    return seen


def test_r5_one_closed_rim_of_one_face_carries_every_committed_cap(
    fixtures_dir: Path, monkeypatch
):
    """The ownership rule reaches the older fixtures by its EXACT arm.

    Round 4 asked this of `_mouth_owns_axis` and its closed-mouth arm;
    round 5 decides ownership on RIMS first, so the same claim is now
    made of `_mouth_rims` — and it is the claim the whole round rests
    on. Every capped end of every pre-round-4 fixture resolves through
    exactly ONE closed rim contributed by exactly ONE face, which is why
    none of their numbers moved by a bit. The corner parts are the only
    committed ones whose rim spans several faces, and the slot-crossed
    bore the only one with two rims at an end.

    Worth pinning because a rim that fails to close drops the end back to
    the round-4 contest, and an end that reaches neither drops to the
    parametric bound. If a future change starts routing an ordinary
    through-hole down either fall-back, that hole's depth is one edge
    case away from being a guess, and nothing else here would say so.
    """
    import aberp_cad_extract.holes as holes_mod

    rims = []
    original = holes_mod._mouth_rims

    def spy(mouths):
        found = original(mouths)
        rims.append((len(mouths), [len(keys) for keys, _edges in found]))
        return found

    monkeypatch.setattr(holes_mod, "_mouth_rims", spy)

    for name in (
        "plate_4_through_holes",
        "bore_through_spherical_dome",
        "bore_through_nurbs_dome",
        "bore_through_torus_wall",
        "seam_split_bore",
        "blind_hole_drill_point",
        "stepped_bore",
        "bore_over_centre_post",
        "cross_drilled_shaft",
    ):
        rims.clear()
        _mine(fixtures_dir / f"{name}.step")
        assert rims, f"{name}: the rim decomposition was never consulted"
        assert all(shape == (1, [1]) for shape in rims), (
            f"{name}: an end no longer resolves through a single closed rim "
            f"of a single face — {rims} — so its depth now rests on a "
            "fall-back rather than on topology alone"
        )


def test_r4_an_on_face_trim_test_would_have_re_broken_the_domes(
    fixtures_dir: Path, monkeypatch
):
    """Why ownership is asked of the MOUTH and not of the crossing point.

    The obvious reading of "bound the root by the cap face's own trim" is
    to project the axis crossing onto the face and ask whether it lands
    ON it. That answers NO for every genuine curved cap the miner has,
    because the crossing lands in the middle of the hole the bore itself
    cut — and on the domes it falls outside the face's UV bounds as well.

    Measured here rather than asserted, so the design note in
    `_mouth_owns_axis` is a fact: a UV-in-bounds or a BRepClass on-face
    gate would have reopened round 3's blocker 2 at both ends of the
    spherical dome, the NURBS dome and the torus wall.
    """
    from OCP.BRep import BRep_Tool
    from OCP.BRepTopAdaptor import BRepTopAdaptor_FClass2d
    from OCP.GeomAPI import GeomAPI_ProjectPointOnSurf
    from OCP.gp import gp_Pnt, gp_Pnt2d
    from OCP.ShapeAnalysis import ShapeAnalysis
    from OCP.TopAbs import TopAbs_State

    for name, curved in (
        ("bore_through_spherical_dome", True),
        ("bore_through_nurbs_dome", True),
        ("bore_through_torus_wall", False),
    ):
        checked = 0
        for face, point in _winning_caps(monkeypatch, fixtures_dir / f"{name}.step"):
            surface = BRep_Tool.Surface_s(face)
            projector = GeomAPI_ProjectPointOnSurf(gp_Pnt(*point), surface)
            assert projector.IsDone() and projector.NbPoints() >= 1
            u, v = projector.LowerDistanceParameters()

            state = BRepTopAdaptor_FClass2d(face, 1e-7).Perform(gp_Pnt2d(u, v))
            assert state == TopAbs_State.TopAbs_OUT, (
                f"{name}: the winning cap's crossing classifies {state}. If "
                "OCCT now reports it ON the face, an on-face gate has become "
                "available and the mouth test's design note needs revisiting"
            )

            if curved:
                _u_lo, _u_hi, v_lo, v_hi = ShapeAnalysis.GetFaceUVBounds_s(face)
                assert not v_lo <= v <= v_hi, (
                    f"{name}: the crown at v={v} lies inside the face's trim "
                    f"[{v_lo}, {v_hi}] — a UV-in-bounds gate would have kept it"
                )
            checked += 1
        assert checked, f"{name}: no winning cap was reached"


# ── determinism ──────────────────────────────────────────────────────────


# ---------------------------------------------------------------------------
# ADR-0112 adversarial round 5 — a bore beside a DOUBLY-chamfered corner.
#
# Round 4 made a face earn the right to end a bore by having the bore's
# MOUTH cut in it, and tested that against ONE neighbouring chamfer or
# fillet. With one neighbour the true cap keeps 240 deg of the mouth and
# clears half a turn on its own, so "does this face own more than half"
# was a sufficient test. Chamfer the ADJACENT top edge as well — an
# ordinary plate detail — and the mouth splits three ways with no face
# holding half of it. Every face abstained, and the end fell through to
# the outermost carrier surface: a chamfer plane 2-3 mm ABOVE a plate
# that stops at z=20.
#
# The fix is to stop asking faces and start asking RIMS: the mouth's
# arcs sum to the full turn however the faces divide it, so the whole
# mouth is always evidence even when no part of it is. See
# `_mouth_rims` and `_EndEvidence._rim_winner`.


def test_r5_bore_beside_a_two_chamfer_corner_stops_at_the_plate(fixtures_dir: Path):
    """Equal 6 mm chamfers on both top edges, Ø8 through-bore at (32, 32).

    Both chamfer planes are ``· + z = 54`` and both meet the axis at
    z=22, two millimetres above the metal. The mouth divides
    150 deg / 105 deg / 105 deg between the flat top and the two
    chamfers, so round 4's per-face test abstained on all three — and
    then the two chamfers, TIED at z=22, had their sectors pooled into
    one 210 deg chain that beat the real top outright. Pooling is sound
    for faces sharing one cap seam; these are distinct faces that merely
    agree on a number.
    """
    holes = _mine(fixtures_dir / "bore_beside_two_chamfers_corner.step")

    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(20.0, abs=TOL), (
        f"depth must be the plate's 20.0; got {hole.depth_mm} "
        "(round 4 reported 22.0, above the part)"
    )
    _approx_vec(hole.entry_point_mm, (32.0, 32.0, 0.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r5_uneven_corner_chamfers_do_not_carry_the_end_off_the_part(
    fixtures_dir: Path,
):
    """The same corner with 6 mm and 5 mm legs — no tie to pool.

    The chamfers cross at z=22 and z=23 and the outermost simply won.
    The mouth splits 168.59 / 115.18 / 76.23 deg, so the flat top misses
    half a turn by 11.41 deg and abstains: the escape hatch, reached by
    nothing more exotic than two different chamfers on one corner.
    """
    holes = _mine(fixtures_dir / "bore_beside_uneven_chamfer_corner.step")

    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(20.0, abs=TOL), (
        f"depth must be the plate's 20.0; got {hole.depth_mm} "
        "(round 4 reported 23.0, 3 mm above the part)"
    )
    _approx_vec(hole.entry_point_mm, (32.0, 32.0, 0.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH


def test_r5_a_blind_bore_at_a_chamfered_corner_enters_on_the_part(
    fixtures_dir: Path,
):
    """The corner's NEGATIVE control, and the one that moves a coordinate.

    A miner that answered the corner by calling every bore near one
    THROUGH would pass the two above and fail here. On a blind hole the
    OPEN end carries the entry point, so the hijack put the entry at
    z=22 — two millimetres of mid-air above the plate, a coordinate no
    machine can reach.
    """
    holes = _mine(fixtures_dir / "blind_bore_beside_two_chamfers_corner.step")

    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    assert hole.depth_mm == pytest.approx(12.0, abs=TOL), (
        f"depth must be the drilled 12.0; got {hole.depth_mm} "
        "(round 4 reported 14.0)"
    )
    assert hole.entry_point_mm[2] == pytest.approx(20.0, abs=TOL), (
        "the entry must sit ON the top face at z=20; it is at "
        f"{hole.entry_point_mm[2]} (round 4 put it at 22.0, in mid-air)"
    )
    _approx_vec(hole.entry_point_mm, (32.0, 32.0, 20.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))
    assert hole.end_condition is HoleEndCondition.BLIND
    assert hole.flat_bottom is True


def test_r5_a_bore_whose_axis_sits_on_a_chamfer_boundary_is_through(
    fixtures_dir: Path,
):
    """Equal chamfers, bore at (32, 34): the axis ON where a chamfer starts.

    y=34 is exactly where the y=40 chamfer begins, so that chamfer's
    plane meets the axis at z=20 — the SAME level as the flat top. The
    winning level is therefore a tie between two genuinely different
    faces, and every one of them has to vote the end open for it to be
    open. Pinned as a configuration this family has to get right, next
    to the two above where the chamfers cross well outside the part.

    Not a regression pin: this reads correctly on round 4 too, because
    the defect it was built for — an INDIRECT carrier plane inverting one
    face's outward normal — does not survive OCCT's STEP writer. The
    test with teeth for that is
    `test_r5_in_memory_corner_chamfer_is_where_the_indirect_plane_bites`.
    """
    holes = _mine(fixtures_dir / "bore_on_a_chamfer_corner_boundary.step")

    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    _approx(hole.depth_mm, 20.0)
    _approx_vec(hole.entry_point_mm, (32.0, 34.0, 0.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
    assert hole.end_condition is HoleEndCondition.THROUGH
    assert hole.flat_bottom is False


def test_r5_the_corner_fixtures_really_are_the_escape_hatch(
    fixtures_dir: Path, monkeypatch
):
    """Guard the guard: these parts must reach the case they were built for.

    Every corner fixture above would pass against a miner that still
    decided ends per-face, if only some face happened to own half the
    mouth. State the shape of the evidence out loud instead: at the high
    end there are THREE mouth faces, they form ONE closed rim of four
    edges, and NOT ONE of them owns the axis on its own. That triple is
    what makes round 4's rule abstain, and it is the whole reason round 5
    exists.
    """
    import aberp_cad_extract.holes as holes_mod

    seen = []
    original = holes_mod._mouth_rims

    def spy(mouths):
        found = original(mouths)
        seen.append((dict(mouths), found))
        return found

    monkeypatch.setattr(holes_mod, "_mouth_rims", spy)

    for name in (
        "bore_beside_two_chamfers_corner",
        "bore_beside_uneven_chamfer_corner",
        "bore_on_a_chamfer_corner_boundary",
        "blind_bore_beside_two_chamfers_corner",
    ):
        seen.clear()
        _mine(fixtures_dir / f"{name}.step")
        corner = [(mouths, rims) for mouths, rims in seen if len(mouths) == 3]
        assert len(corner) == 1, (
            f"{name}: expected exactly one end with three mouth faces; "
            f"got {[len(mouths) for mouths, _rims in seen]}"
        )
        mouths, rims = corner[0]
        assert [(len(keys), len(edges)) for keys, edges in rims] == [(3, 4)], (
            f"{name}: the corner mouth is no longer ONE closed rim spanning "
            f"all three faces — {[(len(k), len(e)) for k, e in rims]}"
        )
        assert not any(
            holes_mod._mouth_owns_axis(edges, (32.0, 32.0, 0.0), (0.0, 0.0, 1.0))
            for edges in mouths.values()
        ), (
            f"{name}: some face now owns the axis on its own, so this part "
            "no longer exercises the escape hatch round 5 is about"
        )


def test_r5_a_tie_at_one_level_is_not_a_shared_cap(fixtures_dir: Path):
    """The tie-pooling half of the corner, stated as the numbers it turns on.

    `_EndEvidence._owns` pools the mouths of every cap TIED at one axial
    level, because a bore breaking out across a seam reaches one cap
    along several arcs and only the pair of them closes the loop. On the
    equal-leg corner that pooling misfires exactly: the two chamfers are
    distinct faces that merely agree on a number, and pooled they make a
    210 deg chain which DOES clear half a turn.

    So the old rule did not merely abstain here — it was outvoted. The
    tie at z=22 claims ownership and the real top at z=20 does not, and
    "outermost owned wins" then picks the pair, off the part. Pinned as
    that exact asymmetry, because it is what says the corner needed a
    different question rather than a wider threshold.
    """
    import aberp_cad_extract.holes as holes_mod

    with _silence_stdout_fd():
        shape = _load_step_shape(
            str(fixtures_dir / "bore_beside_two_chamfers_corner.step")
        )
    faces = holes_mod._collect_faces(shape)
    ancestors = holes_mod._EdgeFaces(faces)

    groups = []
    for face in faces:
        cyl = holes_mod._face_to_cyl(face)
        if cyl is None:
            continue
        for group in groups:
            if group.accepts(cyl):
                group.add(cyl)
                break
        else:
            groups.append(holes_mod._BoreGroup(cyl))
    bore = [group for group in groups if group.is_full_sweep()]
    assert len(bore) == 1
    group = bore[0]

    _low, high = holes_mod._walk_caps(group, ancestors, group.lo, group.hi)
    levels = sorted({cap[0] for cap in high.caps}, reverse=True)
    assert levels == pytest.approx([22.0, 20.0], abs=TOL), (
        f"the corner's high end should offer z=22 (both chamfers, tied) and "
        f"z=20 (the plate's top); got {levels}"
    )

    tied = high._at(levels[0], 1.0)
    assert len({cap[4] for cap in tied}) == 2, (
        "the two chamfers must reach z=22 as TWO distinct faces for this to "
        "be the pooling case at all"
    )
    assert high._owns(tied, group.origin, group.direction) is True, (
        "the pooled tie no longer claims the mouth, so this part has stopped "
        "exercising the defect: two distinct faces pooled into one 210 deg "
        "chain is what beat the real cap"
    )
    assert (
        high._owns(high._at(levels[1], 1.0), group.origin, group.direction) is False
    ), (
        "the plate's own top must NOT own the mouth on its own — it holds "
        "150 deg of it — or the corner would never have been mis-mined"
    )

    # And the rim rule ignores that claim: the end is the plate's top.
    assert high.resolve(
        group.hi, group.origin, group.direction, 1.0, group.radius
    )[0] == (pytest.approx(20.0, abs=TOL))


def test_r5_the_corner_family_is_stable_under_a_reversed_walk(
    fixtures_dir: Path, monkeypatch
):
    """S3 over the new machinery: rims must not depend on the walk order.

    `_mouth_rims` is the first thing in the miner to build a union-find
    over edges, and a union-find is exactly the kind of code whose
    component ORDER — and therefore whose "outermost rim" — can follow
    the order its input arrived in. OCCT does not contractually
    guarantee explorer order across versions, so reverse it explicitly
    and require the same answer to the bit.
    """
    import aberp_cad_extract.holes as holes_mod

    names = (
        "bore_beside_two_chamfers_corner",
        "bore_beside_uneven_chamfer_corner",
        "bore_on_a_chamfer_corner_boundary",
        "blind_bore_beside_two_chamfers_corner",
    )
    forward = {
        name: [hole.model_dump() for hole in _mine(fixtures_dir / f"{name}.step")]
        for name in names
    }

    original = holes_mod._collect_faces
    monkeypatch.setattr(
        holes_mod, "_collect_faces", lambda shape: list(reversed(original(shape)))
    )
    for name in names:
        walked = [hole.model_dump() for hole in _mine(fixtures_dir / f"{name}.step")]
        assert walked == forward[name], (
            f"{name}: the face-walk order reached the output — "
            f"forward={forward[name]} reversed={walked}"
        )


def test_r5_the_rim_rule_is_the_only_thing_holding_the_corner_together(
    fixtures_dir: Path, monkeypatch
):
    """REVERT-PROOF: take the rim decomposition away and all three go red.

    Disabling `_mouth_rims` drops every end back to round 4's per-face
    contest, which is exactly the miner that shipped before this round.
    Each corner fixture must then report its round-4 number, to the bit —
    so these fixtures are pinned to the mechanism and not merely to the
    outcome.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(holes_mod, "_mouth_rims", lambda mouths: [])
    # Round 6 bounded the fall-back by the mouth, so that guard has to
    # be conceded too before round 4's arm can carry anything off the
    # part — see `test_r6_the_mouth_bound_is_what_stops_the_fall_back`.
    monkeypatch.setattr(
        holes_mod, "_mouth_reach", lambda mouths, origin, direction, sign: None
    )

    hijacked = _mine(fixtures_dir / "bore_beside_two_chamfers_corner.step")
    assert len(hijacked) == 1
    _approx(hijacked[0].depth_mm, 22.0)

    hijacked = _mine(fixtures_dir / "bore_beside_uneven_chamfer_corner.step")
    assert len(hijacked) == 1
    _approx(hijacked[0].depth_mm, 23.0)

    hijacked = _mine(fixtures_dir / "blind_bore_beside_two_chamfers_corner.step")
    assert len(hijacked) == 1
    _approx(hijacked[0].depth_mm, 14.0)
    _approx_vec(hijacked[0].entry_point_mm, (32.0, 32.0, 22.0))


def test_r5_the_round_4_per_face_arm_still_carries_its_own_fixtures(
    fixtures_dir: Path, monkeypatch
):
    """Round 4's rule is a live fall-back, not dead code behind a newer one.

    With `_mouth_rims` disabled the four round-4 parts stay EXACT, which
    is what says the per-face ownership test still works and still earns
    its place. It is what answers an end whose mouth does not close — a
    partial or unreadable rim — and round 5 deliberately left that path
    standing rather than replacing it.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(holes_mod, "_mouth_rims", lambda mouths: [])

    for name, depth in (
        ("bore_beside_chamfered_edge", 20.0),
        ("blind_bore_beside_chamfered_edge", 12.0),
        ("bore_beside_concave_corner_fillet", 20.0),
        ("bore_inside_a_chamfer", 14.0),
    ):
        holes = _mine(fixtures_dir / f"{name}.step")
        assert len(holes) == 1, f"{name}: got {[h.diameter_mm for h in holes]}"
        assert holes[0].depth_mm == pytest.approx(depth, abs=TOL), (
            f"{name}: round 4's own fixture no longer survives without the "
            f"rim rule; got {holes[0].depth_mm}, want {depth}"
        )


def test_r5_in_memory_corner_chamfer_is_where_the_indirect_plane_bites():
    """The openness half of round 5, before STEP normalises it away.

    Built in memory for the same reason
    `test_b5_in_memory_filleted_block_is_where_the_sweep_union_bites` is:
    OCCT's STEP writer re-parametrises the carrier surface, and this
    defect lives in the parametrisation. Chamfer two ADJACENT top edges
    of a block and the second chamfer comes back on an INDIRECT
    (left-handed) `gp_Ax3`, whose parametric normal is the NEGATION of
    its `Axis().Direction()`. The face's FORWARD/REVERSED flag refers to
    the parametric normal, so reading the axis direction alone inverts
    that one face's outward normal — it votes "material continues" at a
    genuine exit, and being tied at the winning level it vetoes the
    opening. A Ø8 through-hole came back BLIND with a flat bottom, which
    prices as a different cycle on a different machine.

    OCCT's own `BRepClass3d_SolidClassifier` is the arbiter and agrees
    with `_plane_normal`: material is on the side it now reports.
    """
    from OCP.BRepAdaptor import BRepAdaptor_Surface
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepFilletAPI import BRepFilletAPI_MakeChamfer
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox, BRepPrimAPI_MakeCylinder
    from OCP.GeomAbs import GeomAbs_SurfaceType
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    import aberp_cad_extract.holes as holes_mod
    from aberp_cad_extract.holes import mine_cylindrical_holes as mine

    def one_edge(shape, want):
        from OCP.BRepAdaptor import BRepAdaptor_Curve
        from OCP.GeomAbs import GeomAbs_CurveType
        from OCP.TopAbs import TopAbs_EDGE
        from OCP.TopExp import TopExp_Explorer
        from OCP.TopoDS import TopoDS

        seen, chosen = [], []
        explorer = TopExp_Explorer(shape, TopAbs_EDGE)
        while explorer.More():
            edge = TopoDS.Edge_s(explorer.Current())
            explorer.Next()
            if any(edge.IsSame(other) for other in seen):
                continue
            seen.append(edge)
            curve = BRepAdaptor_Curve(edge)
            if curve.GetType() != GeomAbs_CurveType.GeomAbs_Line:
                continue
            mid = curve.Value(0.5 * (curve.FirstParameter() + curve.LastParameter()))
            if want(curve.Line().Direction(), mid):
                chosen.append(edge)
        assert len(chosen) == 1, f"expected 1 edge, found {len(chosen)}"
        return chosen[0]

    box = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    maker = BRepFilletAPI_MakeChamfer(box)
    maker.Add(
        6.0,
        one_edge(
            box,
            lambda d, p: abs(abs(d.Y()) - 1.0) <= 1e-9
            and abs(p.X() - 40.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    maker.Add(
        6.0,
        one_edge(
            box,
            lambda d, p: abs(abs(d.X()) - 1.0) <= 1e-9
            and abs(p.Y() - 40.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    axis = gp_Ax2(gp_Pnt(32.0, 34.0, -5.0), gp_Dir(0, 0, 1))
    shape = BRepAlgoAPI_Cut(
        maker.Shape(), BRepPrimAPI_MakeCylinder(axis, 4.0, 30.0).Shape()
    ).Shape()

    indirect = 0
    for face in holes_mod._collect_faces(shape):
        adaptor = BRepAdaptor_Surface(face)
        if adaptor.GetType() != GeomAbs_SurfaceType.GeomAbs_Plane:
            continue
        if not adaptor.Plane().Position().Direct():
            indirect += 1

    # Guard the guard: without an indirect carrier this probe silently
    # stops testing anything, so say so out loud instead.
    assert indirect == 1, (
        "this probe only means something while OCCT gives the second "
        f"chamfer an INDIRECT plane; it reported {indirect} of them, so the "
        "inverted-normal path it exists to cover is no longer reachable here"
    )

    holes = mine(shape)
    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    _approx(hole.depth_mm, 20.0)
    _approx_vec(hole.entry_point_mm, (32.0, 34.0, 0.0))
    assert hole.end_condition is HoleEndCondition.THROUGH, (
        f"a through-hole read {hole.end_condition}; the indirect chamfer "
        "plane is voting with an inverted outward normal"
    )
    assert hole.flat_bottom is False


def test_r5_ignoring_plane_handedness_re_breaks_the_corner_in_memory(
    monkeypatch,
):
    """REVERT-PROOF for the handedness correction, on the same part.

    Put `_plane_normal` back to reading `Axis().Direction()` alone — the
    code that shipped through round 4 — and the in-memory corner bore
    must go BLIND with a flat bottom again. Nothing else in the suite
    can say this: every committed STEP fixture has direct planes only,
    so the correction is inert on all of them by construction.
    """
    import aberp_cad_extract.holes as holes_mod

    def naive(plane):
        direction = plane.Axis().Direction()
        return holes_mod._unit(
            (float(direction.X()), float(direction.Y()), float(direction.Z()))
        )

    monkeypatch.setattr(holes_mod, "_plane_normal", naive)

    with pytest.raises(AssertionError, match="inverted outward normal"):
        test_r5_in_memory_corner_chamfer_is_where_the_indirect_plane_bites()


def test_s3_both_sides_drilled_is_stable_under_a_reversed_walk(
    fixtures_dir: Path, monkeypatch
):
    """S3: the same part must always yield the same hole.

    A bore drilled from both sides is two half-faces carrying OPPOSITE
    authored axis senses. The shipped `_merge_group` took its frame from
    `group[0]` — whichever half OCCT's face walk reached first — so
    `axis_unit` flipped between (0,0,-1) and (0,0,+1) and `entry_point_mm`
    jumped from one face of the block to the other, for one unchanged
    part. The final sort could not repair it, because it sorts on the very
    field that flipped.

    OCCT does not contractually guarantee explorer order across versions,
    so this reverses the walk explicitly rather than hoping to catch a
    future OCCT upgrade doing it. Both runs must agree exactly, and on the
    canonical answer.
    """
    import aberp_cad_extract.holes as holes_mod

    forward = _mine(fixtures_dir / "both_sides_drilled.step")

    original = holes_mod._collect_faces
    monkeypatch.setattr(
        holes_mod, "_collect_faces", lambda shape: list(reversed(original(shape)))
    )
    reversed_walk = _mine(fixtures_dir / "both_sides_drilled.step")

    assert len(forward) == 1 and len(reversed_walk) == 1
    for hole in (forward[0], reversed_walk[0]):
        _approx(hole.diameter_mm, 8.0)
        _approx(hole.depth_mm, 40.0)
        _approx_vec(hole.axis_unit, (0.0, 0.0, 1.0))
        _approx_vec(hole.entry_point_mm, (20.0, 20.0, 0.0))
        assert hole.end_condition is HoleEndCondition.THROUGH

    assert forward[0].model_dump() == reversed_walk[0].model_dump(), (
        "the face-walk order must not reach the output at all; "
        f"forward={forward[0].model_dump()} reversed={reversed_walk[0].model_dump()}"
    )


def test_axis_unit_carries_no_negative_zero(fixtures_dir: Path):
    """PIN: no `-0.0` on the wire.

    Negating a canonical axis to point a blind hole's drill into the
    material turns 0.0 into -0.0, and `json.dumps` writes it as `-0.0`.
    It compares EQUAL to 0.0, so no numeric assertion here would ever
    catch it — but the daemon blake3-hashes the encoded bytes into
    `feature_graph_hash`, so it would move the hash of any part with a
    blind hole. Checked on the sign bit, which is the only way to see it.
    """
    import math as _math

    for name in ("blind_hole_flat_bottom.step", "blind_hole_drill_point.step"):
        for hole in _mine(fixtures_dir / name):
            for value in list(hole.axis_unit) + list(hole.entry_point_mm):
                assert not (value == 0.0 and _math.copysign(1.0, value) < 0.0), (
                    f"{name}: -0.0 reached the wire, which changes "
                    "feature_graph_hash without changing the geometry"
                )


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

    It mirrors the Rust side's `skip_serializing_if = "Vec::is_empty"`,
    and both halves must agree or a graph stops surviving the Python →
    Rust → Python round-trip as the same bytes.

    CORRECTED (ADR-0112 adversarial S1). This docstring used to say the
    omission "is what keeps a hole-less part's `feature_graph_hash`
    byte-identical to its pre-v6 value". That was false on the extractor
    path and is the kind of claim that stops people looking: the same cut
    moved Python's `SCHEMA_VERSION` from 2 to 6, `_schema_version` is
    inside the encoding the daemon blake3-hashes, so the hash of a
    freshly extracted part changes whether or not it has holes. What the
    omission actually buys is that this field contributes NOTHING to the
    difference — pinned positively by
    `test_hole_free_extraction_differs_from_pre_v6_only_in_the_version`.
    """
    fg = extract_step(fixtures_dir / "unit_cube.step", material_grade="6061-T6")
    assert fg.located_holes == []

    payload = fg.to_canonical_dict()
    assert "located_holes" not in payload, (
        "an empty located_holes must emit NO key — an empty array would "
        "silently change feature_graph_hash for every hole-less part"
    )


def test_hole_free_extraction_differs_from_pre_v6_only_in_the_version(
    fixtures_dir: Path,
):
    """PIN the EXACT extractor-path encoding change (ADR-0112 S1).

    The branch moved Python's `SCHEMA_VERSION` 2 -> 6, and
    `_schema_version` is inside the encoding the daemon blake3-hashes
    into `feature_graph_hash` — so the hash of every freshly extracted
    part changes. Three doc sites claimed the opposite before the
    adversarial round caught it.

    Rather than replace one prose claim with another, pin the thing that
    actually matters: for a part with NO holes, `_schema_version` is the
    ONLY key whose value moved. Nothing else about the encoding drifted
    alongside it, and `located_holes` contributes no key at all. That
    makes the change auditable — a reviewer can see the whole blast
    radius here — and it goes red if any future cut sneaks a second
    encoding change in under cover of the version bump.
    """
    fg = extract_step(fixtures_dir / "unit_cube.step", material_grade="6061-T6")
    payload = fg.to_canonical_dict()

    assert payload["_schema_version"] == 6
    assert "located_holes" not in payload

    # The pre-v6 key set, written out rather than derived, so this test
    # cannot drift along with the code it is pinning.
    assert set(payload) == {
        "_schema_version",
        "bounding_box_mm",
        "volume_mm3",
        "surface_area_mm2",
        "material_grade",
        "features",
        "requires_5_axis",
        "thin_wall_present",
    }, (
        "a hole-less extraction must carry exactly the pre-v6 key set; any "
        "addition here is a SECOND unflagged change to feature_graph_hash"
    )


def test_hole_mining_failure_does_not_kill_the_extraction(
    fixtures_dir: Path, monkeypatch, capsys
):
    """PIN the failure posture — BOTH halves of it (ADR-0112 B.4 + S2).

    Half one, unchanged: hole mining does not fail the extraction from
    inside. The graph is still built and still priceable, so a miner bug
    cannot cost the pipeline the geometry it does have.

    Half two, ADDED by the ADR-0112 adversarial round, and the reason the
    original posture was not enough: it also has to be LOUD. Empty
    `located_holes` is the honest encoding of "this part has no holes"
    AND was the degradation value for "mining crashed", so the two were
    indistinguishable on the wire — a 200-hole part that failed to mine
    quoted exactly like a blank billet. The old code wrote a prose
    warning to stderr that nothing read (the Rust wrapper only looked at
    stderr on non-zero exit). It now writes a machine-readable sentinel
    that the wrapper greps for on the success path and turns into a hard
    `ExtractError::HoleMiningFailed`.
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

    # …but it is NOT silent. This is the half that makes the degradation
    # safe: without the marker the daemon cannot tell this graph from a
    # genuinely hole-free one.
    err = capsys.readouterr().err
    assert step_mod.HOLE_MINING_FAILED_SENTINEL in err, (
        "a mining failure must leave a machine-readable marker on stderr; "
        "without it the daemon cannot distinguish 'could not measure the "
        f"holes' from 'this part has none'. stderr was: {err!r}"
    )
    assert "RuntimeError: simulated OCCT face-walk explosion" in err, (
        "the diagnostic has to travel with the marker, or the audit entry "
        "records a failure nobody can debug"
    )


def test_a_successful_extraction_emits_no_failure_sentinel(fixtures_dir: Path, capsys):
    """Mutation guard for the test above: the marker must not be constant.

    If `extract_step` ever stamped the sentinel unconditionally, the
    assertion above would still pass — and EVERY extraction would then
    fail at the wrapper. Pin the negative arm too.
    """
    import aberp_cad_extract.extractors.step as step_mod

    fg = extract_step(fixtures_dir / "plate_4_through_holes.step", material_grade="6061-T6")
    assert len(fg.located_holes) == 4

    err = capsys.readouterr().err
    assert step_mod.HOLE_MINING_FAILED_SENTINEL not in err, (
        "a healthy extraction must NOT stamp the failure marker; "
        f"stderr was: {err!r}"
    )


# ── ADR-0112 adversarial round 6 ─────────────────────────────────────────
#
# A bore whose mouth STRADDLES an edge of the part, and a bore whose bottom
# is TANGENT to it. Both under-reported on round 5, which is the direction
# that costs money.

R6_STRADDLES = {
    # fixture: (diameter, depth, entry, axis, end condition, flat bottom)
    "bore_straddling_a_rounded_edge": (
        10.0, 20.0, (30.0, 20.0, 0.0), (0.0, 0.0, 1.0),
        HoleEndCondition.THROUGH, False,
    ),
    "blind_bore_straddling_a_rounded_edge": (
        10.0, 12.0, (30.0, 20.0, 20.0), (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND, True,
    ),
    "bore_straddling_a_concave_fillet": (
        6.0, 26.0 - math.sqrt(32.0), (36.0, 20.0, 0.0), (0.0, 0.0, 1.0),
        HoleEndCondition.THROUGH, False,
    ),
    "bore_through_a_domed_shoulder": (
        8.0, 20.0 + math.sqrt(55.0), (37.0, 20.0, 0.0), (0.0, 0.0, 1.0),
        HoleEndCondition.THROUGH, False,
    ),
}

#: What round 5 returned for each of them, to the digits it returned. Used
#: by the revert-proof below, and written out here because the numbers are
#: the finding: every one is SHORTER than the truth.
R6_ROUND5_STRADDLES = {
    # fixture: (round 5's depth, round 5's entry z)
    #
    # 14 + sqrt(20) is the R6 fillet's unbounded cylinder, 1.53 mm under
    # a plate that stops at 20; the blind bore is the same surface read as
    # a coordinate, 8 -> 18.4721 instead of 8 -> 20. On the concave part it
    # is the flat top's plane at 20 where the fillet over the axis reaches
    # 20.3431. On the dome it is the sphere's SECOND crossing, so both ends
    # move and the depth collapses to the chord between them, 2*sqrt(55).
    "bore_straddling_a_rounded_edge": (14.0 + math.sqrt(20.0), 0.0),
    "blind_bore_straddling_a_rounded_edge": (
        6.0 + math.sqrt(20.0),
        14.0 + math.sqrt(20.0),
    ),
    "bore_straddling_a_concave_fillet": (20.0, 0.0),
    "bore_through_a_domed_shoulder": (
        2.0 * math.sqrt(55.0),
        20.0 - math.sqrt(55.0),
    ),
}


@pytest.mark.parametrize("name", sorted(R6_STRADDLES))
def test_r6_a_straddled_edge_does_not_shorten_the_bore(fixtures_dir: Path, name):
    """The bore ends where the part ends, not where a NEIGHBOUR's carrier
    happens to cross the axis.

    Each of these puts the bore's mouth across an edge of the part, so two
    faces of the outer skin reach it and only one of them is the skin over
    the axis. Round 5 took the innermost crossing over every face of the
    rim; the other face's UNBOUNDED carrier dives under the material and
    won, and every answer came back SHORT. The exact shortfalls are pinned
    in :func:`test_r6_keeping_every_rim_face_re_breaks_the_straddles`.

    The four cases are deliberately not variations of one shape: the round
    puts the axis over the FLAT and the neighbour convex, the axis over the
    NEIGHBOUR and it concave, the same on a blind bore where the error
    lands in a coordinate rather than a length, and a doubly-curved
    neighbour that is the skin at BOTH ends of the bore.
    """
    diameter, depth, entry, axis, end, flat = R6_STRADDLES[name]
    holes = _mine(fixtures_dir / f"{name}.step")
    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, diameter)
    _approx(hole.depth_mm, depth)
    _approx_vec(hole.entry_point_mm, entry)
    _approx_vec(hole.axis_unit, axis)
    assert hole.end_condition is end
    assert hole.flat_bottom is flat


def test_r6_ball_nose_blind_bore_is_blind_and_full_depth(fixtures_dir: Path):
    """A spherical bottom TANGENT to its bore reads BLIND at full depth.

    The ball-nose is the tangency a machine shop actually makes, and
    tangency is what leaves the two roots of the sphere EXACTLY equidistant
    from the mouth — the mouth being the sphere's own equator. Round 5's
    "keep the root nearest the mouth" then had nothing to choose with and
    took whichever `GeomAPI_IntCS` listed first: 8 deep and THROUGH against
    a true 16 and BLIND, on a pocket that does not go anywhere.

    Not a flat bottom: the cap is a sphere, and `_has_flat_bottom` must
    keep saying so — the depth being right is no excuse for pricing a ball
    nose as a flat-bottomed pocket.
    """
    holes = _mine(fixtures_dir / "ball_nose_blind_bore.step")
    assert len(holes) == 1, f"got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 8.0)
    _approx(hole.depth_mm, 16.0)
    _approx_vec(hole.entry_point_mm, (20.0, 20.0, 20.0))
    _approx_vec(hole.axis_unit, (0.0, 0.0, -1.0))
    assert hole.end_condition is HoleEndCondition.BLIND
    assert hole.flat_bottom is False


def test_r6_keeping_every_rim_face_re_breaks_the_straddles(
    fixtures_dir: Path, monkeypatch
):
    """REVERT-PROOF for the narrowing, on all four straddles at once.

    Put `_EndEvidence._skin_over_axis` back to answering "nothing to say"
    — which is round 5's rule exactly, since an empty answer is what the
    caller falls back on — and every straddle must return the wrong number
    it returned then, to the digit. Anything else means these fixtures have
    stopped covering the defect.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(
        holes_mod._EndEvidence,
        "_skin_over_axis",
        lambda self, keys, edges, origin, direction, radius: [],
    )
    for name, (depth, entry_z) in sorted(R6_ROUND5_STRADDLES.items()):
        holes = _mine(fixtures_dir / f"{name}.step")
        assert len(holes) == 1, name
        assert holes[0].depth_mm == pytest.approx(depth, abs=TOL), (
            f"{name}: without the narrowing this must be round 5's wrong "
            f"depth {depth}, got {holes[0].depth_mm}"
        )
        assert holes[0].entry_point_mm[2] == pytest.approx(entry_z, abs=TOL), (
            f"{name}: round 5's entry z was {entry_z}, got "
            f"{holes[0].entry_point_mm[2]}"
        )
        assert holes[0].depth_mm < R6_STRADDLES[name][1] - TOL, (
            f"{name}: the round-5 answer must be SHORT of the truth — that "
            "is what makes this an under-quote and not a rounding argument"
        )


def test_r6_nearest_root_alone_re_breaks_the_ball_nose(
    fixtures_dir: Path, monkeypatch
):
    """REVERT-PROOF for the tangency tie-break.

    Put `_root_for_end` back to a bare nearest-the-mouth pick and the
    ball-nose pocket must go back to reading 8 deep and THROUGH — half its
    depth, and an end condition that says the part has a hole in it.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(
        holes_mod,
        "_root_for_end",
        lambda roots, t_edge, at_low, radius, t_inner, material=None, face=None: min(
            roots, key=lambda root: abs(root[0] - t_edge)
        ),
    )
    holes = _mine(fixtures_dir / "ball_nose_blind_bore.step")
    assert len(holes) == 1
    assert holes[0].depth_mm == pytest.approx(8.0, abs=TOL)
    assert holes[0].end_condition is HoleEndCondition.THROUGH


def test_r6_the_ball_nose_does_not_depend_on_the_root_order(
    fixtures_dir: Path, monkeypatch
):
    """S3 over the tangency: a tie must not be settled by OCCT's list order.

    The bug this closes was not only a wrong number, it was a wrong number
    that FLIPPED — reverse `GeomAPI_IntCS`'s roots and round 5 answered
    16/BLIND instead of 8/THROUGH for one unchanged part. OCCT does not
    contractually guarantee that order, so reverse it explicitly and
    require the same answer to the bit.
    """
    import aberp_cad_extract.holes as holes_mod

    forward = _mine(fixtures_dir / "ball_nose_blind_bore.step")
    original = holes_mod._cap_axis_intersections
    monkeypatch.setattr(
        holes_mod,
        "_cap_axis_intersections",
        lambda face, origin, direction, radius: list(
            reversed(original(face, origin, direction, radius))
        ),
    )
    reversed_roots = _mine(fixtures_dir / "ball_nose_blind_bore.step")

    assert len(forward) == 1 and len(reversed_roots) == 1
    assert forward[0].depth_mm == reversed_roots[0].depth_mm
    assert forward[0].entry_point_mm == reversed_roots[0].entry_point_mm
    assert forward[0].end_condition is reversed_roots[0].end_condition
    _approx(forward[0].depth_mm, 16.0)


@pytest.mark.parametrize("name", sorted(R6_STRADDLES))
def test_r6_the_straddles_are_stable_under_a_reversed_walk(
    fixtures_dir: Path, monkeypatch, name
):
    """S3 over the narrowing: the answer must not follow the face walk.

    `_rim_barriers` explores faces and edges in whatever order OCCT hands
    them over, and `_skin_reaches_axis` stops at the first mouth sample
    that gets through. Neither may reach the answer.
    """
    import aberp_cad_extract.holes as holes_mod

    forward = _mine(fixtures_dir / f"{name}.step")
    original = holes_mod._collect_faces
    monkeypatch.setattr(
        holes_mod, "_collect_faces", lambda shape: list(reversed(original(shape)))
    )
    backward = _mine(fixtures_dir / f"{name}.step")

    assert len(forward) == 1 and len(backward) == 1
    assert forward[0].depth_mm == backward[0].depth_mm
    assert forward[0].entry_point_mm == backward[0].entry_point_mm
    assert forward[0].axis_unit == backward[0].axis_unit
    assert forward[0].end_condition is backward[0].end_condition
    _approx(forward[0].depth_mm, R6_STRADDLES[name][1])


def test_r6_the_narrowing_is_inert_where_the_bore_cuts_no_edge(fixtures_dir: Path):
    """The plain hole in the plain plate must not go NEAR the new machinery.

    `_rim_barriers` is what the narrowing runs on, and it is empty unless
    the bore actually cut an edge of the part. Where it is empty
    `_skin_over_axis` answers nothing and the caller keeps every cap, which
    is round 5's rule to the bit — so the whole of round 6 is inert on an
    ordinary hole by construction rather than by luck. Asserted here
    because it is the reason 38 committed fixtures did not move.

    The straddles are the other half of the same statement: there the
    barriers must NOT be empty, or these fixtures would be testing nothing.
    """
    import aberp_cad_extract.holes as holes_mod

    def barriers_of(name):
        with _silence_stdout_fd():
            shape = _load_step_shape(str(fixtures_dir / f"{name}.step"))
        faces = holes_mod._collect_faces(shape)
        ancestors = holes_mod._EdgeFaces(faces)
        groups = []
        for face in faces:
            cyl = holes_mod._face_to_cyl(face)
            if cyl is None:
                continue
            for group in groups:
                if group.accepts(cyl):
                    group.add(cyl)
                    break
            else:
                groups.append(holes_mod._BoreGroup(cyl))
        bores = [group for group in groups if group.is_full_sweep()]
        assert bores, name
        group = bores[0]
        low, high = holes_mod._walk_caps(group, ancestors, group.lo, group.hi)
        e1, e2 = holes_mod._perp_basis(group.direction)
        found = []
        for end in (low, high):
            for keys, edges in holes_mod._mouth_rims(end.mouths):
                faces_of = [
                    holes_mod.TopoDS.Face_s(end._face_keys.FindKey(key))
                    for key in keys
                ]
                found.append(
                    len(
                        holes_mod._rim_barriers(
                            faces_of, edges, group.origin, e1, e2, group.radius
                        )
                    )
                )
        return found

    assert max(barriers_of("plate_4_through_holes")) == 0, (
        "an ordinary hole in an ordinary plate cut no edge of it, so the "
        "round-6 narrowing must have nothing to work on there"
    )
    assert min(barriers_of("bore_straddling_a_rounded_edge")) >= 0
    assert max(barriers_of("bore_straddling_a_rounded_edge")) > 0, (
        "the straddle must actually produce a cut edge, or this fixture is "
        "not exercising the narrowing at all"
    )


def test_r6_the_mouth_bound_is_what_stops_the_fall_back(
    fixtures_dir: Path, monkeypatch
):
    """The fall-back may no longer carry an end OFF the part.

    `_EndEvidence.resolve` keeps round 4's per-face contest for the case
    where no rim closes — a real imported part whose mouth is a shade
    unsewn — and NO committed part reaches it. That was the danger: an arm
    round 5 proved insufficient at a chamfered corner, live, untested by
    any fixture, and silent when it fired.

    Round 6 bounds it by the mouth, which is boundary of the solid, so a
    cap further out than every edge of the mouth is off the part and
    cannot be where the bore leaves it. Conceded here in both directions:
    with the bound the corner stays on the plate at z=20 even with the rim
    rule and ownership both disabled, and without it the old z=22 comes
    straight back.
    """
    import aberp_cad_extract.holes as holes_mod

    def concede_the_newer_rules():
        monkeypatch.setattr(holes_mod, "_mouth_rims", lambda mouths: [])
        monkeypatch.setattr(
            holes_mod, "_mouth_owns_axis", lambda edges, origin, direction: True
        )

    concede_the_newer_rules()
    held = _mine(fixtures_dir / "bore_beside_two_chamfers_corner.step")
    assert len(held) == 1
    _approx(held[0].depth_mm, 20.0)
    assert held[0].end_condition is HoleEndCondition.THROUGH, (
        "the bounded fall-back must still read the corner bore as through — "
        "bounding it may not cost an end condition"
    )

    monkeypatch.setattr(
        holes_mod, "_mouth_reach", lambda mouths, origin, direction, sign: None
    )
    loose = _mine(fixtures_dir / "bore_beside_two_chamfers_corner.step")
    assert len(loose) == 1
    _approx(loose[0].depth_mm, 22.0)
    assert loose[0].depth_mm > 20.0, (
        "without the bound the chamfer planes must still carry the exit "
        "above a plate that stops at z=20, or this test has stopped "
        "covering the arm it exists for"
    )


def test_r6_the_mouth_bound_never_empties_the_contest(fixtures_dir: Path, monkeypatch):
    """Bounding the fall-back must not trade a wrong number for no number.

    The bound narrows the candidates and is dropped where it would leave
    none, so round 4's four parts still come out of that arm EXACTLY as
    they did — which is what makes this a guard rather than a rewrite.
    `bore_inside_a_chamfer` is the one that matters: there the chamfer
    really IS the cap and its crossing sits inside its own mouth, so the
    bound must let it through.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(holes_mod, "_mouth_rims", lambda mouths: [])
    for name, depth in (
        ("bore_beside_chamfered_edge", 20.0),
        ("blind_bore_beside_chamfered_edge", 12.0),
        ("bore_inside_a_chamfer", 14.0),
    ):
        holes = _mine(fixtures_dir / f"{name}.step")
        assert len(holes) == 1, name
        assert holes[0].depth_mm == pytest.approx(depth, abs=TOL), (
            f"{name}: round 4's arm must still carry this exactly, got "
            f"{holes[0].depth_mm}"
        )


# ── ADR-0112 adversarial round 7 ─────────────────────────────────────────
#
# Two under-quotes, and both of them are round 6's own fixes holding only
# where their fixture holds them.
#
# B1: the ball-nose tie-break fired on `==`, which is true of the
#     committed pocket and of essentially no other. B2: the evenly spaced
#     mouth rays found the conical boss's sliver of skin at six samples
#     and not at five, which is a property of the number and not of the
#     part.


R7_FIXTURES = {
    # fixture: (diameter, depth, entry, axis, end condition, flat bottom)
    "ball_nose_blind_bore_d6": (
        6.0, 9.8, (20.0, 20.0, 20.0), (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND, False,
    ),
    "ball_nose_blind_bore_d4_deep": (
        4.0, 16.2601508, (20.0, 20.0, 20.0), (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND, False,
    ),
    "bore_beside_a_conical_boss": (
        8.0, 25.0, (38.5, 22.0, 0.0), (0.0, 0.0, 1.0),
        HoleEndCondition.THROUGH, False,
    ),
    "bore_beside_a_taller_conical_boss": (
        8.0, 28.75, (38.5, 22.0, 0.0), (0.0, 0.0, 1.0),
        HoleEndCondition.THROUGH, False,
    ),
}

#: What round 6 mined for each, where it was wrong. Every one of these is
#: SHORT of the truth, which is what makes them under-quotes.
R7_ROUND6 = {
    "ball_nose_blind_bore_d6": (3.8, HoleEndCondition.THROUGH),
    "ball_nose_blind_bore_d4_deep": (12.2601508, HoleEndCondition.THROUGH),
    "bore_beside_a_conical_boss": (20.0, HoleEndCondition.THROUGH),
}


@pytest.mark.parametrize("name", sorted(R7_FIXTURES))
def test_r7_fixtures_are_exact(fixtures_dir: Path, name):
    """Every round-7 part, against the dimensions it was built from."""
    diameter, depth, entry, axis, end, flat = R7_FIXTURES[name]
    holes = _mine(fixtures_dir / f"{name}.step")

    assert len(holes) == 1, name
    _approx(holes[0].diameter_mm, diameter)
    _approx(holes[0].depth_mm, depth)
    _approx_vec(holes[0].entry_point_mm, entry)
    _approx_vec(holes[0].axis_unit, axis)
    assert holes[0].end_condition is end, name
    assert holes[0].flat_bottom is flat, name


@pytest.mark.parametrize("name", sorted(R7_ROUND6))
def test_r7_every_round6_answer_was_short(fixtures_dir: Path, name):
    """The defect was an UNDER-quote, on all three parts.

    Stated as its own test because "wrong" and "short" are different
    claims and only the second one is a quote that loses money.
    """
    was, _ = R7_ROUND6[name]
    assert was < R7_FIXTURES[name][1] - TOL, name


# ── B1: the tangency tie ─────────────────────────────────────────────────


def _ball_nose(nose_centre_z: float, radius: float, thickness: float = 20.0):
    """A ball-nose pocket, built in memory from its dimensions.

    Same construction as ``ball_nose_blind_bore`` with the depth and the
    cutter free, so a whole family can be swept without committing a STEP
    file per member. The pocket bottoms at ``nose_centre_z - radius``, so
    its depth from the plate top is ``thickness - nose_centre_z + radius``.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCylinder,
        BRepPrimAPI_MakeSphere,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, thickness).Shape()
    shaft = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(20.0, 20.0, nose_centre_z), gp_Dir(0, 0, 1)),
        radius,
        thickness + 10.0,
    ).Shape()
    nose = BRepPrimAPI_MakeSphere(gp_Pnt(20.0, 20.0, nose_centre_z), radius).Shape()
    return BRepAlgoAPI_Cut(BRepAlgoAPI_Cut(block, shaft).Shape(), nose).Shape()


#: The adversarial's 36-size family: one ball-nose pocket per cutter from
#: Ø1 to Ø18, all at the same nose depth, so nothing but the SIZE varies.
R7_BALL_NOSE_SIZES = tuple(0.5 + 0.25 * k for k in range(36))


def _ball_nose_verdicts(sizes, nose_centre_z=13.2, thickness=20.0):
    """``{radius: (depth, end condition)}`` over a family of pockets."""
    out = {}
    for radius in sizes:
        holes = [
            hole
            for hole in mine_cylindrical_holes(
                _ball_nose(nose_centre_z, radius, thickness)
            )
            if abs(hole.diameter_mm - 2.0 * radius) < TOL
        ]
        out[radius] = (
            (holes[0].depth_mm, holes[0].end_condition) if len(holes) == 1 else None
        )
    return out


def test_r7_the_tangency_tie_holds_across_a_whole_size_sweep():
    """The tie-break must be about the GEOMETRY, not about one pocket.

    Round 6's `==` is true of the committed fixture's arithmetic and of
    almost nothing else, so a fix that merely moves the coincidence would
    still pass a single new fixture. Thirty-six cutters at one depth is
    the family the defect was found on, and every one of them must come
    back at its full depth and BLIND.
    """
    verdicts = _ball_nose_verdicts(R7_BALL_NOSE_SIZES)
    wrong = {
        radius: got
        for radius, got in verdicts.items()
        if got is None
        or abs(got[0] - (20.0 - 13.2 + radius)) > TOL
        or got[1] is not HoleEndCondition.BLIND
    }
    assert not wrong, f"{len(wrong)}/{len(verdicts)} pockets mis-mined: {wrong}"


def test_r7_the_tangency_tie_holds_as_the_pocket_is_walked_down():
    """The same sweep along the OTHER axis: one cutter, every depth.

    A tie that survives 36 diameters at one depth could still be an
    artifact of that depth's arithmetic. Ø8 walked from a nose centre of
    4.05 to 19.35 in steps of 0.1 takes the tie through 154 different
    roundings of the same exact tangency.
    """
    depths = [4.05 + 0.1 * k for k in range(154)]
    wrong = []
    for nose_centre_z in depths:
        holes = mine_cylindrical_holes(_ball_nose(nose_centre_z, 4.0))
        want = 20.0 - nose_centre_z + 4.0
        if (
            len(holes) != 1
            or abs(holes[0].depth_mm - want) > TOL
            or holes[0].end_condition is not HoleEndCondition.BLIND
        ):
            wrong.append((nose_centre_z, [(h.depth_mm, h.end_condition) for h in holes]))
    assert not wrong, f"{len(wrong)}/{len(depths)} depths mis-mined: {wrong[:5]}"


def _round7_root_for_end(
    roots, t_edge, at_low, radius, _t_inner=None, _material=None, _face=None
):
    """`_root_for_end` exactly as ADR-0112 round 7 shipped it.

    Nearest the mouth, with the two crossings of a TANGENT cap broken by
    :func:`_tangency_band` in favour of the outward one. Kept verbatim in
    the tests, not in the miner, because D-19 replaced the trigger and
    the round-7 claims still have to be checkable — both the one that was
    right and the one that was not.
    """
    import aberp_cad_extract.holes as holes_mod

    best = min(roots, key=lambda root: abs(root[0] - t_edge))
    reach = abs(best[0] - t_edge)
    band = holes_mod._tangency_band(radius)
    tied = [root for root in roots if abs(abs(root[0] - t_edge) - reach) <= band]
    sign = -1.0 if at_low else 1.0
    outward = [root for root in tied if sign * (root[0] - t_edge) >= 0.0]
    inward = [root for root in tied if sign * (root[0] - t_edge) < 0.0]
    if not (outward and inward):
        return best
    return max(outward, key=lambda root: sign * root[0])


def test_r7_nearest_root_alone_re_breaks_the_whole_sweep(monkeypatch):
    """REVERT-PROOF for the void bound, on the whole tangency family.

    `test_r6_nearest_root_alone_re_breaks_the_ball_nose` does this on the
    one committed fixture. Thirty-six cutters is the family the round-7
    defect was found on, and it is what shows the protection is not a
    property of one pocket's arithmetic: take the bound away — leaving a
    bare nearest-the-mouth pick, which is round 5 — and most of the sweep
    must collapse.

    Until D-19 this test removed round 7's tangency BAND instead, on the
    reading that the band was what protected the sweep. It was, and it
    protected it for a reason that turned out to be more general than the
    band: the inward pole is disqualified for being inside the bore's own
    hollow, whether or not anything is tied with it (see
    :func:`aberp_cad_extract.holes._root_for_end`). Zeroing the band now
    changes nothing, so the revert has to remove the rule that is
    actually load bearing.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(
        holes_mod,
        "_root_for_end",
        lambda roots, t_edge, at_low, radius, t_inner, material=None, face=None: min(
            roots, key=lambda root: abs(root[0] - t_edge)
        ),
    )

    verdicts = _ball_nose_verdicts(R7_BALL_NOSE_SIZES)
    broken = [
        radius
        for radius, got in verdicts.items()
        if got is None
        or abs(got[0] - (20.0 - 13.2 + radius)) > TOL
        or got[1] is not HoleEndCondition.BLIND
    ]
    assert len(broken) > len(verdicts) // 2, (
        "with a bare nearest-the-mouth pick most of the size sweep must go "
        f"back to being short and THROUGH; only {len(broken)}/{len(verdicts)} did"
    )

    # ... and every one of them short by exactly one cutter diameter,
    # which is the signature of the INWARD pole winning the end.
    for radius in broken:
        got = verdicts[radius]
        assert got is not None and abs(
            got[0] - (20.0 - 13.2 + radius - 2.0 * radius)
        ) <= TOL, (radius, got)


def test_r7_zeroing_the_band_no_longer_moves_the_tangency(monkeypatch):
    """The band has stopped being the tangency's protection, deliberately.

    Pinned rather than deleted, because "this knob does nothing now" is a
    claim about the fix and not a tidy-up: D-19 promoted round 6's REASON
    — a crossing inside the bore's own hollow is not a candidate — from a
    tie-break to the rule, and the tangency became the zero-undercut
    member of the undercut family. The band survives only as the slack on
    that comparison, and a slack of zero rejects the inward pole just the
    same.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(holes_mod, "_tangency_band", lambda radius: 0.0)

    verdicts = _ball_nose_verdicts(R7_BALL_NOSE_SIZES)
    wrong = {
        radius: got
        for radius, got in verdicts.items()
        if got is None
        or abs(got[0] - (20.0 - 13.2 + radius)) > TOL
        or got[1] is not HoleEndCondition.BLIND
    }
    assert not wrong, f"{len(wrong)}/{len(verdicts)} pockets moved: {wrong}"


def test_r7_the_tangency_band_is_not_a_tuned_epsilon():
    """The band may be moved by decades without moving a single answer.

    An epsilon tuned to make a fixture pass has an answer that changes
    just outside it. This one does not, in its round-7 role as the
    tangency tie or in its D-19 role as the slack on the void bound: the
    noise it must swallow is ~4e-15 mm and the nearest thing it must not
    swallow is 0.64 mm away, so everything from 1e-12 to 1e-2 mm of
    surface mismatch — twenty decades of the quantity the band is derived
    from — gives the same numbers. :data:`SURFACE_CONFUSION_MM` sits in
    the middle of that because it is the kernel's own figure, not because
    it is where the fixtures pass.
    """
    import aberp_cad_extract.holes as holes_mod

    baseline = None
    for confusion in (1e-12, 1e-10, 1e-8, 1e-7, 1e-6, 1e-4, 1e-2):
        band = 2.0 * math.sqrt(2.0 * 3.0 * confusion)
        original = holes_mod._tangency_band
        try:
            holes_mod._tangency_band = lambda radius, _b=band: _b
            verdicts = _ball_nose_verdicts(R7_BALL_NOSE_SIZES[:12])
        finally:
            holes_mod._tangency_band = original
        signature = repr(sorted(verdicts.items()))
        if baseline is None:
            baseline = signature
        assert signature == baseline, (
            f"the answer moved when the surface-confusion figure was "
            f"{confusion}; that would make the band a tuned epsilon"
        )


def test_d19_no_true_cap_comes_near_the_void_bound(fixtures_dir: Path):
    """What the margin can decide, what it cannot, and that the answer is right.

    THREE claims, and D-19 round 2 rewrote this test because the version
    with two of them was measuring the wrong thing. All of it is measured
    over every committed fixture rather than asserted, so none of it can
    rot. For each crossing the cap walk offers, take how far OUTWARD of
    its own mouth it sits.

    **What the margin still decides.** Every crossing the bound KEEPS is
    at zero or positive — a true cap lands on its mouth or outside it,
    and not one of them lands inward even by a float's worth. So the
    slack swallows nothing, and it is still not a tuned epsilon.

    **What the margin turned out NOT to decide, which is the round-2
    finding.** This test used to go on to say that every DISCARDED
    crossing is at least 0.6 mm inward, and that between the two
    populations there is nothing — so a discard was safe. That was true
    of a corpus with no convex floor in it. ``domed_floor_pocket`` has
    one, its floor is 1.2e-3 mm inward of the mouth, and the bound
    discards it: a genuine floor, INSIDE the scale the slack itself works
    at (1.1e-3 mm at r=6). The two populations are not separated and
    cannot be — the crown is a free dimension of the part, so a real
    floor can be put at ANY margin, and
    ``test_d19r2_the_dome_floor_family_is_right_across_the_whole_crown_sweep``
    walks it across the slack to show that. No threshold does this job,
    which is the entire argument for asking the SOLID instead.

    So the discarded population is split by the material question rather
    than by the margin, and that is what is asserted: the discards that
    bound metal reach to within a thousandth of a millimetre of their
    mouth, and the discards that do not are the ones sitting 0.6 mm and
    further inside the bore's own hollow.

    **And the answer that comes out is the built one.** A margin with the
    right sign is not an answer. The old version stopped at the
    arithmetic, and the arithmetic looked just as healthy on the part the
    miner reported 30 metres deep. So the depth and the end condition
    every pinned fixture was built from are checked here too, in the same
    walk that measures the margins.
    """
    import aberp_cad_extract.holes as holes_mod

    kept, floors, hollow = [], [], []
    original = holes_mod._root_for_end

    def spy(roots, t_edge, at_low, radius, t_inner, material=None, face=None):
        if t_inner is not None:
            sign = -1.0 if at_low else 1.0
            slack = 0.5 * holes_mod._tangency_band(radius)
            margins = [sign * (root[0] - t_inner) for root in roots]
            live = [m for m in margins if m >= -slack]
            # Where the bound would empty it is not applied, so those
            # crossings are not ones it judged.
            if live:
                kept.extend(live)
                step = holes_mod._tangency_band(radius)
                for root, margin in zip(roots, margins):
                    if margin >= -slack:
                        continue
                    # Split by what the SOLID says, not by how far in it
                    # sits — which is the whole of the round-2 finding.
                    bucket = (
                        floors
                        if material is not None
                        and material.is_exit(root[0], sign, step)
                        else hollow
                    )
                    bucket.append(margin)
        return original(roots, t_edge, at_low, radius, t_inner, material, face)

    holes_mod._root_for_end = spy
    checked = 0
    try:
        for path in sorted(fixtures_dir.glob("*.step")):
            holes = _mine(path)
            want = COMMITTED_ONE_HOLE.get(path.stem)
            if want is None:
                continue
            assert len(holes) == 1, f"{path.stem}: got {len(holes)} holes"
            assert holes[0].depth_mm == pytest.approx(want[0], abs=TOL), path.stem
            assert holes[0].end_condition is want[1], path.stem
            checked += 1
    finally:
        holes_mod._root_for_end = original

    assert checked == len(COMMITTED_ONE_HOLE), (
        f"only {checked}/{len(COMMITTED_ONE_HOLE)} of the pinned parts were "
        "walked; the correctness half of this test is not covering what it says"
    )
    assert kept and floors and hollow, (
        "the corpus must exercise the kept side, a discarded crossing that "
        f"bounds metal, and one that does not; got {len(kept)}/"
        f"{len(floors)}/{len(hollow)}"
    )
    assert min(kept) >= 0.0, (
        f"a crossing the bound kept sat INWARD of its own mouth: {min(kept)}"
    )
    assert max(floors) > -1e-2, (
        "the corpus no longer holds a real floor the bound discards by a "
        f"hair, so it has stopped showing why the margin cannot decide "
        f"this; the closest is {max(floors)}"
    )
    assert max(hollow) < -0.6, (
        "a crossing in the bore's own hollow came within 0.6 mm of its "
        f"mouth: {max(hollow)}"
    )
    assert max(floors) > max(hollow), (
        "the two discarded populations must overlap or sit the wrong way "
        "round for a threshold; if they ever separate cleanly, this test "
        "is no longer showing what it claims"
    )


def test_r7_the_ball_nose_sweep_does_not_depend_on_the_root_order(monkeypatch):
    """S3 over the tangency, across the family rather than one part.

    Round 6 pinned root-order independence on its own fixture, where the
    tie fired. Everywhere else the tie did NOT fire, so the answer was
    whichever pole `GeomAPI_IntCS` listed first — S3, on 36 parts,
    invisible to a test that only looked at one.
    """
    import aberp_cad_extract.holes as holes_mod

    sizes = R7_BALL_NOSE_SIZES[:12]
    forward = _ball_nose_verdicts(sizes)

    original = holes_mod._cap_axis_intersections
    monkeypatch.setattr(
        holes_mod,
        "_cap_axis_intersections",
        lambda face, origin, direction, radius: list(
            reversed(original(face, origin, direction, radius))
        ),
    )
    backward = _ball_nose_verdicts(sizes)

    assert repr(sorted(forward.items())) == repr(sorted(backward.items()))


# ── B2: the pinched mouth ────────────────────────────────────────────────


def _conical_boss(cone_height: float, bore_x: float = 38.5, bore_y: float = 22.0):
    """A plate with an R10 conical boss on its edge, and a bore beside it."""
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut, BRepAlgoAPI_Fuse
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCone,
        BRepPrimAPI_MakeCylinder,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    cone = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(40.0, 20.0, 10.0), gp_Dir(0, 0, 1)), 10.0, 0.0, cone_height
    ).Shape()
    bore = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(bore_x, bore_y, -5.0), gp_Dir(0, 0, 1)), 4.0, 120.0
    ).Shape()
    return BRepAlgoAPI_Cut(BRepAlgoAPI_Fuse(block, cone).Shape(), bore).Shape()


def test_r7_evenly_spaced_rays_alone_re_break_the_boss(
    fixtures_dir: Path, monkeypatch
):
    """REVERT-PROOF for the end-anchored refinement.

    Put :func:`_mouth_ray_fractions` back to round 6's evenly spaced
    ladder and the conical boss must go back to reading 20.0 — the plate
    top, on a part whose skin over the axis is the cone 5 mm above it.
    """
    import aberp_cad_extract.holes as holes_mod

    monkeypatch.setattr(
        holes_mod,
        "_mouth_ray_fractions",
        lambda radius: [
            k / (holes_mod.MOUTH_RAY_SAMPLES + 1)
            for k in range(1, holes_mod.MOUTH_RAY_SAMPLES + 1)
        ],
    )
    holes = _mine(fixtures_dir / "bore_beside_a_conical_boss.step")
    assert len(holes) == 1
    _approx(holes[0].depth_mm, 20.0)


def test_r7_the_boss_does_not_depend_on_the_ray_count(fixtures_dir: Path, monkeypatch):
    """The COUNT must not reach the answer — which is the whole fix.

    Round 6 answered this part at six evenly spaced rays and not at five,
    so the number was load-bearing and one geometry away from being wrong
    again. With the ends refined, every count from one to twenty-one
    gives 25.0 to the bit: the coarse ladder only decides how quickly the
    refinement is reached, never what it finds.
    """
    import aberp_cad_extract.holes as holes_mod

    answers = {}
    for count in (1, 2, 3, 4, 5, 6, 7, 9, 13, 21):
        monkeypatch.setattr(holes_mod, "MOUTH_RAY_SAMPLES", count)
        holes = _mine(fixtures_dir / "bore_beside_a_conical_boss.step")
        assert len(holes) == 1, count
        answers[count] = holes[0].depth_mm

    assert len(set(answers.values())) == 1, f"the count reached the answer: {answers}"
    _approx(next(iter(answers.values())), 25.0)


def test_r7_the_refinement_finds_slivers_a_bumped_count_never_would():
    """Cone heights where NO evenly spaced count in reach gets it right.

    The boss's free sliver of mouth moves as the cone steepens. Round 6's
    five rays miss it at a height of 20 and find it at 25; six find 20 and
    a seventh geometry would defeat both. Sweeping the height is what
    shows the refinement is answering the part rather than the ladder.
    """
    heights = (20.0, 22.0, 25.0, 28.0, 30.0)
    wrong = []
    for height in heights:
        holes = mine_cylindrical_holes(_conical_boss(height))
        # the cone's own surface 2.5 mm off its axis: base + h*(1 - 2.5/10)
        want = 10.0 + height * 0.75
        if len(holes) != 1 or abs(holes[0].depth_mm - want) > TOL:
            wrong.append((height, want, [h.depth_mm for h in holes]))
    assert not wrong, wrong


def test_r7_the_mouth_ray_floor_is_load_bearing(monkeypatch):
    """Refining PAST the barrier's own accuracy invents routes.

    Arbitrarily close to a rim vertex every ray reads unobstructed,
    because the barrier's track begins at that vertex and a target a hair
    to one side of it passes the segment test on a technicality. This
    part — a bore under a boss whose cone is BURIED below the plate top
    over the axis — has no route at all, and reads 20.0 correctly. Drop
    :func:`_barrier_chord_tolerance` to a thousandth of a micron and the
    miner finds one of those phantom slivers at ~1e-7 of a mouth edge and
    reports the cone's crossing 6 mm inside the plate instead.

    So the floor is not a cost control. It is the statement that a ray
    aimed nearer a barrier than the barrier is known is not evidence.
    """
    import aberp_cad_extract.holes as holes_mod
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut, BRepAlgoAPI_Fuse
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCone,
        BRepPrimAPI_MakeCylinder,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    cone = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(40.0, 20.0, 6.2829), gp_Dir(0, 0, 1)), 11.1653, 0.0, 20.1485
    ).Shape()
    bore = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(33.4153, 22.0507, -5.0), gp_Dir(0, 0, 1)), 3.7313, 120.0
    ).Shape()
    part = BRepAlgoAPI_Cut(BRepAlgoAPI_Fuse(block, cone).Shape(), bore).Shape()

    holes = mine_cylindrical_holes(part)
    assert len(holes) == 1
    _approx(holes[0].depth_mm, 20.0)

    # TWO knobs, because D-19 added a second and independent reason this
    # part comes out right: the buried cone crosses the axis 6 mm inside
    # the bore's own hollow, so the void bound in `_root_for_end` throws
    # that crossing away before any route is aimed at it. The floor is
    # still what stops the phantom ROUTE being found, and showing that it
    # is means taking the later net away as well.
    monkeypatch.setattr(
        holes_mod, "_barrier_chord_tolerance", lambda radius: radius * 1e-12
    )
    monkeypatch.setattr(
        holes_mod, "_mouth_inward_bound", lambda edge, origin, direction, sign: None
    )
    unfloored = mine_cylindrical_holes(part)
    assert len(unfloored) == 1
    assert unfloored[0].depth_mm < 20.0 - TOL, (
        "without the floor this part must pick up a phantom route and read "
        f"the buried cone; got {unfloored[0].depth_mm}"
    )

    # The void bound alone does NOT make the floor redundant: put the
    # floor back and take only the bound away, and the part still reads
    # 20.0, so the assertion above is about the floor.
    monkeypatch.undo()
    monkeypatch.setattr(
        holes_mod, "_mouth_inward_bound", lambda edge, origin, direction, sign: None
    )
    unbounded = mine_cylindrical_holes(part)
    assert len(unbounded) == 1
    _approx(unbounded[0].depth_mm, 20.0)


def test_r7_the_mouth_ray_floor_is_not_a_tuned_epsilon(fixtures_dir: Path):
    """The floor may be moved by decades without moving an answer.

    The sliver that matters sits 0.52 mm from its vertex and the phantom
    ones sit at 9.4e-7 mm, so any floor between them does the same job.
    :func:`_barrier_chord_tolerance` lands at 1.95e-3 mm on a O8 bore
    because that is what the barrier march's chords are worth, and the
    three decades either side of it give the same answer.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._barrier_chord_tolerance
    answers = {}
    try:
        for scale in (1e-2, 1e-1, 1.0, 1e1, 1e2):
            holes_mod._barrier_chord_tolerance = (
                lambda radius, _s=scale: original(radius) * _s
            )
            holes = _mine(fixtures_dir / "bore_beside_a_conical_boss.step")
            assert len(holes) == 1, scale
            answers[scale] = holes[0].depth_mm
    finally:
        holes_mod._barrier_chord_tolerance = original

    assert len(set(answers.values())) == 1, f"the floor reached the answer: {answers}"
    _approx(next(iter(answers.values())), 25.0)


def test_r7_the_refinement_is_a_superset_of_round_6s_ladder():
    """Why no answer round 6 already had could move.

    `_skin_reaches_axis` returns True on the FIRST sample that gets
    through, so a sample set that contains round 6's can only ever turn a
    "no route" into a "route". The evenly spaced ladder being a prefix of
    the new schedule is therefore the whole regression argument for B2,
    and it is a property of the schedule rather than of any part — so it
    is checked here directly, at several radii.
    """
    import aberp_cad_extract.holes as holes_mod

    for radius in (0.5, 4.0, 37.5, 500.0):
        schedule = list(holes_mod._mouth_ray_fractions(radius))
        prefix = [
            k / (holes_mod.MOUTH_RAY_SAMPLES + 1)
            for k in range(1, holes_mod.MOUTH_RAY_SAMPLES + 1)
        ]
        assert schedule[: len(prefix)] == prefix, radius
        assert all(0.0 < fraction < 1.0 for fraction in schedule), radius
        # bounded, and bounded by the GEOMETRY rather than by a count
        assert len(prefix) < len(schedule) <= len(prefix) + 2 * 40, radius


def test_r7_the_refinement_is_inert_where_the_bore_cuts_no_edge(fixtures_dir: Path):
    """The ordinary hole in the ordinary plate never reaches the ladder.

    Round 6's inertness argument is that `_rim_barriers` is empty there,
    so `_skin_over_axis` returns before any ray is aimed. Round 7 adds
    samples behind that gate and not in front of it, which is why the 43
    committed fixtures stay bit-identical. Asserted by counting the rays
    actually aimed.
    """
    import aberp_cad_extract.holes as holes_mod

    aimed = []
    original = holes_mod._mouth_ray_fractions

    def spy(radius):
        for fraction in original(radius):
            aimed.append(fraction)
            yield fraction

    holes_mod._mouth_ray_fractions = spy
    try:
        assert len(_mine(fixtures_dir / "plate_4_through_holes.step")) == 4
        assert aimed == [], "an ordinary plate aimed a mouth ray"
        assert len(_mine(fixtures_dir / "bore_beside_a_conical_boss.step")) == 1
        assert aimed, "the boss aimed none, so this fixture tests nothing"
    finally:
        holes_mod._mouth_ray_fractions = original


# ══ D-19: the undercut spherical cavity (N4 + N3) ════════════════════════
#
# A spherical seat whose radius EXCEEDS the bore's: a ball-end cutter
# swung out, a lollipop cutter, an O-ring gland, a seat for a ball. The
# cavity is wider than the hole that reaches it, so the sphere's upper
# pole is left in mid-void one sphere radius above the mouth — and the
# round-6/7 rule, which only looked for a pole in the void when something
# was TIED with it, walked straight into it.


#: Committed undercut parts: name -> (diameter, depth, entry, axis, end,
#: flat). Every number is one the fixture was BUILT from — the seat sits
#: at z=12 on a 20 mm plate, so the pocket bottoms at
#: ``12 - (bore radius + undercut)`` and its depth is ``20`` less that.
D19_FIXTURES = {
    "undercut_ball_seat_blind_bore": (
        12.0,
        14.1,
        (20.0, 20.0, 20.0),
        (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND,
        False,
    ),
    "undercut_ball_seat_blind_bore_d8": (
        8.0,
        12.1,
        (20.0, 20.0, 20.0),
        (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND,
        False,
    ),
    "undercut_ball_seat_at_the_confusion_edge": (
        16.0,
        16.000001,
        (20.0, 20.0, 20.0),
        (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND,
        False,
    ),
    "undercut_ball_seat_below_the_confusion": (
        16.0,
        16.00000005,
        (20.0, 20.0, 20.0),
        (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND,
        False,
    ),
}

#: What the miner said BEFORE D-19: name -> (depth, end condition), or
#: ``None`` where it produced no hole at all. Measured against the real
#: kernel on these exact committed files, not reconstructed from prose.
D19_BEFORE = {
    "undercut_ball_seat_blind_bore": (1.9, HoleEndCondition.THROUGH),
    "undercut_ball_seat_blind_bore_d8": (3.9, HoleEndCondition.THROUGH),
    "undercut_ball_seat_at_the_confusion_edge": None,
    "undercut_ball_seat_below_the_confusion": (16.00000005, HoleEndCondition.BLIND),
}


@pytest.mark.parametrize("name", sorted(D19_FIXTURES))
def test_d19_the_undercut_seat_is_blind_to_its_real_floor(fixtures_dir: Path, name):
    """An undercut spherical seat is BLIND, and bottoms where it bottoms.

    The floor is the sphere's LOWER pole, which is solid-bounded metal;
    the upper pole is in the void the bore itself cut and is not the end
    of anything. Entry is the plate's top face, where the drill went in.
    """
    diameter, depth, entry, axis, end, flat = D19_FIXTURES[name]
    holes = _mine(fixtures_dir / f"{name}.step")
    assert len(holes) == 1, f"{name}: got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, diameter)
    _approx(hole.depth_mm, depth)
    _approx_vec(hole.entry_point_mm, entry)
    _approx_vec(hole.axis_unit, axis)
    assert hole.end_condition is end
    assert hole.flat_bottom is flat


@pytest.mark.parametrize("name", sorted(D19_BEFORE))
def test_d19_every_wrong_answer_was_an_under_quote(name):
    """The three ways this defect showed, all of them costing money.

    Two of the parts came back SHORT and THROUGH — a pocket priced as a
    fraction of itself, and as a hole that goes somewhere. The third came
    back as no hole at all, which is the same under-quote with nothing
    left to notice it by. Stated as its own test because "wrong" and
    "cheap" are different claims and only the second one loses money.
    """
    was = D19_BEFORE[name]
    truth = D19_FIXTURES[name][1]
    if was is None:
        return  # a dropped hole is short by the whole of it
    if was[1] is HoleEndCondition.BLIND:
        # the sub-confusion neighbour, which was already right
        assert abs(was[0] - truth) <= TOL, name
        return
    assert was[0] < truth - TOL, name


def test_d19_the_dropped_hole_was_a_silent_under_count():
    """The worst of the three: a part with one bore mining ZERO.

    Pinned separately because a missing hole cannot be spotted by
    eyeballing a depth. At Ø16 with the seat at z=12 the pole in the void
    lands at z=20.000001 — above the plate's own top face and still
    inside the far bound — so the span came out negative and
    :func:`mine_cylindrical_holes` dropped the bore.
    """
    assert D19_BEFORE["undercut_ball_seat_at_the_confusion_edge"] is None
    assert D19_FIXTURES["undercut_ball_seat_at_the_confusion_edge"][1] > 16.0


def test_d19_the_before_table_is_measured_and_not_remembered(fixtures_dir: Path):
    """`D19_BEFORE` is checkable, on the committed files, to the digit.

    A table of "what it used to say" written in prose rots the moment
    anybody edits a fixture. Put `_root_for_end` back to round 7 verbatim
    — the code as it shipped, band and all — and every entry has to come
    back out of the real kernel: 1.9 and THROUGH, 3.9 and THROUGH, no
    hole at all, and the sub-confusion neighbour unchanged at its true
    depth. That last one is the control: it says the fix moved the parts
    that were wrong and nothing else.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._root_for_end
    holes_mod._root_for_end = _round7_root_for_end
    try:
        for name, was in sorted(D19_BEFORE.items()):
            holes = _mine(fixtures_dir / f"{name}.step")
            if was is None:
                assert holes == [], f"{name}: round 7 dropped this bore; got {holes}"
                continue
            assert len(holes) == 1, name
            assert holes[0].depth_mm == pytest.approx(was[0], abs=TOL), name
            assert holes[0].end_condition is was[1], name
    finally:
        holes_mod._root_for_end = original


def _undercut_seat(bore_radius, undercut, nose_centre_z=12.0, thickness=20.0):
    """An undercut spherical seat, built in memory from its dimensions.

    Same construction as ``undercut_ball_seat_blind_bore`` with the
    cutter, the undercut and the depth free, so the whole family can be
    swept without a STEP file per member. The pocket bottoms at
    ``nose_centre_z - (bore_radius + undercut)``.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCylinder,
        BRepPrimAPI_MakeSphere,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, thickness).Shape()
    shaft = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(20.0, 20.0, nose_centre_z), gp_Dir(0, 0, 1)),
        bore_radius,
        thickness + 10.0,
    ).Shape()
    seat = BRepPrimAPI_MakeSphere(
        gp_Pnt(20.0, 20.0, nose_centre_z), bore_radius + undercut
    ).Shape()
    return BRepAlgoAPI_Cut(BRepAlgoAPI_Cut(block, shaft).Shape(), seat).Shape()


#: Nine cutters from O1 to O16 ...
D19_RADII = (0.5, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0)

#: ... crossed with the undercut swept ACROSS OCCT's confusion figure.
#: 0 is the tangent ball nose; 5e-8 and 1e-7 are at or below
#: `SURFACE_CONFUSION_MM`, where round 7's band still read the seat as the
#: tangency it cannot be told apart from; 4e-7 upward is where the band
#: stopped firing and the defect appeared. The boundary must not be
#: visible in the answers.
D19_UNDERCUTS = (0.0, 5e-8, 1e-7, 2e-7, 4e-7, 1e-6, 1e-5, 1e-3, 0.05, 0.5)


def _undercut_verdicts(radii=D19_RADII, undercuts=D19_UNDERCUTS, nose_centre_z=12.0):
    """``{(radius, undercut): (depth, end condition)}`` over the family."""
    out = {}
    for radius in radii:
        for undercut in undercuts:
            holes = mine_cylindrical_holes(_undercut_seat(radius, undercut, nose_centre_z))
            out[(radius, undercut)] = (
                (holes[0].depth_mm, holes[0].end_condition) if len(holes) == 1 else None
            )
    return out


def _undercut_wrong(verdicts, nose_centre_z=12.0, thickness=20.0):
    """The members of a sweep that are not at their built depth and BLIND."""
    return {
        key: got
        for key, got in verdicts.items()
        if got is None
        or abs(got[0] - (thickness - nose_centre_z + key[0] + key[1])) > TOL
        or got[1] is not HoleEndCondition.BLIND
    }


def test_d19_the_undercut_family_is_right_across_size_and_undercut():
    """Ninety seats: every cutter, every undercut, one mechanism.

    Two committed parts could be two coincidences. This is the family
    they belong to, swept along BOTH axes at once — the cutter, which
    sets how far into the void the wrong pole sits, and the undercut,
    which sets whether the kernel can tell the seat from a tangent ball
    nose at all. Nothing in the answers may show where that boundary is.
    """
    wrong = _undercut_wrong(_undercut_verdicts())
    assert not wrong, f"{len(wrong)}/90 seats mis-mined: {sorted(wrong)[:6]}"


def test_d19_the_undercut_family_is_right_as_the_seat_is_walked_down():
    """The same family along the third axis: one seat, every depth.

    O8 with 0.1 mm of undercut, walked from a seat centre of 4.6 to 18.9
    in steps of 0.1. Depth is the one thing the wrong answer tracked most
    closely — it was always short by exactly two seat radii — so walking
    it is what shows the fix is not reading a coincidence of one plate.

    The walk stops at 18.9 because at 19.1 the seat stops being one: its
    mouth reaches the plate's top face, the bore's cylindrical wall is
    consumed entirely, and what is left is a spherical dish with no bore
    to mine. That is a different part, not a harder one.
    """
    wrong = []
    for k in range(144):
        nose_centre_z = 4.6 + 0.1 * k
        verdicts = _undercut_verdicts(
            radii=(4.0,), undercuts=(0.1,), nose_centre_z=nose_centre_z
        )
        bad = _undercut_wrong(verdicts, nose_centre_z=nose_centre_z)
        if bad:
            wrong.append((nose_centre_z, bad))
    assert not wrong, f"{len(wrong)}/144 depths mis-mined: {wrong[:5]}"


def test_d19_nearest_root_alone_re_breaks_the_undercut_family():
    """REVERT-PROOF for the void bound, on the family it was found on.

    Take the bound away — a bare nearest-the-mouth pick, which is round 5
    — and every seat in the sweep must go back to being short by exactly
    two seat radii, because the crossing nearest the mouth IS the pole in
    the void and it sits one seat radius the wrong side of it.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._root_for_end
    holes_mod._root_for_end = (
        lambda roots, t_edge, at_low, radius, t_inner, material=None, face=None: min(
            roots, key=lambda root: abs(root[0] - t_edge)
        )
    )
    try:
        verdicts = _undercut_verdicts(radii=(2.0, 4.0, 6.0), undercuts=(1e-6, 0.1))
    finally:
        holes_mod._root_for_end = original

    assert _undercut_wrong(verdicts) == verdicts, "the bound was not load bearing"
    for (radius, undercut), got in verdicts.items():
        if got is None:
            continue
        short_by = 2.0 * (radius + undercut)
        assert abs(got[0] - (20.0 - 12.0 + radius + undercut - short_by)) <= TOL, (
            (radius, undercut),
            got,
        )
        assert got[1] is HoleEndCondition.THROUGH, ((radius, undercut), got)


def test_d19_round7s_own_rule_answers_the_tangency_and_not_the_undercut():
    """Round 7 was right about the tangency and wrong about the trigger.

    Put `_root_for_end` back to round 7 VERBATIM — nearest the mouth,
    with a tangency broken by the band in favour of the outward crossing
    — and the two halves of D-19's finding come apart cleanly:

    - the ball-nose sweep is still perfect, so nothing round 7 claimed
      about the tangency is being contradicted here;
    - every undercut seat past the confusion figure is still wrong, and
      the sub-confusion neighbour is still right, which is the whole
      shape of the defect: an answer that depended on whether OCCT could
      tell two surfaces apart.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._root_for_end
    holes_mod._root_for_end = _round7_root_for_end
    try:
        tangency = _ball_nose_verdicts(R7_BALL_NOSE_SIZES[:12])
        seats = _undercut_verdicts(radii=(4.0,), undercuts=(5e-8, 1e-7, 4e-7, 0.1))
    finally:
        holes_mod._root_for_end = original

    assert not {
        radius: got
        for radius, got in tangency.items()
        if got is None
        or abs(got[0] - (20.0 - 13.2 + radius)) > TOL
        or got[1] is not HoleEndCondition.BLIND
    }, "round 7's rule must still answer the tangency it was written for"

    survived = sorted(key for key in seats if key not in _undercut_wrong(seats))
    assert survived == [(4.0, 5e-8), (4.0, 1e-7)], (
        "round 7's rule must answer exactly the seats OCCT cannot tell from "
        f"a tangent ball nose, and no others; it answered {survived}"
    )


def test_d19_the_undercut_family_does_not_depend_on_the_root_order():
    """S3 over the seat: OCCT's list order may not reach the answer.

    `GeomAPI_IntCS` hands back a sphere's two poles in whatever order it
    likes, and until D-19 the nearer of them won an undercut seat. Now
    neither of them wins on being listed first, so reversing the list has
    to leave the family bit-identical.
    """
    import aberp_cad_extract.holes as holes_mod

    radii, undercuts = (2.0, 4.0, 8.0), (0.0, 1e-6, 0.1)
    forward = _undercut_verdicts(radii=radii, undercuts=undercuts)

    original = holes_mod._cap_axis_intersections
    holes_mod._cap_axis_intersections = lambda face, origin, direction, radius: list(
        reversed(original(face, origin, direction, radius))
    )
    try:
        backward = _undercut_verdicts(radii=radii, undercuts=undercuts)
    finally:
        holes_mod._cap_axis_intersections = original

    assert repr(sorted(forward.items())) == repr(sorted(backward.items()))

    # ... and the same over the FACE walk, which is the other order OCCT
    # does not promise. The bound is read off one mouth edge at a time,
    # so the order the edges arrive in must not reach the answer either.
    collect = holes_mod._collect_faces
    holes_mod._collect_faces = lambda shape: list(reversed(collect(shape)))
    try:
        reversed_walk = _undercut_verdicts(radii=radii, undercuts=undercuts)
    finally:
        holes_mod._collect_faces = collect

    assert repr(sorted(forward.items())) == repr(sorted(reversed_walk.items()))


def test_d19_the_void_bound_narrows_and_never_empties(fixtures_dir: Path):
    """Where the bound would leave no candidate it is not applied.

    A bore STRADDLING a convex rounded edge reaches the fillet along a
    mouth that lies entirely ABOVE where the fillet's carrier crosses the
    axis, so both of that carrier's crossings read as void and the bound
    has nothing left to offer. It withdraws, the fillet's evidence stands
    exactly as it did, and
    :meth:`aberp_cad_extract.holes._EndEvidence._skin_over_axis` goes on
    being what rejects it — which is where that rejection belongs, and
    which is why `test_r6_keeping_every_rim_face_re_breaks_the_straddles`
    still shows round 5's numbers to the digit.

    Asserted by counting: the straddles must actually reach the empty
    case, or this is describing something that does not happen.
    """
    import aberp_cad_extract.holes as holes_mod

    emptied = []
    original = holes_mod._root_for_end

    def spy(roots, t_edge, at_low, radius, t_inner, material=None, face=None):
        if t_inner is not None:
            sign = -1.0 if at_low else 1.0
            slack = 0.5 * holes_mod._tangency_band(radius)
            if not [r for r in roots if sign * (r[0] - t_inner) >= -slack]:
                emptied.append(len(roots))
        return original(roots, t_edge, at_low, radius, t_inner, material, face)

    holes_mod._root_for_end = spy
    try:
        assert len(_mine(fixtures_dir / "bore_straddling_a_rounded_edge.step")) == 1
    finally:
        holes_mod._root_for_end = original

    assert emptied, "the straddle never reached the empty case; the test is vacuous"


def _seat_on_axis(seat_point, direction, bore_radius, undercut, run=30.0):
    """An undercut seat on a bore of ARBITRARY position and direction.

    `_undercut_seat` builds the family Z-up and seat-at-the-low-end,
    which is one corner of the space the rule has to hold over. The rule
    is written in the bore's own frame — inward is ``-sign`` along the
    bore's axis, not down Z — so the frame has to be varied to show it.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCylinder,
        BRepPrimAPI_MakeSphere,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    frame = gp_Ax2(gp_Pnt(*seat_point), gp_Dir(*direction))
    bored = BRepAlgoAPI_Cut(
        block, BRepPrimAPI_MakeCylinder(frame, bore_radius, run).Shape()
    ).Shape()
    seat = BRepPrimAPI_MakeSphere(gp_Pnt(*seat_point), bore_radius + undercut).Shape()
    return BRepAlgoAPI_Cut(bored, seat).Shape()


#: label -> (seat point, bore direction, bore radius, undercut, true depth,
#: true entry, true axis, the depth D-19 replaced).
D19_ORIENTATIONS = {
    "seat at the bore's HIGH end": (
        (20.0, 20.0, 8.0),
        (0.0, 0.0, -1.0),
        4.0,
        0.1,
        12.1,
        (20.0, 20.0, 0.0),
        (0.0, 0.0, 1.0),
        3.9,
    ),
    "seat at the bore's LOW end": (
        (20.0, 20.0, 12.0),
        (0.0, 0.0, 1.0),
        4.0,
        0.1,
        12.1,
        (20.0, 20.0, 20.0),
        (0.0, 0.0, -1.0),
        3.9,
    ),
    "seat on a 30 deg angled bore": (
        (14.0, 20.0, 20.0 - 12.0 * math.cos(math.radians(30.0))),
        (-0.5, 0.0, math.cos(math.radians(30.0))),
        3.0,
        0.1,
        15.1,
        (8.0, 20.0, 20.0),
        (0.5, 0.0, -math.cos(math.radians(30.0))),
        8.9,
    ),
}


@pytest.mark.parametrize("label", sorted(D19_ORIENTATIONS))
def test_d19_the_void_bound_is_in_the_bores_own_frame(label):
    """The seat is not a Z-up, low-end fact, and neither is the rule.

    Inward is back up the BORE, so the same seat has to come out right
    capping the bore's high end, capping its low end, and on an axis
    tilted 30 deg out of Z where "up" and "outward" are different
    directions. All three read short and THROUGH before D-19, by exactly
    two seat radii, which is the same signature the Z-up family shows.
    """
    point, direction, radius, undercut, depth, entry, axis, was = (
        D19_ORIENTATIONS[label]
    )
    holes = mine_cylindrical_holes(
        _seat_on_axis(point, direction, radius, undercut)
    )
    assert len(holes) == 1, f"{label}: got {[h.depth_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, 2.0 * radius)
    _approx(hole.depth_mm, depth)
    _approx_vec(hole.entry_point_mm, entry)
    _approx_vec(hole.axis_unit, axis)
    assert hole.end_condition is HoleEndCondition.BLIND
    # the pre-D-19 answer, and the arithmetic that makes it a signature
    assert was == pytest.approx(depth - 2.0 * (radius + undercut), abs=TOL)
    assert was < depth - TOL


# ══ D-19 round 2: the mirror — a convex floor, and caps off the part ═════
#
# Round 1 fixed a crossing in mid-void being taken as the floor. Round 2
# is the same function failing in the opposite direction: the real floor
# DISCARDED for standing where a mid-void crossing would stand, and then
# the crossing that inherits the end never checked for being on the part
# at all. It over-quotes where round 1 under-quoted, and on a dome-floored
# pocket it did so by three orders of magnitude.


def _crown_carrier(crown: float, radius: float = 6.0) -> float:
    """The sphere radius that stands ``crown`` proud over ``radius``."""
    return (radius * radius + crown * crown) / (2.0 * crown)


#: The round-2 parts: name -> (diameter, depth, entry, axis, end, flat).
#: Every number is arithmetic on the dimensions the fixture was BUILT
#: from — the pocket's mouth is at z=8 and its dome crowns ``c`` above
#: that, so a 20 mm plate leaves ``12 - c``.
D19R2_FIXTURES = {
    "domed_floor_pocket": (
        12.0,
        12.0 - 1.2e-3,
        (30.0, 30.0, 20.0),
        (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND,
        False,
    ),
    "domed_floor_pocket_proud": (
        12.0,
        12.0 - 5.0,
        (30.0, 30.0, 20.0),
        (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND,
        False,
    ),
    "far_opening_through_bore": (
        8.0,
        12.0,
        (30.0, 30.0, 8.0),
        (0.0, 0.0, 1.0),
        HoleEndCondition.THROUGH,
        False,
    ),
    "spherical_mouth_undercut_bore": (
        4.0,
        20.0 - math.sqrt(2.556**2 - 2.0**2) - 9.368,
        (30.0, 30.0, 20.0 - math.sqrt(2.556**2 - 2.0**2)),
        (0.0, 0.0, -1.0),
        HoleEndCondition.BLIND,
        True,
    ),
}

#: What the miner said after D-19 round 1 and before round 2:
#: name -> (depth, end condition). Measured against the real kernel on
#: these exact committed files by
#: ``test_d19r2_the_before_table_is_measured_and_not_remembered``.
D19R2_BEFORE = {
    "domed_floor_pocket": (30012.0, HoleEndCondition.THROUGH),
    "domed_floor_pocket_proud": (19.2, HoleEndCondition.THROUGH),
    "far_opening_through_bore": (24.0, HoleEndCondition.BLIND),
    "spherical_mouth_undercut_bore": (13.188, HoleEndCondition.UNKNOWN),
}


@pytest.mark.parametrize("name", sorted(D19R2_FIXTURES))
def test_d19r2_fixtures_are_exact(fixtures_dir: Path, name):
    """The four round-2 parts, against the dimensions they were built from.

    - the two DOMED FLOORS bottom on their crown, which is inward of the
      mouth and is still the floor;
    - the FAR-OPENING bore is through, and ends where its own wall ends
      rather than four millimetres under the plate;
    - the SPHERICAL MOUTH UNDERCUT is blind and flat-bottomed, and its
      entry is the top of the BORE — the same convention
      ``countersunk_blind_bore`` and ``chamfered_mouth_bore`` are pinned
      on, that a relief cut at the mouth is not part of the hole.
    """
    diameter, depth, entry, axis, end, flat = D19R2_FIXTURES[name]
    holes = _mine(fixtures_dir / f"{name}.step")
    assert len(holes) == 1, f"{name}: got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, diameter)
    _approx(hole.depth_mm, depth)
    _approx_vec(hole.entry_point_mm, entry)
    _approx_vec(hole.axis_unit, axis)
    assert hole.end_condition is end, name
    assert hole.flat_bottom is flat, name


def _d19_round1_root_for_end(
    roots, t_edge, at_low, radius, t_inner, _material=None, _face=None
):
    """`_root_for_end` exactly as D-19 round 1 shipped it.

    The void bound and the nearest pick, with no material question and
    no off-the-part refusal. Kept verbatim in the tests, not in the
    miner, because round 2 replaced the rule and round 1's claims still
    have to be checkable — both the one that was right (an undercut
    seat's upper pole is not a floor) and the one that was not (nor is
    a convex floor's crown).
    """
    import aberp_cad_extract.holes as holes_mod

    if t_inner is None:
        live = list(roots)
    else:
        sign = -1.0 if at_low else 1.0
        slack = 0.5 * holes_mod._tangency_band(radius)
        live = [root for root in roots if sign * (root[0] - t_inner) >= -slack]
    return min(live or roots, key=lambda root: abs(root[0] - t_edge))


def test_d19r2_the_before_table_is_measured_and_not_remembered(fixtures_dir: Path):
    """`D19R2_BEFORE` is checkable, on the committed files, to the digit.

    Put `_root_for_end` back to D-19 round 1 VERBATIM — void bound,
    nearest pick, nothing else — and every entry has to come back out of
    the real kernel: 30012 and THROUGH, 19.2 and THROUGH, 24.0 and BLIND
    in a plate 20 mm thick, and 13.188 and UNKNOWN.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._root_for_end
    holes_mod._root_for_end = _d19_round1_root_for_end
    try:
        for name, was in sorted(D19R2_BEFORE.items()):
            holes = _mine(fixtures_dir / f"{name}.step")
            assert len(holes) == 1, name
            assert holes[0].depth_mm == pytest.approx(was[0], abs=TOL), name
            assert holes[0].end_condition is was[1], name
    finally:
        holes_mod._root_for_end = original


@pytest.mark.parametrize("name", sorted(D19R2_BEFORE))
def test_d19r2_every_wrong_answer_was_an_over_quote(name):
    """The mirror of D-19 round 1, and it costs money the other way.

    Round 1's undercut seat came back SHORT — a pocket priced as a
    fraction of itself. Round 2's parts come back LONG, every one of
    them, and two of them longer than the plate is thick. An over-quote
    loses the job rather than the margin, and a bore reported 30 metres
    deep in a 20 mm plate is a number no downstream sanity check that
    only looks at end conditions would catch: it came back THROUGH,
    which is a perfectly ordinary thing for a hole to be.
    """
    was = D19R2_BEFORE[name][0]
    truth = D19R2_FIXTURES[name][1]
    assert was > truth + TOL, (name, was, truth)


def test_d19r2_two_of_them_were_longer_than_the_part(fixtures_dir: Path):
    """The claim that makes these impossible and not merely wrong.

    A depth is a length and it can be argued about. A bore that ENTERS
    outside the material cannot be argued about — there is nothing there
    to start the drill in. Both of these did, and in opposite directions:
    the domed pocket entered 29992 mm BELOW a plate that starts at z=0,
    and the mouth-undercut bore put its far end 2.556 mm ABOVE a plate
    that stops at z=20. Measured off the part's own bounding box, so the
    claim is about the geometry rather than about two remembered numbers.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._root_for_end
    holes_mod._root_for_end = _d19_round1_root_for_end
    try:
        outside = {}
        for name in ("domed_floor_pocket", "spherical_mouth_undercut_bore"):
            with _silence_stdout_fd():
                shape = _load_step_shape(str(fixtures_dir / f"{name}.step"))
            box = holes_mod.Bnd_Box()
            holes_mod.BRepBndLib.Add_s(shape, box)
            _x0, _y0, z_lo, _x1, _y1, z_hi = box.Get()
            holes = holes_mod.mine_cylindrical_holes(shape)
            assert len(holes) == 1, name
            ends = (
                holes[0].entry_point_mm[2],
                holes[0].entry_point_mm[2] + holes[0].depth_mm * holes[0].axis_unit[2],
            )
            outside[name] = [z for z in ends if z < z_lo or z > z_hi]
    finally:
        holes_mod._root_for_end = original

    assert outside["domed_floor_pocket"] == [-29992.0], outside
    assert outside["spherical_mouth_undercut_bore"] == [
        pytest.approx(22.556, abs=TOL)
    ], outside


def _domed_floor(crown, radius=6.0, wall_stop=8.0, plate=60.0, thickness=20.0):
    """A convex-floored pocket built in memory from its dimensions.

    Same construction as ``domed_floor_pocket`` with the crown free, so
    the whole family can be swept without a STEP file per member. The
    floor is the dome's crown at ``wall_stop + crown``.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCylinder,
        BRepPrimAPI_MakeSphere,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    carrier = _crown_carrier(crown, radius)
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), plate, plate, thickness).Shape()
    cutter = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(30.0, 30.0, wall_stop), gp_Dir(0, 0, 1)), radius, thickness + 10.0
    ).Shape()
    dome = BRepPrimAPI_MakeSphere(
        gp_Pnt(30.0, 30.0, wall_stop + crown - carrier), carrier
    ).Shape()
    return BRepAlgoAPI_Cut(block, BRepAlgoAPI_Cut(cutter, dome).Shape()).Shape()


#: The crown swept ACROSS the slack, which is 1.1e-3 mm at r=6. Below it
#: the void bound never fired and round 1 was already right; above it the
#: bound discarded the floor and the answer left the part. The boundary
#: must not be visible in the answers.
D19R2_CROWNS = (
    1e-5,
    1e-4,
    5e-4,
    1.0e-3,
    1.1e-3,
    1.2e-3,
    2e-3,
    1e-2,
    0.1,
    0.9,
    2.0,
    5.0,
    9.0,
)


def _domed_verdicts(crowns=D19R2_CROWNS, radius=6.0, wall_stop=8.0, thickness=20.0):
    """``{crown: (depth, end condition)}`` over the dome-floor family."""
    out = {}
    for crown in crowns:
        holes = mine_cylindrical_holes(_domed_floor(crown, radius, wall_stop))
        out[crown] = (
            (holes[0].depth_mm, holes[0].end_condition) if len(holes) == 1 else None
        )
    return out


def _domed_wrong(verdicts, wall_stop=8.0, thickness=20.0):
    """The members of a sweep not at their built depth and BLIND."""
    return {
        crown: got
        for crown, got in verdicts.items()
        if got is None
        or abs(got[0] - (thickness - wall_stop - crown)) > TOL
        or got[1] is not HoleEndCondition.BLIND
    }


def test_d19r2_the_dome_floor_family_is_right_across_the_whole_crown_sweep():
    """Thirteen crowns straddling the slack, one mechanism, no boundary.

    The defect appeared and disappeared as the crown crossed the void
    bound's slack — 1.1e-3 mm at r=6 — which is the signature of an
    answer that is a property of a tolerance rather than of the part. A
    pocket a micron flatter was perfect; a pocket a micron prouder came
    back 30 metres deep. Sweeping the crown by six decades either side of
    that figure is what says the boundary has stopped existing.
    """
    wrong = _domed_wrong(_domed_verdicts())
    assert not wrong, f"{len(wrong)}/{len(D19R2_CROWNS)} domes mis-mined: {wrong}"


def test_d19r2_the_dome_floor_family_is_right_across_the_cutter():
    """The same family along the other axis: every cutter, one crown.

    The wrong depth tracked the CARRIER, which is ``(r^2 + c^2) / 2c`` —
    so it scaled with the square of the cutter and the reciprocal of the
    crown, and no single part's arithmetic could show that. Six radii at
    a fixed 1.2e-3 mm crown, which is the far pole ranging from z=-2075
    to z=-53325.
    """
    wrong = {}
    for radius in (0.5, 1.0, 2.0, 3.0, 4.0, 6.0):
        verdicts = _domed_verdicts(crowns=(1.2e-3,), radius=radius)
        bad = _domed_wrong(verdicts)
        if bad:
            wrong[radius] = bad
    assert not wrong, f"{len(wrong)}/6 cutters mis-mined: {wrong}"


def test_d19r2_round1s_own_rule_answers_the_seat_and_not_the_dome():
    """Round 1 was right about the void and wrong about what proves it.

    Put `_root_for_end` back to D-19 round 1 VERBATIM and the two halves
    of round 2's finding come apart cleanly:

    - the undercut-seat sweep is still perfect, so nothing round 1
      claimed about a crossing in mid-void is being contradicted here;
    - every dome whose crown clears the slack is wrong, and every dome
      below it is right — the whole shape of the defect, an answer that
      depended on whether a tolerance happened to swallow the floor.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._root_for_end
    holes_mod._root_for_end = _d19_round1_root_for_end
    try:
        seats = _undercut_verdicts(radii=(2.0, 4.0, 6.0), undercuts=(1e-6, 0.1))
        domes = _domed_verdicts()
    finally:
        holes_mod._root_for_end = original

    assert not _undercut_wrong(seats), (
        "round 1's rule must still answer the seat it was written for; "
        f"it missed {sorted(_undercut_wrong(seats))}"
    )

    slack = 0.5 * holes_mod._tangency_band(6.0)
    survived = sorted(crown for crown in domes if crown not in _domed_wrong(domes))
    assert survived == sorted(crown for crown in D19R2_CROWNS if crown <= slack), (
        "round 1's rule must answer exactly the domes whose crown the "
        f"slack swallows and no others; it answered {survived} against a "
        f"slack of {slack}"
    )


# ── the three knobs, pinned directly ─────────────────────────────────────
#
# All three of these live inside `_root_for_end`, and `_walk_caps` wraps
# every call to it in `except Exception: continue`. So a knob removed by
# hand does not necessarily RED anything — it can raise, get swallowed,
# drop the cap silently, and leave an answer that happens not to move.
# These three call `_root_for_end` DIRECTLY, with hand-built crossings
# and no miner around them, so what they assert is the function's own
# return value and nothing can eat it.


def _root(t):
    """A crossing at axial parameter ``t``, with a normal nothing reads."""
    return (t, (0.0, 0.0, 1.0))


def test_d19_the_empty_fallback_is_pinned_directly_not_through_the_miner():
    """`min(live or roots, ...)`: the `or roots` half, asserted as a value.

    Where the void bound would leave NO candidate it is not applied — a
    bore straddling a convex rounded edge reaches a fillet whose carrier
    crosses the axis entirely inward of the mouth, so both crossings read
    as hollow and the bound has nothing to offer.
    ``test_d19_the_void_bound_narrows_and_never_empties`` shows that the
    straddle really reaches this case, but it shows it with a SPY, and a
    spy that goes wrong is swallowed by `_walk_caps` and reports an empty
    list rather than a failure — which is exactly what happened when
    round 2 changed the signature underneath it.

    So the fall-back is pinned here as arithmetic instead: two crossings,
    both inward of the mouth, both discarded, and the function must still
    return the nearer of them rather than raising or returning None.
    Delete the `or roots` and this line reds on a ValueError from
    `min(())` with nothing in the way to catch it.
    """
    import aberp_cad_extract.holes as holes_mod

    # Low end, mouth at t=0, both crossings well INWARD of it (t > 0).
    roots = [_root(3.0), _root(7.0)]
    got = holes_mod._root_for_end(roots, 0.0, True, 4.0, 0.0)
    assert got is not None, "the bound emptied the contest and gave nothing back"
    assert got[0] == 3.0, f"the fall-back must keep the NEAREST crossing; got {got[0]}"

    # ... and the same at the high end, where inward is the other way.
    roots = [_root(-3.0), _root(-7.0)]
    got = holes_mod._root_for_end(roots, 0.0, False, 4.0, 0.0)
    assert got is not None and got[0] == -3.0, got


class _StubMaterial:
    """An `_AxisMaterial` that answers the MATERIAL question from a set.

    `_root_for_end` asks its oracle two questions and this stub answers
    exactly one of them, which is the point. "Does the metal begin at
    this crossing?" is a property of the crossing alone, so a stub can
    stand in for the solid and pin the SELECTION rules without a part,
    without a kernel and without `_walk_caps`'s `except Exception:
    continue` in the way.

    "Is this crossing outside the extent of the face that produced it?"
    is NOT such a question — it is a question about a part and a face,
    and a stub has neither. D-19 round 3 is the bill for having let one
    answer it anyway: round 2's refusal keyed off the WHOLE PART's
    bounding box, so any unrelated feature that enlarged the box turned
    it off, and the stub could not see that because a stub has no extent
    to vary. So it refuses to answer, loudly, and the arm is pinned on
    real geometry by
    ``test_d19r3_the_refusal_is_pinned_on_a_real_face_and_a_real_part``.
    """

    def __init__(self, exits=()):
        self._exits = set(exits)

    def beyond_the_face(self, face, t):  # pragma: no cover — a guard
        raise AssertionError(
            "the off-the-face refusal may not be pinned by a stub: a stub "
            "has no extent, and round 3's defect was that the refusal read "
            "the wrong thing's extent. Pin it on real geometry instead — "
            "see `test_d19r3_the_refusal_is_pinned_on_a_real_face_and_a_"
            "real_part`"
        )

    def is_exit(self, t, sign, step):
        return t in self._exits


def test_d19r2_the_selection_rules_are_pinned_directly_as_arithmetic():
    """The round-2 SELECTION rules, as return values, with a stub oracle.

    The end-to-end fixtures say these rules produce the right PARTS. This
    says what the rules ARE, on hand-built crossings, and it reaches one
    thing the corpus does not: which of SEVERAL material exits wins.

    Selection only. The refusal that follows it is a question about the
    extent of a real face on a real part, the stub is barred from
    answering it (see :class:`_StubMaterial`), and D-19 round 3 is what
    happened the round a stub was allowed to.

    No committed part has two exits on one cap face — a cap face is one
    surface and the axis leaves the metal through it once — so the
    "innermost wins" rule is reasoned rather than measured out there, and
    swapping it for a nearest-the-mouth pick reds nothing in the corpus.
    It is pinned here instead, because the reasoning is load bearing even
    where the corpus is silent: walking out of the bore, the FIRST metal
    is the floor, and a second exit further out is behind it, in a place
    the drill never reached.
    """
    import aberp_cad_extract.holes as holes_mod

    # Low end, mouth at t=0, so inward is t > 0 and outward is t < 0.
    crown, far = _root(0.5), _root(-900.0)

    # 1. Without an oracle, the bound discards the crown for being inward
    #    and the far crossing inherits the end. That is round 1, and it
    #    is the defect.
    assert holes_mod._root_for_end([crown, far], 0.0, True, 6.0, 0.0)[0] == -900.0

    # 2. With one, a crossing that bounds metal takes the end outright,
    #    inward of the mouth or not.
    oracle = _StubMaterial(exits=(0.5,))
    assert holes_mod._root_for_end([crown, far], 0.0, True, 6.0, 0.0, oracle)[0] == 0.5

    # 3. Among several, the INNERMOST — the first metal on the way out —
    #    and NOT the one nearest the mouth, which is the rule this would
    #    otherwise be indistinguishable from. Two exits at t=1 and t=5
    #    with the mouth at t=0: metal fills [1, 5], the bore's floor is
    #    its near face at t=5, and t=1 is the far side of the same slab
    #    with the drill never having reached it. Nearest-the-mouth would
    #    answer 1.0 and quote four millimetres of metal as hole.
    oracle = _StubMaterial(exits=(1.0, 5.0))
    got = holes_mod._root_for_end(
        [_root(1.0), _root(5.0)], 0.0, True, 6.0, 0.0, oracle
    )
    assert got[0] == 5.0, f"innermost, not nearest-the-mouth; got {got[0]}"
    # ... and the same at the high end, where "innermost" is the other way.
    oracle = _StubMaterial(exits=(-1.0, -5.0))
    got = holes_mod._root_for_end(
        [_root(-1.0), _root(-5.0)], 0.0, False, 6.0, 0.0, oracle
    )
    assert got[0] == -5.0, got

    # 4. No exit anywhere, and no face to judge the survivor against: it
    #    stands, and the refusal arm is not reached. What the refusal
    #    DOES is pinned on a real part and a real face by
    #    `test_d19r3_the_refusal_is_pinned_on_a_real_face_and_a_real_part`,
    #    because it is a question about extents and a stub has none —
    #    which is exactly how round 2's defect stayed invisible.
    oracle = _StubMaterial()
    assert holes_mod._root_for_end([crown, far], 0.0, True, 6.0, 0.0, oracle)[0] == -900.0


def test_d19r2_the_void_slack_is_inert_and_is_pinned_as_inert():
    """Nothing depends on the slack, and that is the assertion.

    Round 6 broke a tangency tie with this figure, round 7 widened the
    tie into a band, D-19 round 1 left it as the slack on the void bound,
    and round 2 handed its last arguable job — a floor landing a hair
    inward of its mouth — to the material question, which DECIDES it
    instead of tolerating it. So there is nothing left for the slack to
    do, and the honest thing is to say so in a form that would notice if
    it stopped being true.

    Zeroed here on its own rather than through `_tangency_band`, which is
    now also the material probe's step: zeroing the band would remove two
    things at once and the result would not say which.

    Directly first — a crossing one part in ten thousand inward of the
    mouth is discarded either way, since with the slack it is inside it
    and without it there is nothing to be inside — then over the whole
    committed corpus, where not one answer may move.
    """
    import aberp_cad_extract.holes as holes_mod

    assert holes_mod._void_slack(6.0) == pytest.approx(
        0.5 * holes_mod._tangency_band(6.0)
    )
    assert holes_mod._void_slack(6.0) > 0.0, "the slack must exist to be inert"

    original = holes_mod._void_slack
    holes_mod._void_slack = lambda radius: 0.0
    try:
        zeroed = _corpus_verdicts()
    finally:
        holes_mod._void_slack = original

    assert zeroed == _corpus_verdicts(), (
        "an answer moved when the void slack was zeroed. That is not a "
        "failure — it means the slack has become load bearing again and "
        "this test's claim, and `_void_slack`'s docstring, are now wrong"
    )


def test_d19r2_the_material_probe_is_not_a_tuned_epsilon():
    """The probe's step may be moved by decades without moving an answer.

    `_AxisMaterial.is_exit` steps off the crossing by a `_tangency_band`
    before asking the solid, and an epsilon tuned to make a fixture pass
    has an answer that changes just outside it. This one does not: the
    thing it must clear is the surface's own positional uncertainty
    (~4e-15 mm of float noise, and OCCT's 1e-7 confusion figure), and the
    thing it must not step over is the thinnest floor in the corpus. Four
    decades of the confusion figure the band is derived from — 1e-11 to
    1e-5 mm, which is a step from 1.5e-5 to 0.49 mm at r=6 — give the
    same numbers on every committed part.

    The upper end is where it stops, and that is a fact about the probe
    rather than a weakness: a step of half a millimetre reaches through
    the 1.2e-3 mm crown of ``domed_floor_pocket`` and out the other side.
    A probe must be shorter than the metal it is probing for.
    """
    import aberp_cad_extract.holes as holes_mod

    baseline = _corpus_verdicts()
    original = holes_mod._tangency_band
    for confusion in (1e-11, 1e-10, 1e-8, 1e-7, 1e-6, 1e-5):
        try:
            holes_mod._tangency_band = lambda radius, _c=confusion: 2.0 * math.sqrt(
                2.0 * max(radius, 0.0) * _c
            )
            got = _corpus_verdicts()
        finally:
            holes_mod._tangency_band = original
        assert got == baseline, (
            f"an answer moved when the surface-confusion figure was "
            f"{confusion}; that would make the probe step a tuned epsilon. "
            f"Moved: {sorted(k for k in baseline if baseline[k] != got[k])}"
        )




# ══ D-19 round 3: WHOSE extent ═══════════════════════════════════════════
#
# Round 2 refused a survivor crossing for lying outside the PART's own
# bounding box. The refusal is right and the box is right; the PART is
# not. A bounding box is a property of the whole shape and the crossing
# is a property of one face of one bore, so any unrelated feature that
# reaches further than the crossing does enlarges the box past it and
# turns the refusal off — and both of round 2's over-quotes come back on
# a single body cut from one block.
#
# What was invisible, and why. The end-to-end fixtures all held the
# part's overall extent fixed while varying the bore, so not one of them
# could see it; and the direct test of the selection rules answered the
# refusal from a STUB, which cannot have an extent at all. The stub is
# what hid the whole failure surface, so it no longer answers that
# question — see `_StubMaterial.beyond_the_face`.


def _r3_leg(depth):
    """A 10 x 10 leg hanging ``depth`` mm under the plate at (2, 2)."""
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox
    from OCP.gp import gp_Pnt

    return BRepPrimAPI_MakeBox(gp_Pnt(2.0, 2.0, -depth), 10.0, 10.0, depth).Shape()


def _r3_step(depth):
    """A full-width step ``depth`` mm under the plate's y=0 edge."""
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox
    from OCP.gp import gp_Pnt

    return BRepPrimAPI_MakeBox(gp_Pnt(0.0, 0.0, -depth), 60.0, 8.0, depth).Shape()


def _r3_boss(height):
    """A 10 x 10 boss standing ``height`` mm on the plate at (2, 2)."""
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox
    from OCP.gp import gp_Pnt

    return BRepPrimAPI_MakeBox(gp_Pnt(2.0, 2.0, 20.0), 10.0, 10.0, height).Shape()


def _r3_rib(height):
    """A 6 mm rib ``height`` mm tall along the plate's whole y=0 edge."""
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox
    from OCP.gp import gp_Pnt

    return BRepPrimAPI_MakeBox(gp_Pnt(0.0, 0.0, 20.0), 60.0, 6.0, height).Shape()


def _r3_plate(extra):
    """The 60 x 60 x 20 plate, with ``extra`` fused in BEFORE the bore.

    Fused first and cut second, so what the miner sees is ONE SOLID —
    the finding is not about assemblies, and a part that arrived as two
    bodies would be a different complaint.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Fuse
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox
    from OCP.gp import gp_Pnt

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 60.0, 60.0, 20.0).Shape()
    return block if extra is None else BRepAlgoAPI_Fuse(block, extra).Shape()


def _r3_far_opening(extra=None):
    """``far_opening_through_bore`` in memory, plus an unrelated feature."""
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut, BRepAlgoAPI_Fuse
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeCylinder, BRepPrimAPI_MakeSphere
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    shaft = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(30.0, 30.0, -5.0), gp_Dir(0, 0, 1)), 4.0, 35.0
    ).Shape()
    ball = BRepPrimAPI_MakeSphere(gp_Pnt(30.0, 30.0, 8.0 / 3.0), 20.0 / 3.0).Shape()
    return BRepAlgoAPI_Cut(
        _r3_plate(extra), BRepAlgoAPI_Fuse(shaft, ball).Shape()
    ).Shape()


def _r3_mouth_undercut(extra=None):
    """``spherical_mouth_undercut_bore`` in memory, plus a feature."""
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut, BRepAlgoAPI_Fuse
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeCylinder, BRepPrimAPI_MakeSphere
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    shaft = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(30.0, 30.0, 9.368), gp_Dir(0, 0, 1)), 2.0, 20.0 - 9.368
    ).Shape()
    dish = BRepPrimAPI_MakeSphere(gp_Pnt(30.0, 30.0, 20.0), 2.556).Shape()
    return BRepAlgoAPI_Cut(
        _r3_plate(extra), BRepAlgoAPI_Fuse(shaft, dish).Shape()
    ).Shape()


def _r3_domed(extra=None, crown=1.2e-3):
    """``domed_floor_pocket`` in memory, plus a feature."""
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepPrimAPI import BRepPrimAPI_MakeCylinder, BRepPrimAPI_MakeSphere
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    radius = 6.0
    carrier = _crown_carrier(crown, radius)
    cutter = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(30.0, 30.0, 8.0), gp_Dir(0, 0, 1)), radius, 30.0
    ).Shape()
    dome = BRepPrimAPI_MakeSphere(
        gp_Pnt(30.0, 30.0, 8.0 + crown - carrier), carrier
    ).Shape()
    return BRepAlgoAPI_Cut(
        _r3_plate(extra), BRepAlgoAPI_Cut(cutter, dome).Shape()
    ).Shape()


#: The round-3 parts: name -> (diameter, depth, entry, axis, end, flat).
#: Every one of them is a round-2 part with ONE unrelated feature added
#: somewhere else on it, and every expected value is its round-2 twin's,
#: unchanged — which is the whole claim.
D19R3_FIXTURES = {
    "far_opening_through_bore_with_a_leg": D19R2_FIXTURES["far_opening_through_bore"],
    "spherical_mouth_undercut_bore_with_a_boss": D19R2_FIXTURES[
        "spherical_mouth_undercut_bore"
    ],
    "domed_floor_pocket_with_a_rib": D19R2_FIXTURES["domed_floor_pocket"],
}

#: Which committed round-2 part each of them was built from.
D19R3_TWINS = {
    "far_opening_through_bore_with_a_leg": "far_opening_through_bore",
    "spherical_mouth_undercut_bore_with_a_boss": "spherical_mouth_undercut_bore",
    "domed_floor_pocket_with_a_rib": "domed_floor_pocket",
}

#: What the miner says on these parts when the refusal is put back to
#: round 2 — the PART's world bounding box instead of the FACE's.
#: name -> (depth, end condition). Measured on the committed files by
#: ``test_d19r3_an_unrelated_feature_may_not_rescue_a_cap_in_mid_air``.
D19R3_BEFORE = {
    "far_opening_through_bore_with_a_leg": (24.0, HoleEndCondition.BLIND),
    "spherical_mouth_undercut_bore_with_a_boss": (13.188, HoleEndCondition.UNKNOWN),
    "domed_floor_pocket_with_a_rib": (12.0 - 1.2e-3, HoleEndCondition.BLIND),
}


def _world_box_axial_span(shape, origin, direction):
    """One shape's WORLD-axis-aligned bounding box, projected onto an axis.

    `holes._box_axial_span` as rounds 2 and 3 shipped it, kept here
    rather than in the miner because D-19 round 4 is the finding that it
    is not an answer to either question it was being asked. Round 2 asked
    it of the whole PART and any unrelated feature moved the answer;
    round 3 asked it of one FACE and a whole-part ROTATION moved the
    answer, because the projection of a world-aligned box onto a tilted
    axis is the face's real extent plus the lateral extents times the
    direction's other components. Both superseded claims stay checkable
    from here — see `_round2_beyond_the_extent` and `D19R4_BEFORE`.
    """
    import aberp_cad_extract.holes as holes_mod

    box = holes_mod.Bnd_Box()
    holes_mod.BRepBndLib.Add_s(shape, box)
    if box.IsVoid():
        return None
    x_lo, y_lo, z_lo, x_hi, y_hi, z_hi = box.Get()
    ts = [
        holes_mod._dot((x - origin[0], y - origin[1], z - origin[2]), direction)
        for x in (x_lo, x_hi)
        for y in (y_lo, y_hi)
        for z in (z_lo, z_hi)
    ]
    return min(ts), max(ts)


def _round3_beyond_the_face(self, face, t):
    """`_AxisMaterial.beyond_the_face` as D-19 round 3 shipped it.

    The right OBJECT — the face that produced the crossing — measured
    with the wrong RULER: a world-axis-aligned box. Kept verbatim so
    round 3's claim stays checkable on the parts that break it, which are
    the ones that are not upright.
    """
    span = _world_box_axial_span(face, self._origin, self._direction)
    if span is None:
        return False
    return t < span[0] or t > span[1]


def _round2_beyond_the_extent(self, _face, t):
    """`_AxisMaterial.beyond_the_face` as D-19 round 2 shipped it.

    The PART's world bounding box, projected onto the bore's axis, with
    the face it is handed ignored — because round 2 was never handed one.
    Kept verbatim in the tests so that round 2's claim stays checkable on
    the parts that break it.
    """
    span = _world_box_axial_span(self._shape, self._origin, self._direction)
    if span is None:
        return False
    return t < span[0] or t > span[1]


@pytest.mark.parametrize("name", sorted(D19R3_FIXTURES))
def test_d19r3_fixtures_are_exact(fixtures_dir: Path, name):
    """The three round-3 parts, against the dimensions they were built from.

    A 6 mm LEG under one corner, a 4 mm BOSS on another, and a 5 mm RIB
    along one edge. None of them touches the bore, is reached by it, or
    is within 20 mm of it; every one of them moves the part's bounding
    box past a crossing the miner has to refuse.

    ONE SOLID, asserted rather than intended. The feature is fused into
    the block before the bore is cut, so this is a finding about a part a
    machinist would recognise — a plate with a leg on it — and not about
    an assembly, a multi-body STEP or anything the miner could reasonably
    have declined to handle.
    """
    from OCP.TopAbs import TopAbs_SOLID
    from OCP.TopExp import TopExp_Explorer

    diameter, depth, entry, axis, end, flat = D19R3_FIXTURES[name]
    with _silence_stdout_fd():
        shape = _load_step_shape(str(fixtures_dir / f"{name}.step"))
    explorer, solids = TopExp_Explorer(shape, TopAbs_SOLID), 0
    while explorer.More():
        solids += 1
        explorer.Next()
    assert solids == 1, f"{name}: {solids} solids; the finding is not about assemblies"

    holes = _mine(fixtures_dir / f"{name}.step")
    assert len(holes) == 1, f"{name}: got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, diameter)
    _approx(hole.depth_mm, depth)
    _approx_vec(hole.entry_point_mm, entry)
    _approx_vec(hole.axis_unit, axis)
    assert hole.end_condition is end, name
    assert hole.flat_bottom is flat, name


@pytest.mark.parametrize("name", sorted(D19R3_TWINS))
def test_d19r3_the_unrelated_feature_moves_not_one_bit(fixtures_dir: Path, name):
    """Stronger than "right": IDENTICAL to the part without the feature.

    ``test_d19r3_fixtures_are_exact`` says the answers are correct to a
    micron. This says they are the SAME FLOATS as the twin without the
    leg, the boss or the rib — depth, end condition, entry point and axis
    — because a feature 30 mm away has nothing to say about this bore and
    a rule that reads its answer off the part's overall size is not
    measuring the bore at all.
    """
    show = lambda holes: [
        (
            repr(h.diameter_mm),
            repr(h.depth_mm),
            h.end_condition.name,
            [repr(v) for v in h.entry_point_mm],
            [repr(v) for v in h.axis_unit],
            h.flat_bottom,
        )
        for h in holes
    ]
    with_it = show(_mine(fixtures_dir / f"{name}.step"))
    without = show(_mine(fixtures_dir / f"{D19R3_TWINS[name]}.step"))
    assert with_it == without, name


def test_d19r3_an_unrelated_feature_may_not_rescue_a_cap_in_mid_air(
    fixtures_dir: Path,
):
    """THE REVERT-PROOF, and the measurement of :data:`D19R3_BEFORE`.

    Put the refusal back to round 2 — the PART's world bounding box — and
    the two breakout parts go straight back to the answers round 2 was
    written to kill, on parts whose bores are bit-identical to the ones
    round 2 fixed:

    - the leg takes the box down to z=-6, past the breakout sphere's far
      pole at z=-4, and the bore reads 24.0 and BLIND in a 20 mm plate —
      four millimetres longer than the plate is thick, and a hole that
      really does go right through it called closed at the bottom;
    - the boss takes it up to z=24, past the mouth dish's far pole at
      z=22.556, and the bore reads 13.188 and UNKNOWN.

    The RIB part is the control and must not move under either rule: the
    dome's far pole is 30 metres under the plate, so no box was ever
    going to reach it, and what saves that pocket is the crown being
    recognised as material one branch earlier. Round 3 does not touch
    that branch and the assertion is that it did not.

    The twins WITHOUT the feature are re-measured under the same reverted
    rule in the same run, and must still be right — which is what makes
    this a finding about the part's overall extent rather than about the
    bore, the fixture or the reverted code being broken in general.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._AxisMaterial.beyond_the_face
    holes_mod._AxisMaterial.beyond_the_face = _round2_beyond_the_extent
    try:
        rescued = {
            name: _mine(fixtures_dir / f"{name}.step") for name in sorted(D19R3_BEFORE)
        }
        twins = {
            name: _mine(fixtures_dir / f"{twin}.step")
            for name, twin in sorted(D19R3_TWINS.items())
        }
    finally:
        holes_mod._AxisMaterial.beyond_the_face = original

    for name, was in sorted(D19R3_BEFORE.items()):
        holes = rescued[name]
        assert len(holes) == 1, name
        assert holes[0].depth_mm == pytest.approx(was[0], abs=TOL), name
        assert holes[0].end_condition is was[1], name

    for name, twin in sorted(D19R3_TWINS.items()):
        want = D19R2_FIXTURES[twin]
        holes = twins[name]
        assert len(holes) == 1, twin
        assert holes[0].depth_mm == pytest.approx(want[1], abs=TOL), (
            f"{twin} moved under the reverted rule, so this test is not "
            "measuring what the extra feature did"
        )
        assert holes[0].end_condition is want[4], twin

    # ... and the reverted rule really did move something, or the whole
    # test is vacuous and the two rules are the same rule.
    moved = [
        name
        for name in sorted(D19R3_BEFORE)
        if abs(D19R3_BEFORE[name][0] - D19R3_FIXTURES[name][1]) > TOL
    ]
    assert moved == [
        "far_opening_through_bore_with_a_leg",
        "spherical_mouth_undercut_bore_with_a_boss",
    ], moved


@pytest.mark.parametrize(
    "name",
    [
        "far_opening_through_bore_with_a_leg",
        "spherical_mouth_undercut_bore_with_a_boss",
    ],
)
def test_d19r3_every_rescued_answer_was_an_over_quote(name):
    """And by how much, so the cost is a number and not an adjective.

    The leg puts 24 mm of hole in a 20 mm plate — a 100 % over-quote, and
    a depth longer than the material it is cut in. The boss quotes 13.188
    against a true 9.0404 — 45.9 % — and calls a plainly blind pocket
    UNKNOWN, which is what a downstream router does not know how to
    price. Both are OVER, like the rest of round 2's family: an
    over-quote loses the job rather than the margin.
    """
    was = D19R3_BEFORE[name][0]
    truth = D19R3_FIXTURES[name][1]
    assert was > truth + TOL, (name, was, truth)
    over = 100.0 * (was - truth) / truth
    expected = {
        "far_opening_through_bore_with_a_leg": 100.0,
        "spherical_mouth_undercut_bore_with_a_boss": 45.9,
    }[name]
    assert abs(over - expected) < 0.1, (name, over, expected)


#: ``(builder, feature, sizes that clear the pole, sizes that reach past
#: it, the pole's own axial position)`` — one row per round-2 defect, and
#: the DOMED row is the control whose pole nothing reaches.
D19R3_SWEEP = {
    "far opening, leg": (
        _r3_far_opening,
        _r3_leg,
        (1.0, 2.0, 3.0, 3.9),
        (4.1, 6.0, 25.0),
    ),
    "far opening, step": (_r3_far_opening, _r3_step, (1.0, 3.9), (4.1, 12.0)),
    "mouth undercut, boss": (
        _r3_mouth_undercut,
        _r3_boss,
        (0.5, 1.0, 2.0, 2.5),
        (2.6, 4.0, 10.0),
    ),
    "mouth undercut, rib": (_r3_mouth_undercut, _r3_rib, (0.5, 2.5), (2.6, 8.0)),
    "domed floor, rib": (_r3_domed, _r3_rib, (0.5, 2.5, 2.6, 5.0, 40.0), ()),
    "domed floor, leg": (_r3_domed, _r3_leg, (0.5, 4.1, 6.0, 40.0), ()),
}


def _r3_depth(shape):
    """``(depth, end condition)`` of the one bore, or None."""
    import aberp_cad_extract.holes as holes_mod

    holes = holes_mod.mine_cylindrical_holes(shape)
    if len(holes) != 1:
        return None
    return holes[0].depth_mm, holes[0].end_condition


def test_d19r3_the_rescue_is_the_box_reaching_the_pole_and_nothing_else():
    """The mechanism, swept: four feature shapes, both directions, either rule.

    Three committed parts prove the defect exists. This proves what it
    IS, which is the part of a finding that decides whether the fix is
    the right one. A leg, a step, a boss and a rib — different shapes,
    different faces, opposite sides of the plate — and the only thing
    that matters about any of them is whether the box it makes reaches
    PAST the mid-air crossing:

    - under ROUND 2 every size short of the pole is right and every size
      past it is wrong, on every shape, in both directions. The flip is
      at z=-4 for the breakout sphere and z=+2.556 for the mouth dish,
      which are the poles themselves and not a tolerance;
    - under ROUND 3 nothing flips, anywhere, at any size, because the
      face that produced the crossing does not grow when the part does.

    The DOMED rows are the control: their pole is 30 metres under the
    plate, no feature reaches it, and both rules are right at every size.
    """
    import aberp_cad_extract.holes as holes_mod

    truth = {
        _r3_far_opening: (12.0, HoleEndCondition.THROUGH),
        _r3_mouth_undercut: (
            20.0 - math.sqrt(2.556**2 - 2.0**2) - 9.368,
            HoleEndCondition.BLIND,
        ),
        _r3_domed: (12.0 - 1.2e-3, HoleEndCondition.BLIND),
    }

    def sweep():
        out = {}
        for label, (build, feature, clear, past) in sorted(D19R3_SWEEP.items()):
            for size in clear + past:
                out[(label, size)] = _r3_depth(build(feature(size)))
        return out

    got = sweep()
    for (label, size), answer in sorted(got.items()):
        build = D19R3_SWEEP[label][0]
        want = truth[build]
        assert answer is not None, f"round 3 dropped the bore: {label} {size}"
        assert answer[0] == pytest.approx(want[0], abs=TOL), (label, size, answer)
        assert answer[1] is want[1], (label, size, answer)

    original = holes_mod._AxisMaterial.beyond_the_face
    holes_mod._AxisMaterial.beyond_the_face = _round2_beyond_the_extent
    try:
        round2 = sweep()
    finally:
        holes_mod._AxisMaterial.beyond_the_face = original

    for label, (build, _feature, clear, past) in sorted(D19R3_SWEEP.items()):
        want = truth[build]
        for size in clear:
            answer = round2[(label, size)]
            assert answer is not None and answer[0] == pytest.approx(
                want[0], abs=TOL
            ), (
                f"{label} at {size} is SHORT of the pole and round 2 was "
                f"already wrong there; got {answer}. That would make the "
                "flip a tolerance rather than the pole"
            )
        for size in past:
            answer = round2[(label, size)]
            assert answer is None or abs(answer[0] - want[0]) > TOL, (
                f"{label} at {size} reaches PAST the pole and round 2 still "
                f"answered correctly; got {answer}. The sweep is not "
                "reaching the defect it claims to bracket"
            )


def _r3_top_face(shape, origin, direction, want_span):
    """The one face of ``shape`` whose axial box span is ``want_span``."""
    import aberp_cad_extract.holes as holes_mod

    found = [
        face
        for face in holes_mod._collect_faces(shape)
        if _world_box_axial_span(face, origin, direction)
        == pytest.approx(want_span, abs=TOL)
    ]
    assert len(found) == 1, f"wanted one face spanning {want_span}, got {len(found)}"
    return found[0]


def test_d19r3_the_refusal_is_pinned_on_a_real_face_and_a_real_part():
    """The refusal arm, as a return value, on geometry that VARIES the extent.

    ``test_d19r2_the_selection_rules_are_pinned_directly_as_arithmetic``
    pins the rest of `_root_for_end` with a stub oracle, and that is the
    right shape for the rules that are pure selection. It is the WRONG
    shape for this one, and round 3 is what that cost: a stub has no
    extent, so a stub answering "is this crossing off the part?" cannot
    have an opinion about which part, and the entire failure surface —
    that the answer moved with a feature 30 mm from the bore — was
    invisible to it. So this arm is pinned on a REAL part, a REAL face
    and a REAL `_AxisMaterial`, twice: with an unrelated feature on the
    part and without one.

    The part is a plain 60 x 60 x 20 plate; the rib takes it to z=30. The
    face is the plate's own TOP face, which spans exactly z=20 and z=20.
    The crossings are hand-built: one at t=-0.5, inward of a mouth at
    t=0, which the void bound discards, and one at t=25, which survives
    only because of that discard. t=25 is INSIDE the ribbed part's box
    and 5 mm OUTSIDE the face's, which is the whole difference between
    the two rules stated as two numbers.
    """
    import aberp_cad_extract.holes as holes_mod

    origin, direction = (30.0, 30.0, 0.0), (0.0, 0.0, 1.0)
    crossings = [_root(-0.5), _root(25.0)]

    for label, extra in (("bare", None), ("ribbed", _r3_rib(10.0))):
        shape = _r3_plate(extra)
        top = _r3_top_face(shape, origin, direction, (20.0, 20.0))
        material = holes_mod._AxisMaterial(shape, origin, direction)

        # High end, mouth at t=0, so inward is t < 0 and outward is t > 0.
        got = holes_mod._root_for_end(
            crossings, 0.0, False, 6.0, 0.0, material, top
        )
        assert got is None, (
            f"{label}: a crossing 5 mm past the face that produced it capped "
            f"the bore anyway; got {got}"
        )

        # ... and the same survivor INSIDE the face's own extent stands,
        # so the refusal is about being off the FACE and nothing else.
        got = holes_mod._root_for_end(
            [_root(-0.5), _root(20.0)], 0.0, False, 6.0, 0.0, material, top
        )
        assert got is not None and got[0] == 20.0, (label, got)

    # And the two parts really do have different extents, or the loop
    # above ran the same case twice.
    spans = [
        _world_box_axial_span(_r3_plate(extra), origin, direction)
        for extra in (None, _r3_rib(10.0))
    ]
    assert spans[0][1] == pytest.approx(20.0, abs=TOL), spans
    assert spans[1][1] == pytest.approx(30.0, abs=TOL), spans



def test_d19r3_the_extent_question_is_cheaper_and_cannot_go_back():
    """The cost of the refusal, measured, and the old authority, banned.

    Round 2 built ONE bounding box OF THE WHOLE PART for every part it
    mined, whether any bore asked the question or not — `BRepBndLib` walks
    every face of the shape, and an ordinary four-hole plate paid for it
    and then never used it. Round 3 builds a box of ONE FACE, only where
    the void bound has discarded a crossing AND nothing bounds metal, so:

    - the four-hole plate builds NONE, where it used to build one;
    - across the whole committed corpus exactly ELEVEN boxes are built,
      one per part that has a rescued cap to refuse, each of a single
      face rather than of a whole shape.

    D-19 round 4 split the two numbers apart. The question is now ASKED
    135 times and ANSWERED from a per-face memo: a bore whose mouth an
    exporter has split into many edges reaches the same handful of faces
    once per edge, and `nurbs_far_opening_through_bore` asks about one
    face 121 times. Computing it 121 times cost 1.5 s on that one part,
    against 2.6 ms for its analytic twin, because `AddOptimal` on a
    B-spline patch is not cheap. Both numbers are pinned, because the
    gap between them is the memo and a change in either is a change in
    which crossings reach the arm.

    And the world box cannot quietly return. `_AxisMaterial` no longer
    takes one, no longer caches one, and no longer answers the question
    round 2 asked of it — pinned structurally rather than by prose,
    because "we do not do that any more" is exactly the kind of claim
    that decays into a comment. What it MAY cache is one span per FACE,
    which can only ever return the answer a second call would compute.
    """
    import inspect

    import aberp_cad_extract.holes as holes_mod
    from OCP.TopAbs import TopAbs_FACE

    assert not hasattr(holes_mod._AxisMaterial, "off_the_part"), (
        "the part-wide extent test must stay gone: it answered a question "
        "about one bore's crossing with a property of the whole part, and "
        "any unrelated feature turned it off"
    )
    assert "_span" not in holes_mod._AxisMaterial.__slots__, (
        "no per-part axial extent may be cached on the oracle again"
    )
    taken = list(inspect.signature(holes_mod._AxisMaterial.__init__).parameters)
    assert taken == ["self", "shape", "origin", "direction"], (
        f"`_AxisMaterial` must not be handed a part-wide box again; got {taken}"
    )

    here = Path(__file__).parent / "fixtures"
    asked, built = [], []
    original = holes_mod._AxisMaterial.beyond_the_face
    original_span = holes_mod._face_axial_span

    def counting(self, face, t):
        asked.append((face, t))
        return original(self, face, t)

    def counting_span(face, origin, direction):
        built.append(face)
        return original_span(face, origin, direction)

    holes_mod._AxisMaterial.beyond_the_face = counting
    holes_mod._face_axial_span = counting_span
    try:
        per_part, boxes = {}, {}
        for path in sorted(here.glob("*.step")):
            asked.clear()
            built.clear()
            with _silence_stdout_fd():
                shape = _load_step_shape(str(path))
            holes_mod.mine_cylindrical_holes(shape)
            per_part[path.stem] = list(asked)
            boxes[path.stem] = list(built)
    finally:
        holes_mod._AxisMaterial.beyond_the_face = original
        holes_mod._face_axial_span = original_span

    assert per_part["plate_4_through_holes"] == [], (
        "an ordinary plate must not build a bounding box for this question "
        "at all; round 2 built one for every part ever mined"
    )
    every = [call for calls in per_part.values() for call in calls]
    assert len(every) == 134, (
        f"the corpus asks the extent question {len(every)} times, not 134. "
        "That is not a budget — it is one per survivor the void bound "
        "promoted with nothing bounding metal, and a change in it means a "
        "change in which crossings reach the arm"
    )
    every_box = [face for faces in boxes.values() for face in faces]
    assert len(every_box) == 11, (
        f"the corpus BUILDS {len(every_box)} extent boxes, not 11 — one per "
        "part with a rescued cap to refuse. More than one per such part "
        "means the per-face memo has stopped working, which cost 1.5 s on "
        "`nurbs_far_opening_through_bore` before D-19 round 4 added it"
    )
    assert len(boxes["nurbs_far_opening_through_bore"]) == 1, (
        "the part that asks the question 121 times must still build ONE box"
    )
    for face, _t in every:
        assert face.ShapeType() == TopAbs_FACE, (
            "the question must be asked of the FACE that produced the "
            f"crossing, not of {face.ShapeType()}"
        )
    curious = sorted(name for name, calls in per_part.items() if calls)
    assert curious == [
        "angled_far_opening_through_bore",
        "bore_into_fillet",
        "bore_through_a_domed_shoulder",
        "bore_through_torus_wall",
        "cross_drilled_shaft",
        "far_opening_through_bore",
        "far_opening_through_bore_turned",
        "far_opening_through_bore_with_a_leg",
        "nurbs_far_opening_through_bore",
        "spherical_mouth_undercut_bore",
        "spherical_mouth_undercut_bore_with_a_boss",
    ], curious



def _axial_material_root_for_end(
    roots, t_edge, at_low, radius, t_inner, material=None, _face=None
):
    """`_root_for_end` with the refusal asked of the AXIS instead of the face.

    Round 3's rule, with one line changed: a survivor is refused when the
    solid says AIR one band inward and AIR one band outward — "this
    crossing bounds no metal, so it is not a cap" — instead of when it
    lies outside the extent of the face that produced it. It is the
    obvious local replacement for the part's bounding box, it asks the
    authority `is_exit` already asks, and it is WRONG. Kept here so that
    the reason it is wrong is a measurement.
    """
    import aberp_cad_extract.holes as holes_mod

    sign = -1.0 if at_low else 1.0
    if t_inner is None:
        live = list(roots)
    else:
        slack = holes_mod._void_slack(radius)
        live = [root for root in roots if sign * (root[0] - t_inner) >= -slack]
        if material is not None and len(live) != len(roots):
            step = holes_mod._tangency_band(radius)
            exits = [r for r in roots if material.is_exit(r[0], sign, step)]
            if exits:
                return min(exits, key=lambda root: sign * root[0])
            if live:
                live = [
                    root
                    for root in live
                    if material._inside(root[0] - sign * step)
                    or material._inside(root[0] + sign * step)
                ]
                if not live:
                    return None
    return min(live or roots, key=lambda root: abs(root[0] - t_edge))


#: The committed parts an AXIAL material refusal gets wrong, and what it
#: makes of them. Every one is a THROUGH bore whose real cap is the
#: part's own outer surface, which is where the rule breaks.
D19R3_AXIAL_REFUSAL_BREAKS = {
    "bore_into_fillet": 29.802602427879002,
    "bore_through_a_domed_shoulder": 27.937253933193772,
    "bore_through_torus_wall": 15.899748742132001,
    "cross_drilled_shaft": 27.495454169736004,
}


def test_d19r3_an_axial_material_refusal_cannot_do_this_job():
    """WHY the refusal is not a solid probe, measured rather than argued.

    The round-3 report asked for the extent test to be replaced by a
    material determination on the axis — the same authority `is_exit`
    uses one branch earlier, and the obvious thing to reach for. It
    cannot work, and the reason is geometric rather than incidental: at a
    genuine OPEN cap the axis is AIR ON BOTH SIDES. Inward is the bore's
    own hollow and outward is the outside world, so a true cap on a
    through bore is bit-for-bit indistinguishable, to any point query on
    the axis, from a phantom pole floating in space. Every probe within
    one bore radius of the crossing sees the hole.

    So the rule refuses the true cap along with the phantom, and four
    committed parts say so in millimetres — a cross-drilled bar, a bore
    through a fillet, a bore through a torus wall and a bore through a
    domed shoulder, all of which then fall back to the bore's own
    parametric bound and read LONG. Every one of them is exact under the
    face-extent rule that shipped.

    Pinned because "we tried the obvious thing and it was wrong" is a
    claim about the fix, and the next round should not have to rediscover
    it. Note what does NOT move: the crown rescue is a material question
    on the axis and it is the right one, because a floor has metal under
    it. The difference is that a floor bounds metal and an open cap does
    not.
    """
    import aberp_cad_extract.holes as holes_mod

    here = Path(__file__).parent / "fixtures"

    def verdicts():
        out = {}
        for name in sorted(D19R3_AXIAL_REFUSAL_BREAKS):
            with _silence_stdout_fd():
                shape = _load_step_shape(str(here / f"{name}.step"))
            holes = holes_mod.mine_cylindrical_holes(shape)
            out[name] = holes[0].depth_mm if len(holes) == 1 else None
        return out

    shipped = verdicts()
    original = holes_mod._root_for_end
    holes_mod._root_for_end = _axial_material_root_for_end
    try:
        axial = verdicts()
    finally:
        holes_mod._root_for_end = original

    for name, broken in sorted(D19R3_AXIAL_REFUSAL_BREAKS.items()):
        want = COMMITTED_ONE_HOLE.get(name)
        if want is not None:
            assert shipped[name] == pytest.approx(want[0], abs=TOL), (
                f"{name} is not exact under the rule that shipped"
            )
        assert axial[name] == pytest.approx(broken, abs=TOL), (
            f"{name}: an axial material refusal gives {axial[name]}, not the "
            f"{broken} this test was written on. Either the rule changed or "
            "the part did, and the claim it stands for needs re-measuring"
        )
        assert abs(axial[name] - shipped[name]) > TOL, (
            f"{name} did not move, so it is not evidence of anything"
        )

    # ... and the round-2 and round-3 parts stay right under it, which is
    # what makes this a statement about OPEN CAPS and not about the arm
    # being broken in general.
    holes_mod._root_for_end = _axial_material_root_for_end
    try:
        for name in sorted(D19R3_FIXTURES):
            holes = _mine(here / f"{name}.step")
            assert len(holes) == 1, name
            assert holes[0].depth_mm == pytest.approx(
                D19R3_FIXTURES[name][1], abs=TOL
            ), (
                f"{name} is wrong under the axial rule too; this test claims "
                "the axial rule fails on OPEN CAPS specifically"
            )
    finally:
        holes_mod._root_for_end = original


def test_d19r3_the_round_3_parts_are_stable_under_a_reversed_walk(fixtures_dir: Path):
    """S3 on the committed three, through the STEP files themselves."""
    import aberp_cad_extract.holes as holes_mod

    forward = {
        name: _mine(fixtures_dir / f"{name}.step") for name in sorted(D19R3_FIXTURES)
    }
    collect = holes_mod._collect_faces
    holes_mod._collect_faces = lambda shape: list(reversed(collect(shape)))
    try:
        backward = {
            name: _mine(fixtures_dir / f"{name}.step")
            for name in sorted(D19R3_FIXTURES)
        }
    finally:
        holes_mod._collect_faces = collect

    original = holes_mod._cap_axis_intersections
    holes_mod._cap_axis_intersections = lambda face, origin, direction, radius: list(
        reversed(original(face, origin, direction, radius))
    )
    try:
        reordered = {
            name: _mine(fixtures_dir / f"{name}.step")
            for name in sorted(D19R3_FIXTURES)
        }
    finally:
        holes_mod._cap_axis_intersections = original

    show = lambda holes: [
        (repr(h.depth_mm), h.end_condition.name, [repr(v) for v in h.entry_point_mm])
        for h in holes
    ]
    for name in sorted(D19R3_FIXTURES):
        assert show(forward[name]) == show(backward[name]), name
        assert show(forward[name]) == show(reordered[name]), name


#: Every committed fixture that mines exactly one hole, and the depth +
#: end condition it was BUILT from. Used where a test has to say that the
#: answer which survived some rule is the RIGHT answer and not merely a
#: surviving one. Assembled from the round-specific tables rather than
#: re-typed, so it cannot disagree with them.
COMMITTED_ONE_HOLE = dict(
    {name: (spec[1], spec[4]) for name, spec in R7_FIXTURES.items()},
    **{name: (spec[1], spec[4]) for name, spec in D19_FIXTURES.items()},
    **{name: (spec[1], spec[4]) for name, spec in D19R2_FIXTURES.items()},
    **{name: (spec[1], spec[4]) for name, spec in D19R3_FIXTURES.items()},
)


def _corpus_verdicts():
    """``{fixture stem: [(depth, end condition, entry z)]}`` for every
    committed part, as exact reprs — the shape a test compares when its
    claim is "nothing moved"."""
    import aberp_cad_extract.holes as holes_mod

    here = Path(__file__).parent / "fixtures"
    out = {}
    for path in sorted(here.glob("*.step")):
        with _silence_stdout_fd():
            shape = _load_step_shape(str(path))
        out[path.stem] = [
            (repr(h.depth_mm), h.end_condition.name, repr(h.entry_point_mm[2]))
            for h in holes_mod.mine_cylindrical_holes(shape)
        ]
    return out


def test_d19r2_the_dome_floor_family_does_not_depend_on_the_root_order():
    """S3 over the convex floor: OCCT's list order may not reach the answer.

    `GeomAPI_IntCS` hands back a sphere's two poles in whatever order it
    likes, and the round-2 defect was decided by which of them the
    nearest pick was left with. Now the crown wins on being the material
    boundary and the far pole loses on being off the part — neither of
    them on being listed first — so reversing the list, and reversing the
    face walk that reads the mouth bound, both have to leave the family
    bit-identical.
    """
    import aberp_cad_extract.holes as holes_mod

    crowns = (1e-4, 1.2e-3, 0.1, 5.0)
    forward = _domed_verdicts(crowns=crowns)

    original = holes_mod._cap_axis_intersections
    holes_mod._cap_axis_intersections = lambda face, origin, direction, radius: list(
        reversed(original(face, origin, direction, radius))
    )
    try:
        backward = _domed_verdicts(crowns=crowns)
    finally:
        holes_mod._cap_axis_intersections = original
    assert repr(sorted(forward.items())) == repr(sorted(backward.items()))

    collect = holes_mod._collect_faces
    holes_mod._collect_faces = lambda shape: list(reversed(collect(shape)))
    try:
        reversed_walk = _domed_verdicts(crowns=crowns)
    finally:
        holes_mod._collect_faces = collect
    assert repr(sorted(forward.items())) == repr(sorted(reversed_walk.items()))


def test_d19r2_the_round_2_parts_are_stable_under_a_reversed_walk(fixtures_dir: Path):
    """S3 on the committed four, through the STEP files themselves."""
    import aberp_cad_extract.holes as holes_mod

    forward = {
        name: _mine(fixtures_dir / f"{name}.step") for name in sorted(D19R2_FIXTURES)
    }
    collect = holes_mod._collect_faces
    holes_mod._collect_faces = lambda shape: list(reversed(collect(shape)))
    try:
        backward = {
            name: _mine(fixtures_dir / f"{name}.step")
            for name in sorted(D19R2_FIXTURES)
        }
    finally:
        holes_mod._collect_faces = collect

    show = lambda holes: [
        (repr(h.depth_mm), h.end_condition.name, [repr(v) for v in h.entry_point_mm])
        for h in holes
    ]
    for name in sorted(D19R2_FIXTURES):
        assert show(forward[name]) == show(backward[name]), name


# ── D-19 round 4: the bore that does not run down a world axis ───────────
#
# Round 3's re-adversarial left three live over-quotes — an angled bore at
# +83 %, a NURBS-carried breakout cap at +100 %, and a wide spherical
# chamber at +91 % — and named the class rather than the shapes: the
# located-hole rules implicitly assume a WORLD-AXIS-PARALLEL bore, and
# every fixture that reaches the cap refusal has one, so the corpus could
# not see it. Turning the whole part is the cheapest probe that breaks it,
# and under the round-3 rules FOUR of the 58 committed fixtures change
# their answer when the part is rotated — including both of round 2's
# flagship parts, each straight back to the answer round 2 was written to
# fix. See `holes._face_axial_span` and
# `holes.DEGENERATE_ISOLINE_SPAN_MM` for the two mechanisms, and
# `test_d19r4_the_wide_chamber_is_the_undercut_seat_family_not_a_defect`
# for why the third over-quote is not one.

#: name -> (diameter, depth, entry, axis, end condition, flat bottom).
#: Every number derived from the dimensions the part was built from; see
#: the generator docstrings.
D19R4_FIXTURES = {
    "angled_far_opening_through_bore": (
        8.0,
        2.0 + 10.0 / math.cos(math.radians(20.0)),
        (50.0, 50.0 - 10.0 * math.tan(math.radians(20.0)), 20.0),
        (0.0, math.sin(math.radians(20.0)), -math.cos(math.radians(20.0))),
        HoleEndCondition.THROUGH,
        False,
    ),
    # Bit for bit its analytic twin's row in `D19R2_FIXTURES`.
    "nurbs_far_opening_through_bore": D19R2_FIXTURES["far_opening_through_bore"],
}

#: What the miner said on the round-4 parts under the round-3 rules —
#: name -> (depth, end condition). Measured on the committed files by
#: ``test_d19r4_the_before_table_is_measured_and_not_remembered``, never
#: remembered.
D19R4_BEFORE = {
    "angled_far_opening_through_bore": (
        12.0 + 10.0 / math.cos(math.radians(20.0)) + 2.0,
        HoleEndCondition.BLIND,
    ),
    "nurbs_far_opening_through_bore": (24.0, HoleEndCondition.BLIND),
    "far_opening_through_bore_turned": (24.0, HoleEndCondition.BLIND),
}

#: Rigid motions applied to the whole corpus. Rotations about an axis
#: through a point that is on no fixture, so nothing lands back on a
#: world plane by accident, plus translations far from the origin —
#: `GeomAPI_IntCS`'s absolute error grows with the coordinates, and
#: `DEGENERATE_ISOLINE_SPAN_MM` is the floor that has to survive it.
D19R4_MOTIONS = [
    ("rotate", (1.0, 0.0, 0.0), 31.0),
    ("rotate", (0.0, 1.0, 0.0), 17.0),
    ("rotate", (0.0, 0.0, 1.0), 37.0),
    ("rotate", (1.0, 1.0, 1.0), 40.0),
    ("rotate", (1.0, 0.0, 0.0), 90.0),
    ("rotate", (0.0, 1.0, 0.0), 90.0),
    ("rotate", (2.0, -1.0, 3.0), 13.5),
    ("translate", (1000.0, -2500.0, 700.0), 0.0),
    ("translate", (12345.0, 0.0, 0.0), 0.0),
]


def _moved(shape, kind, vector, degrees):
    """``shape`` under one of :data:`D19R4_MOTIONS`."""
    from OCP.BRepBuilderAPI import BRepBuilderAPI_Transform
    from OCP.gp import gp_Ax1, gp_Dir, gp_Pnt, gp_Trsf, gp_Vec

    move = gp_Trsf()
    if kind == "rotate":
        move.SetRotation(
            gp_Ax1(gp_Pnt(3.0, -7.0, 11.0), gp_Dir(*vector)), math.radians(degrees)
        )
    else:
        move.SetTranslation(gp_Vec(*vector))
    return BRepBuilderAPI_Transform(shape, move, True).Shape()


def _rigid_invariants(holes):
    """The part of a mined hole a rigid motion cannot change.

    Diameter, depth, end condition and flat-bottom flag are properties of
    the hole. The entry point and the axis are NOT invariant and are
    deliberately not here: `_canonical_direction` forces the reported
    axis into a canonical hemisphere OF THE WORLD FRAME (ADR-0112 S3), so
    a through hole — whose two ends are both open and equally entitled to
    be called the entry — may report the other one after a rotation. That
    is a documented determinism choice, not a measurement.
    """
    return sorted(
        (round(h.diameter_mm, 9), h.depth_mm, h.end_condition.name, h.flat_bottom)
        for h in holes
    )


def _same_invariants(a, b):
    return len(a) == len(b) and all(
        x[0] == y[0]
        and x[1] == pytest.approx(y[1], abs=TOL)
        and x[2] == y[2]
        and x[3] == y[3]
        for x, y in zip(a, b)
    )


@pytest.mark.parametrize("name", sorted(D19R4_FIXTURES))
def test_d19r4_fixtures_are_exact(fixtures_dir: Path, name):
    """The two round-4 parts that are new geometry, against their dimensions.

    - the ANGLED breakout is a 20°-off-vertical Ø8 bore through a 20 mm
      plate, relieved by the same R=20/3 sphere the upright twin has. Its
      depth is ``2 + 10 / cos 20°`` — the same 12 mm of wall, lengthened
      only by the obliquity — and it is THROUGH.
    - the NURBS breakout is its analytic twin's row, unchanged. One
      ``BRepBuilderAPI_NurbsConvert`` may not move a number.
    """
    diameter, depth, entry, axis, end, flat = D19R4_FIXTURES[name]
    holes = _mine(fixtures_dir / f"{name}.step")
    assert len(holes) == 1, f"{name}: got {[h.diameter_mm for h in holes]}"
    hole = holes[0]
    _approx(hole.diameter_mm, diameter)
    _approx(hole.depth_mm, depth)
    _approx_vec(hole.entry_point_mm, entry)
    _approx_vec(hole.axis_unit, axis)
    assert hole.end_condition is end, name
    assert hole.flat_bottom is flat, name


@pytest.mark.parametrize(
    "name,parent,centre,axis,degrees",
    [
        (
            "far_opening_through_bore_turned",
            "far_opening_through_bore",
            (0.0, 0.0, 0.0),
            (1.0, 0.0, 0.0),
            30.0,
        ),
        (
            "countersunk_blind_bore_turned",
            "countersunk_blind_bore",
            (20.0, 20.0, 10.0),
            (1.0, 1.0, 1.0),
            30.0,
        ),
    ],
)
def test_d19r4_a_turned_part_is_the_same_part(
    fixtures_dir: Path, name, parent, centre, axis, degrees
):
    """A committed part, turned, has the committed part's hole.

    These two fixtures carry no dimensions of their own — they are the
    parent file's geometry under a rigid motion, so their expectations
    are the PARENT's expectations, taken from the same tables, and the
    entry point is the parent's entry through the same rotation.

    The entry is compared as a SET of the bore's two ends rather than as
    one point, because a through hole's two ends are equally entitled to
    be the entry and `_canonical_direction` picks between them in the
    world frame — see :func:`_rigid_invariants`.
    """
    from OCP.gp import gp_Ax1, gp_Dir, gp_Pnt, gp_Trsf

    turn = gp_Trsf()
    turn.SetRotation(gp_Ax1(gp_Pnt(*centre), gp_Dir(*axis)), math.radians(degrees))

    upright = _mine(fixtures_dir / f"{parent}.step")
    turned = _mine(fixtures_dir / f"{name}.step")
    assert _same_invariants(_rigid_invariants(upright), _rigid_invariants(turned)), (
        f"{name}: a rigid motion changed the hole.\n"
        f"  upright {_rigid_invariants(upright)}\n"
        f"  turned  {_rigid_invariants(turned)}"
    )

    assert len(upright) == len(turned) == 1
    ends = []
    for sign in (0.0, upright[0].depth_mm):
        point = tuple(
            upright[0].entry_point_mm[i] + sign * upright[0].axis_unit[i]
            for i in range(3)
        )
        moved = gp_Pnt(*point).Transformed(turn)
        ends.append((moved.X(), moved.Y(), moved.Z()))
    assert any(
        all(
            got == pytest.approx(want, abs=TOL)
            for got, want in zip(turned[0].entry_point_mm, end)
        )
        for end in ends
    ), f"{name}: entry {tuple(turned[0].entry_point_mm)} is neither turned end {ends}"


def test_d19r4_the_before_table_is_measured_and_not_remembered(fixtures_dir: Path):
    """:data:`D19R4_BEFORE`, taken from the kernel with round 3 restored.

    The extent question put back to `_round3_beyond_the_face` — the right
    OBJECT with the wrong RULER — on the three parts whose bore is not
    upright. Each comes back to the over-quote round 4 was written to
    close, and each of those over-quotes is longer than the material it
    is cut from: 24 mm and 24.64 mm of hole in a 20 mm plate.
    """
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._AxisMaterial.beyond_the_face
    holes_mod._AxisMaterial.beyond_the_face = _round3_beyond_the_face
    try:
        before = {
            name: _mine(fixtures_dir / f"{name}.step") for name in sorted(D19R4_BEFORE)
        }
    finally:
        holes_mod._AxisMaterial.beyond_the_face = original

    for name, (depth, end) in sorted(D19R4_BEFORE.items()):
        holes = before[name]
        assert len(holes) == 1, f"{name}: got {len(holes)} holes under round 3"
        assert holes[0].depth_mm == pytest.approx(depth, abs=1e-4), (
            f"{name}: round 3 measured {holes[0].depth_mm}, table says {depth}"
        )
        assert holes[0].end_condition is end, (
            f"{name}: round 3 said {holes[0].end_condition}, table says {end}"
        )
        assert holes[0].depth_mm > 20.0, (
            f"{name}: the round-3 answer {holes[0].depth_mm} is not longer than "
            "the 20 mm plate, so this part is not showing the defect"
        )

    # ... and round 4 answers all three correctly, so the table is a
    # BEFORE and not a standing description.
    for name in sorted(D19R4_BEFORE):
        now = _mine(fixtures_dir / f"{name}.step")
        assert len(now) == 1 and now[0].depth_mm < 20.0, (name, now)
        assert now[0].end_condition is HoleEndCondition.THROUGH, name


def test_d19r4_the_corpus_is_invariant_under_rotation(fixtures_dir: Path):
    """Every fixture, under every motion in :data:`D19R4_MOTIONS`.

    This is the probe that found the round: a hole is a property of the
    part and a rigid motion cannot change one, so anything that moves is
    a world-axis assumption inside the miner. Under the round-3 rules
    four of the 58 fixtures moved — ``far_opening_through_bore`` and its
    leg twin to 24.0 and BLIND, ``spherical_mouth_undercut_bore`` and its
    boss twin to 13.188 and UNKNOWN, which are precisely round 2's two
    headline defects — and the two countersinks moved under a
    translation. Nothing moves now.

    Deliberately over the WHOLE corpus rather than a chosen few: the
    point of the round is that the assumption was invisible to a corpus
    of upright parts, and a rotation sweep over three fixtures would be
    the same mistake one size smaller.
    """
    for path in sorted(fixtures_dir.glob("*.step")):
        with _silence_stdout_fd():
            shape = _load_step_shape(str(path))
            upright = _rigid_invariants(mine_cylindrical_holes(shape))
            for kind, vector, degrees in D19R4_MOTIONS:
                moved = _rigid_invariants(
                    mine_cylindrical_holes(_moved(shape, kind, vector, degrees))
                )
        assert _same_invariants(upright, moved), (
            f"{path.stem} under {kind} {vector} {degrees}:\n"
            f"  upright {upright}\n"
            f"  moved   {moved}"
        )


def test_d19r4_a_nurbs_cap_measures_as_its_analytic_twin(fixtures_dir: Path):
    """One ``NurbsConvert`` may not change a hole. Two pairs, both ends.

    ``BRepBndLib::Add`` bounds a B-spline by its POLES, and a NURBS
    sphere's poles stand outside the sphere — so the round-3 extent
    question gave the same geometry two different answers depending only
    on which representation the exporting CAD system happened to write.
    The breakout pair is the one that showed it as a wrong number; the
    dome pair is the one where the round-3 rule was accidentally LOOSER
    on the NURBS side, which is the same defect facing the other way and
    is why the pin is taken on both.
    """
    from OCP.BRepAdaptor import BRepAdaptor_Surface
    from OCP.GeomAbs import GeomAbs_SurfaceType

    import aberp_cad_extract.holes as holes_mod

    def cap_span(path, origin, direction, rule):
        """``rule`` applied to the one CURVED face of the part."""
        with _silence_stdout_fd():
            shape = _load_step_shape(str(path))
        curved = [
            face
            for face in holes_mod._collect_faces(shape)
            if BRepAdaptor_Surface(face).GetType()
            not in (
                GeomAbs_SurfaceType.GeomAbs_Plane,
                GeomAbs_SurfaceType.GeomAbs_Cylinder,
            )
        ]
        assert len(curved) == 1, (path.stem, len(curved))
        return rule(curved[0], origin, direction)

    for analytic, nurbs in (
        ("far_opening_through_bore", "nurbs_far_opening_through_bore"),
        ("bore_through_spherical_dome", "bore_through_nurbs_dome"),
    ):
        left = _mine(fixtures_dir / f"{analytic}.step")
        right = _mine(fixtures_dir / f"{nurbs}.step")
        assert _same_invariants(
            _rigid_invariants(left), _rigid_invariants(right)
        ), (
            f"{analytic} and {nurbs} are the same geometry and measured "
            f"differently:\n  {_rigid_invariants(left)}\n  {_rigid_invariants(right)}"
        )

        # ... and the reason, one level down: the extent question itself
        # now gives the twins the same answer, where the world box did
        # not. This is the mutation proof for the dome pair, whose mined
        # depth was already the same under both rules.
        origin = tuple(left[0].entry_point_mm)
        direction = tuple(left[0].axis_unit)
        patch = [
            cap_span(fixtures_dir / f"{name}.step", origin, direction, holes_mod._face_axial_span)
            for name in (analytic, nurbs)
        ]
        world = [
            cap_span(fixtures_dir / f"{name}.step", origin, direction, _world_box_axial_span)
            for name in (analytic, nurbs)
        ]
        assert patch[0] == pytest.approx(patch[1], abs=1e-4), (
            f"{analytic} / {nurbs}: the surface-patch extent must not depend "
            f"on the representation; got {patch}"
        )
        assert world[0] != pytest.approx(world[1], abs=1e-4), (
            f"{analytic} / {nurbs}: the world box no longer differs between "
            f"the twins ({world}), so this pair has stopped witnessing the "
            "defect and a new one is needed"
        )


def test_d19r4_the_extent_is_the_patch_not_the_trim(fixtures_dir: Path):
    """Why the box is taken of the SURFACE and not of the trimmed face.

    A tight box of the trimmed face is the obvious way to make the extent
    question orientation-free, and it is wrong for the reason
    ``test_r4_an_on_face_trim_test_would_have_re_broken_the_domes``
    already gives from the other side: a doubly-curved CONVEX face's
    material boundary lies outside its own trim curve, because the bore
    cuts a disc out of the middle of the very crown it is measured
    against.

    ``bore_through_torus_wall`` is where that bites, and it is the only
    fixture in the corpus whose crown actually reaches the refusal. Its
    convex outer wall crowns at x=20 while the trim stops at 19.8997, so
    a trimmed-face box refuses the true cap and the bore reads 15.8997
    against a true 16.0. Both numbers are asserted, so neither the
    regression nor the margin can drift silently.
    """
    from OCP.Bnd import Bnd_Box
    from OCP.BRep import BRep_Tool
    from OCP.BRepBndLib import BRepBndLib
    from OCP.BRepBuilderAPI import BRepBuilderAPI_Transform
    from OCP.gp import gp_Ax3, gp_Dir, gp_Pnt, gp_Trsf

    import aberp_cad_extract.holes as holes_mod

    def trimmed_face_span(face, origin, direction):
        frame = gp_Trsf()
        frame.SetTransformation(gp_Ax3(gp_Pnt(*origin), gp_Dir(*direction)))
        moved = BRepBuilderAPI_Transform(face, frame, True).Shape()
        box = Bnd_Box()
        BRepBndLib.AddOptimal_s(moved, box, True, True)
        if box.IsVoid():
            return None
        _x0, _y0, t_lo, _x1, _y1, t_hi = box.Get()
        return t_lo, t_hi

    original = holes_mod._face_axial_span
    holes_mod._face_axial_span = trimmed_face_span
    try:
        holes = _mine(fixtures_dir / "bore_through_torus_wall.step")
    finally:
        holes_mod._face_axial_span = original

    assert len(holes) == 1, holes
    assert holes[0].depth_mm == pytest.approx(15.899748742, abs=1e-6), (
        "a trimmed-face box no longer shortens the torus wall. Either OCCT "
        "changed or the fixture did; the patch-vs-trim choice needs a new "
        f"witness either way. Got {holes[0].depth_mm}"
    )
    assert _mine(fixtures_dir / "bore_through_torus_wall.step")[0].depth_mm == (
        pytest.approx(16.0, abs=TOL)
    )


def _collapse_by_derivative(floor_of):
    """`_crossing_normal` as rounds 3 and 4 shipped it, with the
    collapsed-isoline test left as a parameter.

    Both of those rounds decided COLLAPSED from the two DERIVATIVE
    magnitudes at the root, and differed only in what they compared them
    against: round 3 took their RATIO against 1e-9, round 4 took the
    smaller of them as a LENGTH against 1e-4 mm. ``floor_of(mag_u, mag_v)``
    is that difference and nothing else is, so one replica carries both
    and the claims about each stay checkable side by side.

    What neither did — and what makes the replica a replica only when it
    is driven through :func:`_pre_r5_cap_axis_intersections` — is run the
    intersection in the bore's frame. See that function.
    """

    def rule(surface, u, v, direction, _radius):
        from OCP.GeomLProp import GeomLProp_SLProps

        import aberp_cad_extract.holes as holes_mod

        props = GeomLProp_SLProps(surface, u, v, 1, 1e-7)
        d_u, d_v = props.D1U(), props.D1V()
        mag_u = math.sqrt(d_u.X() ** 2 + d_u.Y() ** 2 + d_u.Z() ** 2)
        mag_v = math.sqrt(d_v.X() ** 2 + d_v.Y() ** 2 + d_v.Z() ** 2)
        floor_u, floor_v = floor_of(mag_u, mag_v)

        u_min, u_max, v_min, v_max = surface.Bounds()
        if mag_u <= floor_u:
            lo, hi, along_u = u_min, u_max, True
        elif mag_v <= floor_v:
            lo, hi, along_u = v_min, v_max, False
        else:
            if not props.IsNormalDefined():
                return None
            n = props.Normal()
            normal = holes_mod._unit((float(n.X()), float(n.Y()), float(n.Z())))
            return (
                normal
                if abs(holes_mod._dot(direction, normal))
                > holes_mod.CAP_OUTWARD_MIN_COS
                else None
            )

        if not (math.isfinite(lo) and math.isfinite(hi)) or hi <= lo:
            return None

        w_pole, w_min, w_max = (v, v_min, v_max) if along_u else (u, u_min, u_max)
        normal = holes_mod._degenerate_point_normal(
            surface, u, v, along_u, lo, hi, w_pole, w_min, w_max
        )
        if normal is None:
            return None
        return (
            normal
            if abs(holes_mod._dot(direction, normal)) > holes_mod.CAP_OUTWARD_MIN_COS
            else None
        )

    return rule


#: Round 3's rule: a RATIO between two derivatives that are not
#: commensurable, at 1e-9.
_round3_crossing_normal = _collapse_by_derivative(
    lambda mag_u, mag_v: (1e-9 * mag_v, 1e-9 * mag_u)
)

#: Round 4's rule: the smaller derivative as an absolute LENGTH, at
#: 1e-4 mm. Commensurable at last, and still extrinsic — the length it is
#: compared against is fixed while the root it is computed from is placed
#: by an intersector whose absolute error grows with the part's world
#: coordinates. That is D-19 round 5, blocker 1.
_round4_crossing_normal = _collapse_by_derivative(lambda _u, _v: (1e-4, 1e-4))


def _pre_r5_cap_axis_intersections(collapse_rule):
    """`_cap_axis_intersections` as rounds 3 and 4 shipped it: the axis
    intersected with the cap's surface IN THE WORLD FRAME.

    This is the half of the historical path that cannot be replayed by
    monkeypatching :func:`_crossing_normal` alone, and leaving it out
    would quietly rewrite what those rounds did. ``GeomAPI_IntCS``'s
    absolute error grows with the coordinates it is handed, so where the
    root lands — and therefore what the collapse test is even shown — is
    a function of where the part sits in the world. Drive round 3's or
    round 4's collapse rule through the round-5 in-frame intersection and
    it is handed a root on the apex to a nanometre, and both rules answer
    correctly on parts they demonstrably got wrong.
    """
    from OCP.BRep import BRep_Tool
    from OCP.Geom import Geom_Line
    from OCP.GeomAbs import GeomAbs_SurfaceType
    from OCP.GeomAPI import GeomAPI_IntCS
    from OCP.gp import gp_Ax1, gp_Dir, gp_Pnt

    import aberp_cad_extract.holes as holes_mod

    def intersections(face, origin, direction, radius):
        planar = holes_mod._plane_axis_intersection(face, origin, direction)
        if planar is not None:
            return [planar]
        if holes_mod._adaptor(face).GetType() == GeomAbs_SurfaceType.GeomAbs_Plane:
            return []
        surface = BRep_Tool.Surface_s(face)
        if surface is None:
            return []
        line = Geom_Line(
            gp_Ax1(
                gp_Pnt(origin[0], origin[1], origin[2]),
                gp_Dir(direction[0], direction[1], direction[2]),
            )
        )
        intersector = GeomAPI_IntCS(line, surface)
        if not intersector.IsDone():
            return []
        roots = []
        for i in range(1, intersector.NbPoints() + 1):
            u, v, _w = intersector.Parameters(i)
            normal = collapse_rule(surface, u, v, direction, radius)
            if normal is None:
                continue
            p = intersector.Point(i)
            roots.append(
                (
                    holes_mod._dot(
                        (
                            float(p.X()) - origin[0],
                            float(p.Y()) - origin[1],
                            float(p.Z()) - origin[2],
                        ),
                        direction,
                    ),
                    normal,
                )
            )
        return roots

    return intersections


@contextlib.contextmanager
def _as_shipped(collapse_rule):
    """Run the miner on a pre-round-5 crossing path."""
    import aberp_cad_extract.holes as holes_mod

    original = holes_mod._cap_axis_intersections
    holes_mod._cap_axis_intersections = _pre_r5_cap_axis_intersections(collapse_rule)
    try:
        yield
    finally:
        holes_mod._cap_axis_intersections = original


def test_d19r4_a_collapsed_isoline_is_a_length_not_a_ratio(fixtures_dir: Path):
    """The countersink's apex, and the floor that only an upright part met.

    A countersink's cone is swept about the bore's OWN axis, so the axis
    meets it at exactly one point — the apex — which it touches without
    crossing. `_crossing_normal` refuses that root by recognising the
    collapsed isoline it sits on, and round 3 recognised it with a RATIO
    between two derivative magnitudes that are not commensurable. On a
    surface of revolution that reduces to "is the root within a nanometre
    of the apex", which is a question only a part whose bore runs down a
    world axis answers yes to.

    Three things are pinned here, and the first is the one that matters:

    1. Under the round-3 rule the answer is BISTABLE in the rotation
       angle — right at 0°, wrong at 1°, right at 5° and 45°, wrong at
       135° and 180°. An answer that is not monotone in the part's
       orientation is not a property of the part.
    2. A TRANSLATION does it too, which is the mechanism stated plainly:
       the intersector's absolute error grows with the coordinates and
       the floor it was compared against did not. The same part moved
       1 m along X reads 7.000008 against a true 11.0 — an UNDER-quote,
       the direction the module treats as the worse one because it is
       invisible in the reasoning log.
    3. The new floor is a length with the margin measured, not a tuned
       number, so the answer is flat everywhere between — walked here
       decade by decade.

    Round 5 kept every one of those findings and corrected the third's
    ARITHMETIC, which round 4 recorded from too small a sample: the gap
    was not 2.4e-07 against 1.0 mm. Composing a rotation WITH a
    translation puts the worst collapsed isoline at 1.13e-04 mm, above
    the 1e-4 floor meant to catch it, and that is blocker 1 of round 5 —
    see ``test_d19r5_the_collapsed_isoline_gap_is_measured_not_claimed``,
    which measures both sides of the gap rather than asserting them.
    """
    import aberp_cad_extract.holes as holes_mod

    with _silence_stdout_fd():
        upright = _load_step_shape(str(fixtures_dir / "countersunk_blind_bore.step"))

    def depth(shape):
        with _silence_stdout_fd():
            holes = mine_cylindrical_holes(shape)
        assert len(holes) == 1, holes
        return holes[0].depth_mm

    turns = [0.0, 1.0, 5.0, 45.0, 89.9, 90.0, 135.0, 180.0]
    with _as_shipped(_round3_crossing_normal):
        was = {
            deg: depth(_moved(upright, "rotate", (0.0, 1.0, 0.0), deg))
            for deg in turns
        }
        was_far = depth(_moved(upright, "translate", (1000.0, 0.0, 0.0), 0.0))

    wrong = sorted(deg for deg, d in was.items() if abs(d - 11.0) > 1e-3)
    right = sorted(deg for deg, d in was.items() if abs(d - 11.0) <= 1e-3)
    assert wrong and right, (
        f"round 3 answered the turned countersink consistently: {was}. The "
        "bistability IS the finding, so this sweep is no longer showing it"
    )
    for deg in wrong:
        assert was[deg] == pytest.approx(7.0, abs=1e-3), (deg, was[deg])
    assert was_far == pytest.approx(7.0, abs=1e-3), was_far

    # Round 4 answers every one of them, and the same part 1 m away.
    for deg in turns:
        assert depth(_moved(upright, "rotate", (0.0, 1.0, 0.0), deg)) == (
            pytest.approx(11.0, abs=TOL)
        ), deg
    assert depth(_moved(upright, "translate", (1000.0, 0.0, 0.0), 0.0)) == (
        pytest.approx(11.0, abs=TOL)
    )

    # And the floor is flat across the whole measured gap. Round 5 made
    # the floor a FRACTION of the bore's radius rather than a millimetre
    # figure, so the decades walked here are decades of that fraction —
    # on a Ø8 bore, 4e-14 mm to 4e-4 mm.
    floor = holes_mod.DEGENERATE_ISOLINE_FRACTION
    try:
        for exponent in range(-14, -3):
            holes_mod.DEGENERATE_ISOLINE_FRACTION = 10.0**exponent
            for deg in (0.0, 1.0, 135.0):
                assert depth(_moved(upright, "rotate", (0.0, 1.0, 0.0), deg)) == (
                    pytest.approx(11.0, abs=TOL)
                ), (exponent, deg)
    finally:
        holes_mod.DEGENERATE_ISOLINE_FRACTION = floor


def _undercut_ball_seat(bore_radius, undercut, centre_z=12.0):
    """`tools/generate_step_fixtures.py::_undercut_ball_seat`, in memory.

    A blind bore ended by a spherical cavity WIDER than the bore, cut
    from a 40 x 40 x 20 block. ``undercut = 0`` is a ball-nose pocket;
    ``undercut = 0.1`` is the committed ``undercut_ball_seat_blind_bore``;
    larger values are the same part with a wider cavity.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCylinder,
        BRepPrimAPI_MakeSphere,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    bored = BRepAlgoAPI_Cut(
        block,
        BRepPrimAPI_MakeCylinder(
            gp_Ax2(gp_Pnt(20.0, 20.0, centre_z), gp_Dir(0, 0, 1)), bore_radius, 30.0
        ).Shape(),
    ).Shape()
    seat = BRepPrimAPI_MakeSphere(
        gp_Pnt(20.0, 20.0, centre_z), bore_radius + undercut
    ).Shape()
    return BRepAlgoAPI_Cut(bored, seat).Shape()


def test_d19r4_the_wide_chamber_is_the_undercut_seat_family_not_a_defect():
    """The reported "+91 % over-quote" that is the DECIDED convention.

    Round 3's re-adversarial reported a Ø4 access hole into a R8
    spherical chamber as a +91 % over-quote: 33.0 against a
    solid-derived 17.254. It is the same part as the committed
    ``undercut_ball_seat_blind_bore`` — literally
    :func:`_undercut_ball_seat` with a bigger ``undercut`` — and the
    miner answers both by the SAME rule: the bore ends at the cavity's
    far pole, because that is where the tool travelling down the axis
    first meets metal.

    That rule is what D-19 round 1 was written to install, and the
    committed fixture pins it at ``undercut = 0.1``. The oracle that
    calls 33.0 an over-quote measures a bore's end at the last place its
    own WALL is the boundary, and by that measure the committed fixture
    is wrong too — 6.9005 against its pinned 14.1, a 51 % disagreement on
    a part nobody has called defective.

    So the two were never a defect and a correct answer sitting side by
    side. They are ONE semantic, applied consistently, that the corpus
    and the oracle disagreed about:

      "how deep is a hole that opens into a cavity wider than itself" —
      as far as the tool travels (the corpus), or as far as the hole's
      own wall reaches (the oracle)?

    THAT QUESTION IS NOW RULED. Backlog D-19 item 8 is DECIDED = A:
    depth is TOOL TRAVEL, to the deepest point the tool reaches — the
    corpus's reading — on the conservative ground that between two
    defensible answers we take the one that never under-quotes. Nothing
    here moves, because nothing here was ever wrong; what changes is that
    this family is WORKING AS INTENDED rather than an open over-quote,
    and an oracle measuring to the wall now disagrees with a ratified
    convention rather than reporting a defect.

    The sweep below is the evidence that A measures rather than invents.
    The miner's answer is exactly "to the pole" at every undercut from a
    ball nose to a chamber, continuous and with no feature anywhere along
    it to hang a rule on, so every candidate rule that would have closed
    the chamber while keeping the committed fixture was a THRESHOLD on
    the undercut — a boundary put in to make one part agree with an
    oracle. It stays pinned as a family for that reason: it is what would
    catch a later round quietly reintroducing such a threshold.

    NOT a rotation defect either, which is the other half of what this
    pins: the chamber answers the same at every orientation, so it is
    genuinely outside what rounds 4 and 5 are about.
    """
    for bore_radius, undercut in (
        (6.0, 0.0),
        (6.0, 0.1),
        (6.0, 1.0),
        (6.0, 2.0),
        (4.0, 0.1),
        (2.0, 6.0),
    ):
        shape = _undercut_ball_seat(bore_radius, undercut)
        with _silence_stdout_fd():
            holes = mine_cylindrical_holes(shape)
        assert len(holes) == 1, (bore_radius, undercut, len(holes))
        # The pole is `undercut` below the sphere's centre plus the bore's
        # own radius; the bore is entered from the plate's top face.
        pole = 12.0 - (bore_radius + undercut)
        assert holes[0].depth_mm == pytest.approx(20.0 - pole, abs=TOL), (
            f"r={bore_radius} undercut={undercut}: the family answers to the "
            f"pole at every other size and {holes[0].depth_mm} here. A "
            "discontinuity in this sweep would BE the geometric feature a "
            "rule could be hung on, and there is none"
        )
        assert holes[0].end_condition is HoleEndCondition.BLIND

    # The committed fixture is the undercut = 0.1 member of that sweep,
    # and the chamber the re-adversarial reported is the undercut = 6.0
    # member. One rule, one family.
    assert D19_FIXTURES["undercut_ball_seat_blind_bore"][1] == pytest.approx(
        20.0 - (12.0 - 6.1), abs=TOL
    )

    # And it does not move with the part's orientation, so it is not a
    # round-4 defect wearing a round-4 disguise.
    chamber = _undercut_ball_seat(2.0, 6.0)
    with _silence_stdout_fd():
        upright = _rigid_invariants(mine_cylindrical_holes(chamber))
        for kind, vector, degrees in D19R4_MOTIONS:
            moved = _rigid_invariants(
                mine_cylindrical_holes(_moved(chamber, kind, vector, degrees))
            )
    assert _same_invariants(upright, moved), (upright, moved)


def _drill_point_bore(z_tip, included_deg, fused, bore_radius=4.0, plate=20.0):
    """A blind bore ended by a DRILL POINT — a coaxial cone, apex down.

    ``fused`` cuts the shaft and the point as one cutter; the other arm
    cuts them one after the other. Backlog D-19 item 4 recorded the
    defect as topology-dependent, so both are built.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut, BRepAlgoAPI_Fuse
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCone,
        BRepPrimAPI_MakeCylinder,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    height = bore_radius / math.tan(math.radians(included_deg / 2.0))
    shoulder = z_tip + height
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, plate).Shape()
    shaft = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(20.0, 20.0, shoulder), gp_Dir(0, 0, 1)),
        bore_radius,
        plate - shoulder + 5.0,
    ).Shape()
    point = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(20.0, 20.0, z_tip), gp_Dir(0, 0, 1)), 0.0, bore_radius, height
    ).Shape()
    if fused:
        cutter = BRepAlgoAPI_Fuse(shaft, point).Shape()
    else:
        block = BRepAlgoAPI_Cut(block, shaft).Shape()
        cutter = point
    return BRepAlgoAPI_Cut(block, cutter).Shape(), plate - shoulder


def test_d19r4_backlog_item_4_the_drill_point_apex_is_closed():
    """Backlog D-19 item 4, closed by the same constant as the countersink.

    Item 4 recorded the 118° drill point's APEX being admitted as a cap —
    an OVER-quote by the point length, ~0.3·D — and named
    ``DEGENERATE_ISOLINE_RATIO`` as the cause and "raising the ratio to
    ~1e-6, which needs its own regression sweep" as the candidate fix.

    The round-4 diagnosis is sharper than "the ratio is too tight" and it
    changes what the fix is: the quantity is not a ratio at all. It is
    the root's own distance from the axis in millimetres, being compared
    against a number that only reads as dimensionless because a cone's
    other derivative happens to be 1. So the fix is to compare it against
    a LENGTH, and this is the
    regression sweep item 4 asked for: two topologies × three included
    angles × five point positions, with the depth asserted to the bore's
    own SHOULDER every time. Round 5 kept the fix and moved the length off
    the millimetre scale and onto the bore's own radius
    (:data:`holes.DEGENERATE_ISOLINE_FRACTION`), which does not change what
    this sweep shows — and it also moved the intersection into the bore's
    frame, which is why round 3's rule has to be replayed through
    :func:`_as_shipped` to be replayed at all.

    Under the round-3 ratio exactly two of those thirty end at the apex,
    both of them the 118° point at z=4: 16.0 against a true 13.5966,
    +17.7 %. Under the round-4 length, none do.
    """
    import aberp_cad_extract.holes as holes_mod

    grid = [
        (z_tip, included, fused)
        for fused in (True, False)
        for included in (118.0, 135.0, 90.0)
        for z_tip in (4.0, 5.0, 6.0, 7.0, 8.0)
    ]

    def sweep():
        out = {}
        for z_tip, included, fused in grid:
            shape, depth = _drill_point_bore(z_tip, included, fused)
            with _silence_stdout_fd():
                holes = mine_cylindrical_holes(shape)
            assert len(holes) == 1, (z_tip, included, fused, len(holes))
            out[(z_tip, included, fused)] = (holes[0].depth_mm, depth, 20.0 - z_tip)
        return out

    with _as_shipped(_round3_crossing_normal):
        was = sweep()
    at_apex = sorted(
        key for key, (got, _wall, apex) in was.items() if abs(got - apex) <= TOL
    )
    assert at_apex == [(4.0, 118.0, False), (4.0, 118.0, True)], (
        "the round-3 ratio no longer admits the 118° point's apex, so this "
        f"sweep is not showing backlog item 4 any more; got {at_apex}"
    )
    shoulder_118 = 20.0 - 4.0 - 4.0 / math.tan(math.radians(59.0))
    for key in at_apex:
        got, wall, _apex = was[key]
        assert got == pytest.approx(16.0, abs=TOL), (key, was[key])
        assert wall == pytest.approx(shoulder_118, abs=TOL), (key, was[key])
        assert got / wall > 1.17, (
            f"{key}: the over-quote is the point length, ~0.3 diameters — "
            f"{got} against {wall}"
        )

    now = sweep()
    for key, (got, wall, apex) in sorted(now.items()):
        assert got == pytest.approx(wall, abs=TOL), (
            f"{key}: a drill point's apex is not a cap — the bore ends at its "
            f"own shoulder, {wall}. Got {got} (the apex is at {apex})"
        )


# ── D-19 round 5: the world-frame class, closed ────────────────────────
#
# Round 4 named this branch after the defect and did not finish it. It
# replaced TWO world-frame quantities — `_face_axial_span`'s box and the
# collapsed-isoline ratio — and left others standing, so the adversarial
# came back with two more instances of the same class. What follows is
# the class, not the instances: any geometric verdict that depends on
# where the part sits in the world instead of on the part's own shape is
# a bug, because a located hole is a property of the part.
#
# Two things about these probes are deliberate, and both are what round 4
# was missing rather than what it got wrong:
#
# - the motions COMPOSE. Round 4 applied one rotation OR one translation
#   at a time, and every one of them passed. A rotation puts the bore's
#   axis off the world axes; a translation then makes the surface's
#   coordinates large IN THE DIRECTIONS THE ROTATION OPENED UP. Neither
#   half does that alone, and it is the product that broke the
#   countersink.
# - the gap is MEASURED. Round 4's docstring recorded a margin of "three
#   and a half orders" from a sample that never composed the two motions.
#   The real worst case is 1.10x of the floor — on the wrong side of it —
#   and no assertion in the suite would have said so, because the margin
#   lived in prose. `test_d19r5_the_collapsed_isoline_gap_is_measured_
#   not_claimed` computes both sides of it.


D19R5_CORPUS = {
    'angled_blind_hole': (
        (
            8.0,
            20.00000000000007,
            (20.0, 30.0, 40.0),
            (0.7071067811865476, 0.0, -0.7071067811865476),
            'BLIND',
            True,
        ),
    ),
    'angled_far_opening_through_bore': (
        (
            8.0,
            12.641777724762413,
            (50.0, 46.36029765732854, 20.0),
            (0.0, 0.34202014332593184, -0.9396926207858127),
            'THROUGH',
            False,
        ),
    ),
    'angled_through_hole': (
        (
            8.0,
            23.094010767587953,
            (31.547005383798364, 30.0, 0.0),
            (0.50000000000019, 0.0, 0.866025403784329),
            'THROUGH',
            False,
        ),
    ),
    'assembly_two_solids': (
    ),
    'ball_nose_blind_bore': (
        (
            8.0,
            16.0,
            (20.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'ball_nose_blind_bore_d4_deep': (
        (
            4.0,
            16.26015079999996,
            (20.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'ball_nose_blind_bore_d6': (
        (
            6.0,
            9.799999999999997,
            (20.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'blind_bore_beside_chamfered_edge': (
        (
            8.0,
            12.0,
            (32.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'blind_bore_beside_two_chamfers_corner': (
        (
            8.0,
            12.0,
            (32.0, 32.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'blind_bore_straddling_a_rounded_edge': (
        (
            10.0,
            12.0,
            (30.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'blind_bore_under_dome': (
        (
            10.0,
            12.0,
            (0.0, 0.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'blind_hole_curved_top': (
        (
            10.0,
            17.9128784747792,
            (10.0, 20.0, 22.9128784747792),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'blind_hole_drill_point': (
        (
            8.0,
            25.0,
            (20.0, 20.0, 40.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'blind_hole_flat_bottom': (
        (
            10.0,
            18.0,
            (20.0, 20.0, 30.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'bore_beside_a_conical_boss': (
        (
            8.0,
            24.99999999999276,
            (38.5, 22.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_beside_a_taller_conical_boss': (
        (
            8.0,
            28.75000000001998,
            (38.5, 22.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_beside_chamfered_edge': (
        (
            8.0,
            20.0,
            (32.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_beside_concave_corner_fillet': (
        (
            14.0,
            20.0,
            (21.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_beside_concave_fillet': (
        (
            8.0,
            30.0,
            (0.0, 20.0, 25.0),
            (1.0, 0.0, 0.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_beside_two_chamfers_corner': (
        (
            8.0,
            20.0,
            (32.0, 32.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_beside_uneven_chamfer_corner': (
        (
            8.0,
            20.0,
            (32.0, 32.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_inside_a_chamfer': (
        (
            8.0,
            14.000000000000002,
            (32.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_into_fillet': (
        (
            6.0,
            28.66025403784439,
            (55.0, 30.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_on_a_chamfer_corner_boundary': (
        (
            8.0,
            20.0,
            (32.0, 34.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_over_centre_post': (
        (
            30.0,
            20.0,
            (30.0, 30.0, 40.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'bore_straddling_a_concave_fillet': (
        (
            6.0,
            20.34314575050762,
            (36.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_straddling_a_rounded_edge': (
        (
            10.0,
            20.0,
            (30.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_through_a_domed_shoulder': (
        (
            8.0,
            27.416198487095663,
            (37.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_through_nurbs_dome': (
        (
            8.0,
            39.999999999999986,
            (0.0, 0.0, -19.999999999999996),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_through_spherical_dome': (
        (
            8.0,
            40.0,
            (0.0, 0.0, -20.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'bore_through_torus_wall': (
        (
            4.0,
            16.0,
            (4.0, 0.0, 0.0),
            (1.0, 0.0, 0.0),
            'THROUGH',
            False,
        ),
    ),
    'both_sides_drilled': (
        (
            8.0,
            40.0,
            (20.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'chamfered_mouth_bore': (
        (
            8.0,
            18.5,
            (20.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'coaxial_split_faces': (
        (
            9.0,
            40.0,
            (15.0, 15.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'concave_fillet_step': (
    ),
    'countersunk_blind_bore': (
        (
            8.0,
            11.0,
            (20.0, 20.0, 17.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'countersunk_blind_bore_turned': (
        (
            8.0,
            11.000000000000137,
            (22.33333333333058, 18.291881449008574, 16.37478521766258),
            (-0.3333333333330485, 0.24401693585603548, -0.9106836025231323),
            'BLIND',
            True,
        ),
    ),
    'countersunk_bore_120': (
        (
            8.0,
            18.267949192431,
            (20.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'countersunk_through_bore': (
        (
            8.0,
            17.0,
            (20.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'cross_drilled_shaft': (
        (
            8.0,
            22.360679774997898,
            (-11.180339887498947, 10.0, 30.0),
            (1.0, 0.0, 0.0),
            'THROUGH',
            False,
        ),
    ),
    'domed_floor_pocket': (
        (
            12.0,
            11.998800000000976,
            (30.0, 30.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'domed_floor_pocket_proud': (
        (
            12.0,
            6.999999999999968,
            (30.0, 30.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'domed_floor_pocket_with_a_rib': (
        (
            12.0,
            11.998800000000976,
            (30.0, 30.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'far_opening_through_bore': (
        (
            8.0,
            12.0,
            (30.0, 30.0, 8.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'far_opening_through_bore_turned': (
        (
            8.0,
            12.00000000000653,
            (30.0, 15.980762113524985, 32.320508075691876),
            (0.0, 0.50000000000019, -0.866025403784329),
            'THROUGH',
            False,
        ),
    ),
    'far_opening_through_bore_with_a_leg': (
        (
            8.0,
            12.0,
            (30.0, 30.0, 8.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'filleted_block': (
    ),
    'nurbs_far_opening_through_bore': (
        (
            8.0,
            12.0,
            (30.0, 30.0, 8.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'plate_4_through_holes': (
        (
            8.0,
            12.0,
            (20.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
        (
            8.0,
            12.0,
            (20.0, 40.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
        (
            8.0,
            12.0,
            (80.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
        (
            8.0,
            12.0,
            (80.0, 40.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'seam_split_bore': (
        (
            8.0,
            20.0,
            (20.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'spherical_mouth_undercut_bore': (
        (
            4.0,
            9.040417140076999,
            (30.0, 30.0, 18.408417140077),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'spherical_mouth_undercut_bore_with_a_boss': (
        (
            4.0,
            9.040417140076999,
            (30.0, 30.0, 18.408417140077),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'stepped_bore': (
        (
            6.0,
            17.0,
            (25.0, 25.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
        (
            14.0,
            8.0,
            (25.0, 25.0, 25.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            True,
        ),
    ),
    'thin_plate': (
    ),
    'tube_od_not_a_hole': (
        (
            20.0,
            50.0,
            (0.0, 0.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'two_walls_far_apart': (
        (
            8.0,
            10.0,
            (25.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
        (
            8.0,
            10.0,
            (25.0, 20.0, 90.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'two_walls_gapped': (
        (
            8.0,
            10.0,
            (25.0, 20.0, 0.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
        (
            8.0,
            10.0,
            (25.0, 20.0, 30.0),
            (0.0, 0.0, 1.0),
            'THROUGH',
            False,
        ),
    ),
    'undercut_ball_seat_at_the_confusion_edge': (
        (
            16.0,
            16.00000099999999,
            (20.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'undercut_ball_seat_below_the_confusion': (
        (
            16.0,
            16.00000005000001,
            (20.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'undercut_ball_seat_blind_bore': (
        (
            12.0,
            14.100000000000001,
            (20.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'undercut_ball_seat_blind_bore_d8': (
        (
            8.0,
            12.099999999999994,
            (20.0, 20.0, 20.0),
            (0.0, 0.0, -1.0),
            'BLIND',
            False,
        ),
    ),
    'unit_cube': (
    ),
}


#: Rigid motions that COMPOSE a rotation with a translation, in both
#: orders, plus translations far enough out to be an assembly export.
#:
#: :data:`D19R4_MOTIONS` is one motion at a time and every fixture passed
#: it. That is the shape of round 5's blocker 1: `GeomAPI_IntCS`'s
#: absolute error grows with the coordinates of the surface it is handed,
#: and a translation only makes the coordinates large along the axes it
#: translates. It takes a ROTATION first — putting the bore's own axis
#: across those axes — for the error to land where the collapsed-isoline
#: test reads it. Neither half alone reaches 1e-4 mm; the product reaches
#: 1.13e-4 and the countersink caps at its own apex.
#:
#: 12.345 m and 15 m are not stress figures. A part exported from an
#: assembly carries the assembly's origin, and a machine frame is metres
#: across.
D19R5_COMPOSED = [
    ("rotate 31° about X, then 12.345 m along X", [
        ("rotate", (1.0, 0.0, 0.0), 31.0),
        ("translate", (12345.0, 0.0, 0.0), 0.0),
    ]),
    ("12.345 m along X, then rotate 31° about X", [
        ("translate", (12345.0, 0.0, 0.0), 0.0),
        ("rotate", (1.0, 0.0, 0.0), 31.0),
    ]),
    ("rotate 17° about Y, then 12.345 m along X", [
        ("rotate", (0.0, 1.0, 0.0), 17.0),
        ("translate", (12345.0, 0.0, 0.0), 0.0),
    ]),
    ("rotate 37° about Z, then 12.345 m along X", [
        ("rotate", (0.0, 0.0, 1.0), 37.0),
        ("translate", (12345.0, 0.0, 0.0), 0.0),
    ]),
    ("rotate 40° about (1,1,1), then 12.345 m along X", [
        ("rotate", (1.0, 1.0, 1.0), 40.0),
        ("translate", (12345.0, 0.0, 0.0), 0.0),
    ]),
    ("rotate 13.5° about (2,-1,3), then 12.345 m along X", [
        ("rotate", (2.0, -1.0, 3.0), 13.5),
        ("translate", (12345.0, 0.0, 0.0), 0.0),
    ]),
    ("12.345 m along X, then rotate 13.5° about (2,-1,3)", [
        ("translate", (12345.0, 0.0, 0.0), 0.0),
        ("rotate", (2.0, -1.0, 3.0), 13.5),
    ]),
    ("rotate 90° about X, then 15 m out on every axis", [
        ("rotate", (1.0, 0.0, 0.0), 90.0),
        ("translate", (15000.0, 15000.0, 15000.0), 0.0),
    ]),
    # ALONG the upright corpus's own bore axis. `_canonical_origin` puts
    # the zero of the axial parameter at the foot of the perpendicular
    # from the WORLD origin, so this is the motion — and the only one —
    # that makes `t` itself large without making the part large.
    ("15 m along Z", [("translate", (0.0, 0.0, 15000.0), 0.0)]),
    ("rotate 40° about (1,1,1), then 15 m along Z", [
        ("rotate", (1.0, 1.0, 1.0), 40.0),
        ("translate", (0.0, 0.0, 15000.0), 0.0),
    ]),
]


def _composed(shape, steps):
    """``shape`` under a whole sequence of :data:`D19R5_COMPOSED` steps."""
    for kind, vector, degrees in steps:
        shape = _moved(shape, kind, vector, degrees)
    return shape


def test_d19r5_the_corpus_moves_by_at_most_a_last_bit(fixtures_dir: Path):
    """Every hole of every committed fixture, against the table round 4
    left, field by field.

    Round 5 moves the axis/surface intersection into the bore's own frame.
    That is arithmetic, so it cannot be bit-preserving and this is the
    honest accounting of what it cost — measured across all 62 fixtures
    and 63 holes rather than asserted:

    - ``diameter_mm``, ``entry_point_mm``, ``axis_unit``,
      ``end_condition`` and ``flat_bottom`` are unchanged BIT FOR BIT on
      every row. ``flat_bottom`` is the one blocker 2 rewrites, so its
      being untouched here is the whole claim that the rewrite changed
      only wrong answers.
    - ``depth_mm`` moves on eight rows, by at most 1.5e-13 mm — a fifth
      of a picometre, twelve orders below the tightest tolerance any of
      these parts could be made to.

    And the direction of those eight is not neutral. Every one of them
    moves TOWARDS its exact nominal dimension — 9.799999999999997 to 9.8,
    6.999999999999968 to 7.0, 12.099999999999994 to 12.1 — which is what
    a frame centred on the bore does to a subtraction that used to happen
    at the part's distance from the world origin. Asserted below, so
    "closer to nominal" cannot decay into a claim nobody checks.
    """
    moved_rows, worst = [], 0.0
    for name, expected in sorted(D19R5_CORPUS.items()):
        holes = _mine(fixtures_dir / f"{name}.step")
        assert len(holes) == len(expected), (
            f"{name}: {len(holes)} holes, table says {len(expected)}"
        )
        for hole, row in zip(holes, expected):
            diameter, depth, entry, axis, end, flat = row
            assert hole.diameter_mm == diameter, (name, hole.diameter_mm, diameter)
            assert tuple(hole.entry_point_mm) == entry, (name, hole.entry_point_mm)
            assert tuple(hole.axis_unit) == axis, (name, hole.axis_unit)
            assert hole.end_condition.name == end, (name, hole.end_condition)
            assert hole.flat_bottom is flat, (
                f"{name}: `flat_bottom` is {hole.flat_bottom}, was {flat}. "
                "Round 5 rewrites this rule and no committed answer may move "
                "with it — the two that do move are in "
                "`test_d19r5_a_spun_part_does_not_grow_a_flat_bottom`, and "
                "they are wrong answers on a part the corpus does not have"
            )
            gap = abs(hole.depth_mm - depth)
            worst = max(worst, gap)
            if hole.depth_mm != depth:
                moved_rows.append((name, depth, hole.depth_mm))

    assert worst <= 1e-12, (
        f"the worst depth moved {worst} mm, which is more than a last bit. "
        "Moving the intersection into the bore's frame may cost the last "
        "bits of a subtraction and nothing else"
    )
    assert len(moved_rows) == 8, (
        f"eight rows moved when round 5 landed; {len(moved_rows)} move now: "
        f"{moved_rows}"
    )
    closer = 0
    for name, was, now in moved_rows:
        # Nine decimals: the corpus is dimensioned to the nanometre at
        # worst (`undercut_ball_seat_below_the_confusion` is 16.00000005),
        # so this recovers the exact intended figure and not a rounding of
        # it.
        target = round(now, 9)
        if abs(now - target) < abs(was - target):
            closer += 1
            continue
        # The one row that does not is `bore_beside_a_conical_boss`, whose
        # depth was ALREADY 7.2e-12 off its nominal 25.0 before round 5
        # touched anything — its cap is a cone met at a shallow angle and
        # 25.0 is not the number that computation lands on. "Closer to
        # nominal" is not a direction for a row whose nominal it does not
        # compute, so what is asserted there is only that it did not move
        # by more than a last bit, which the bound above already says.
        assert abs(was - target) > 1e-13, (
            f"{name}: {was} -> {now} moved AWAY from a nominal it was landing "
            f"on to {abs(was - target):.1e} mm. The bore's own frame is the "
            "better-conditioned one; a row drifting off an exact answer is a "
            "different change wearing this one's coat"
        )
    assert closer == 7, (
        f"seven of the eight moved TOWARDS their exact nominal dimension; "
        f"{closer} do now. That direction is the evidence that the bore's "
        "frame is better conditioned rather than merely different"
    )


def test_d19r5_no_answer_moves_under_a_composed_rigid_motion(fixtures_dir: Path):
    """The whole corpus under :data:`D19R5_COMPOSED` — the probe that
    would have caught round 5's blocker 1, and did not exist.

    This is `test_d19r4_the_corpus_is_invariant_under_rotation` with the
    one thing round 4 left out: the motions compose, and they go far
    enough out to be an assembly export. Under round 4's code SIX of
    these 620 minings move, and every one of them is a countersink
    capping at its own apex — see
    `test_d19r5_a_composed_motion_capped_the_countersink_at_its_apex`,
    which pins the six by name and by number.
    """
    for step in sorted(fixtures_dir.glob("*.step")):
        with _silence_stdout_fd():
            shape = _load_step_shape(str(step))
            upright = _rigid_invariants(mine_cylindrical_holes(shape))
            for label, steps in D19R5_COMPOSED:
                moved = _rigid_invariants(
                    mine_cylindrical_holes(_composed(shape, steps))
                )
                assert _same_invariants(upright, moved), (
                    f"{step.stem} answers differently after {label}:\n"
                    f"  upright {upright}\n  moved   {moved}"
                )


def test_d19r5_a_composed_motion_capped_the_countersink_at_its_apex(
    fixtures_dir: Path,
):
    """Blocker 1, by name and by number, on both sides of the fix.

    The rule round 4 shipped — the smaller derivative as an absolute
    length against 1e-4 mm — is replayed here through the WORLD-FRAME
    intersection it was shipped with, because that pairing is the defect
    and neither half is it alone. Under that pairing:

    - ``countersunk_blind_bore`` reads 7.000209 against a true 11.0. The
      bore's entry is then 4 mm inside solid metal and its depth is 36 %
      short — an UNDER-quote, which is the direction the module treats as
      the worse one because it never appears in a reasoning log.
    - ``countersunk_through_bore`` reads 13.0002 against a true 17.0.

    Both are the most ordinary feature in the corpus, both are committed
    fixtures, and the motion is a rotation of a few degrees followed by a
    translation smaller than a machine tool's own frame.
    """
    motions = [
        ("rotate 17° about Y, then 12.345 m along X", 0),
        ("rotate 13.5° about (2,-1,3), then 12.345 m along X", 0),
        ("12.345 m along X, then rotate 13.5° about (2,-1,3)", 0),
    ]
    by_label = dict(D19R5_COMPOSED)
    truth = {"countersunk_blind_bore": 11.0, "countersunk_through_bore": 17.0}

    broken = {}
    with _as_shipped(_round4_crossing_normal):
        for name in truth:
            with _silence_stdout_fd():
                shape = _load_step_shape(str(fixtures_dir / f"{name}.step"))
                for label, _ in motions:
                    holes = mine_cylindrical_holes(_composed(shape, by_label[label]))
                    assert len(holes) == 1, (name, label, holes)
                    broken[(name, label)] = holes[0].depth_mm

    apex = {"countersunk_blind_bore": 7.0, "countersunk_through_bore": 13.0}
    wrong = {k: v for k, v in broken.items() if abs(v - truth[k[0]]) > 1e-3}
    assert len(wrong) == 5, (
        "five of these six pairings were wrong when round 5 opened — the "
        f"blind bore under all three motions and the through bore under two. Got {wrong}"
    )
    for (name, _label), got in wrong.items():
        assert got == pytest.approx(apex[name], abs=1e-3), (name, got)
        assert got < truth[name], (
            f"{name}: capping at the apex is an UNDER-quote by construction — "
            f"{got} against {truth[name]}"
        )

    # THE FRAME IS THE FIX, and round 5's intrinsic floor is not. Put the
    # floor round 5 ships — a fraction of the bore's radius, judged
    # against the isoline's real extent — back on top of the WORLD-frame
    # intersection and every one of those five answers is still wrong, to
    # the same micron. A root the intersector has already placed 1.13e-4
    # mm off the apex is genuinely 1.13e-4 mm off the apex; no floor can
    # recover an apex from a root that is not on one.
    import aberp_cad_extract.holes as holes_mod

    with _as_shipped(holes_mod._crossing_normal):
        for name in truth:
            with _silence_stdout_fd():
                shape = _load_step_shape(str(fixtures_dir / f"{name}.step"))
                for label, _ in motions:
                    if (name, label) not in wrong:
                        continue
                    holes = mine_cylindrical_holes(_composed(shape, by_label[label]))
                    assert len(holes) == 1, (name, label)
                    assert holes[0].depth_mm == pytest.approx(
                        broken[(name, label)], abs=1e-6
                    ), (
                        f"{name} under {label}: round 5's floor on round 4's "
                        "frame answers "
                        f"{holes[0].depth_mm}, not {broken[(name, label)]}. If "
                        "the floor alone now rescues this, the claim that the "
                        "FRAME is the fix has stopped being true and the "
                        "constant's docstring is wrong"
                    )

    # And the same six, in the bore's own frame.
    for name, want in truth.items():
        with _silence_stdout_fd():
            shape = _load_step_shape(str(fixtures_dir / f"{name}.step"))
            for label, _ in motions:
                holes = mine_cylindrical_holes(_composed(shape, by_label[label]))
                assert len(holes) == 1, (name, label)
                assert holes[0].depth_mm == pytest.approx(want, abs=TOL), (
                    name,
                    label,
                    holes[0].depth_mm,
                )


def test_d19r5_the_collapsed_isoline_gap_is_measured_not_claimed(
    fixtures_dir: Path,
):
    """Both sides of the degeneracy gap, computed.

    Round 4 recorded the gap in a docstring — "every COLLAPSED isoline
    measures at most 2.4e-07 mm; every LIVE one at least 1.0 mm. Three
    and a half orders of margin below" — and nothing measured it, so when
    composing two motions pushed the worst collapsed isoline to
    1.13e-04 mm the claim went on reading true. It was not: the floor it
    was compared against was 1e-4, so the real headroom was 1.10x on the
    WRONG side.

    So this test computes the two populations rather than describing
    them, over the whole corpus under composed motions, on both the
    world-frame path and the bore-frame one:

    - IN THE WORLD FRAME the worst collapsed isoline exceeds 1e-4 mm.
      That is the mechanism, stated as a number: the intersector's
      absolute error grows with the coordinates, so the root it hands
      back is not on the apex any more, and a root a tenth of a micron
      off the apex reads as an ordinary point of the cone.
    - IN THE BORE'S FRAME the same worst case is under 1e-8 mm, against a
      live minimum of 1.0 mm — because the coordinates the intersector is
      handed are now the PART's, not the part's distance from the world
      origin.

    The floor is a fraction of the bore's radius, which on this corpus is
    between 5e-7 and 1.5e-5 mm, and it sits in a gap eight orders wide.
    """
    import aberp_cad_extract.holes as holes_mod
    from OCP.GeomLProp import GeomLProp_SLProps

    def spans(in_frame):
        seen = []
        original = holes_mod._crossing_normal

        def spy(surface, u, v, direction, radius):
            props = GeomLProp_SLProps(surface, u, v, 1, 1e-7)
            d_u, d_v = props.D1U(), props.D1V()
            seen.append(
                math.sqrt(d_u.X() ** 2 + d_u.Y() ** 2 + d_u.Z() ** 2)
            )
            seen.append(
                math.sqrt(d_v.X() ** 2 + d_v.Y() ** 2 + d_v.Z() ** 2)
            )
            return original(surface, u, v, direction, radius)

        holes_mod._crossing_normal = spy
        try:
            for step in sorted(fixtures_dir.glob("*.step")):
                with _silence_stdout_fd():
                    shape = _load_step_shape(str(step))
                    for _label, steps in D19R5_COMPOSED[:4]:
                        moved = _composed(shape, steps)
                        if in_frame:
                            mine_cylindrical_holes(moved)
                        else:
                            with _as_shipped(spy):
                                mine_cylindrical_holes(moved)
        finally:
            holes_mod._crossing_normal = original
        return seen

    def gap(values):
        """(worst collapsed, smallest live), split at a millimetre — a
        line no measurement lands anywhere near, which is the point."""
        low = [v for v in values if v < 1e-2]
        live = [v for v in values if v >= 1e-2]
        return (max(low) if low else 0.0), (min(live) if live else float("inf"))

    world_worst, world_live = gap(spans(in_frame=False))
    frame_worst, frame_live = gap(spans(in_frame=True))

    assert world_worst > 1e-4, (
        "in the WORLD frame the worst collapsed isoline must exceed the "
        f"1e-4 mm floor round 4 set for it — that is blocker 1. Got "
        f"{world_worst:.3e}, so this probe is no longer showing it"
    )
    assert frame_worst < 1e-8, (
        f"in the BORE'S frame the worst collapsed isoline is {frame_worst:.3e} "
        "mm, which is not the part's own precision any more. Something has "
        "put world coordinates back into the intersection"
    )
    assert frame_live > 0.99 and world_live > 0.99, (
        f"a LIVE isoline may not get small: {frame_live:.3e} in frame, "
        f"{world_live:.3e} in world. The axis meets a cone it is not coaxial "
        "with on the flank, where the isoline is a circle of the order of the "
        "bore's radius"
    )
    assert frame_live / frame_worst > 1e8, (
        f"the gap is {frame_live / frame_worst:.2e} wide. It was 1.10x, on "
        "the wrong side, when this branch opened; a floor set inside a gap "
        "this size is a degeneracy floor and not a tuned number"
    )


def _world_box_reaches_point(face, point, reach):
    """`_has_flat_bottom`'s lateral test as rounds 1 to 4 shipped it: a
    WORLD-AXIS bounding box of the face, inflated by the bore's radius on
    each of the three world axes.

    Kept so that "a box of a face is not a property of that face" stays a
    measurement rather than a sentence.
    """
    from OCP.Bnd import Bnd_Box
    from OCP.BRepBndLib import BRepBndLib

    box = Bnd_Box()
    BRepBndLib.Add_s(face, box)
    if box.IsVoid():
        return False
    xmin, ymin, zmin, xmax, ymax, zmax = box.Get()
    return (
        xmin - reach <= point[0] <= xmax + reach
        and ymin - reach <= point[1] <= ymax + reach
        and zmin - reach <= point[2] <= zmax + reach
    )


def _ball_nose_beside_a_coplanar_slot(spin_deg):
    """A Ø8 ball-nose blind pocket on (20,20) of a 60 x 60 x 20 plate, with
    a slot elsewhere on the plate whose FLOOR lies in the same plane as the
    lowest point of the ball nose — and the whole part spun about the
    BORE'S OWN AXIS.

    Spinning a part about the axis of one of its own bores moves no point
    of that bore relative to any point of the part. Every property of the
    hole is therefore the same at every angle, by construction, and that
    is what makes this the right probe: there is nothing to argue about
    in the geometry, so any answer that moves is the rule reading the
    world instead of the part.

    The slot is 1 mm clear of the bore laterally and never touches it. Its
    floor is a real planar face perpendicular to the bore's axis at the
    bore's own closed end, which is exactly the shape of face
    `_has_flat_bottom` has to judge — and judging it by a world-axis box
    turns the box by up to sqrt(2) as the part spins.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut, BRepAlgoAPI_Fuse
    from OCP.BRepBuilderAPI import BRepBuilderAPI_Transform
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCylinder,
        BRepPrimAPI_MakeSphere,
    )
    from OCP.gp import gp_Ax1, gp_Ax2, gp_Dir, gp_Pnt, gp_Trsf

    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 60.0, 60.0, 20.0).Shape()
    shaft = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(20.0, 20.0, 12.0), gp_Dir(0, 0, 1)), 4.0, 20.0
    ).Shape()
    nose = BRepPrimAPI_MakeSphere(gp_Pnt(20.0, 20.0, 12.0), 4.0).Shape()
    slot = BRepPrimAPI_MakeBox(gp_Pnt(25.0, 19.0, 8.0), 31.0, 2.0, 20.0).Shape()
    part = BRepAlgoAPI_Cut(
        BRepAlgoAPI_Cut(block, BRepAlgoAPI_Fuse(shaft, nose).Shape()).Shape(), slot
    ).Shape()
    spin = gp_Trsf()
    spin.SetRotation(
        gp_Ax1(gp_Pnt(20.0, 20.0, 0.0), gp_Dir(0, 0, 1)), math.radians(spin_deg)
    )
    return BRepBuilderAPI_Transform(part, spin, True).Shape()


def test_d19r5_a_spun_part_does_not_grow_a_flat_bottom():
    """Blocker 2: `flat_bottom` flipped when the part span about the
    BORE'S OWN AXIS.

    A ball nose is round. A flat-bottom drill and a 118° point are
    different cycles at different rates, so `flat_bottom` is a price, and
    Part C will read it. Rounds 1 to 4 decided the LATERAL half of it —
    does this planar face actually reach the bore — from a world-axis
    `Bnd_Box` of the face inflated by the bore's radius. A world-axis box
    of a face is not a property of that face: turn the part and the box
    turns and grows and shrinks with it, by up to sqrt(2) in each axis.

    So on the part above the answer walks: False at 0°, True from 30° to
    60°, False at 75° and 90°, True again at 135°. Fourteen angles, five
    of them wrong, on a part that has not changed at all — spinning a part
    about the axis of one of its own bores is the identity as far as that
    bore is concerned.

    The fix is to ask the FACE for a DISTANCE instead
    (:func:`holes._face_reaches_point`), which a rigid motion cannot
    change. A box in the BORE'S frame would not have been enough and this
    pins that too: `gp_Ax3(P, Dir)`'s X direction is an arbitrary choice
    of OCCT's, so a frame box only relocates the arbitrariness — the
    lateral question is radial and only the face can answer it.
    """
    import aberp_cad_extract.holes as holes_mod

    turns = (0.0, 5.0, 10.0, 15.0, 20.0, 30.0, 40.0, 45.0, 50.0, 60.0, 75.0, 90.0,
             135.0, 180.0)

    def verdicts():
        out = {}
        for deg in turns:
            with _silence_stdout_fd():
                holes = mine_cylindrical_holes(_ball_nose_beside_a_coplanar_slot(deg))
            assert len(holes) == 1, (deg, holes)
            out[deg] = (holes[0].depth_mm, holes[0].flat_bottom)
        return out

    original = holes_mod._face_reaches_point
    holes_mod._face_reaches_point = _world_box_reaches_point
    try:
        was = verdicts()
    finally:
        holes_mod._face_reaches_point = original

    phantom = sorted(deg for deg, (_d, flat) in was.items() if flat)
    assert phantom == [30.0, 40.0, 45.0, 50.0, 60.0, 135.0], (
        "the world box must still grow a phantom flat bottom on this part, "
        f"or this probe is not showing blocker 2 any more; got {phantom}"
    )

    now = verdicts()
    for deg in turns:
        depth, flat = now[deg]
        assert depth == pytest.approx(12.0, abs=TOL), (deg, depth)
        assert flat is False, (
            f"a ball nose is round at every angle; at {deg}° it is priced as a "
            "flat-bottom drill"
        )
    assert {d for d, _f in now.values()} == {now[0.0][0]}, (
        f"the depth may not move either: {now}"
    )


def test_d19r5_the_isoline_extent_does_not_depend_on_the_sample_count(
    fixtures_dir: Path,
):
    """:data:`holes.ISOLINE_EXTENT_SAMPLES` is not a convergence knob.

    The extent of an isoline is a maximum over a curve and this samples
    it, so the honest worry is that the answer is being read off a
    sampling density. It is not, and the reason is geometric rather than
    numerical: on every surface the miner meets, the collapsed parameter
    is the one swept ABOUT the bore's axis, so a LIVE isoline there is a
    closed circle of the order of the bore's radius and any two samples a
    half-turn apart already measure it at its diameter, while a COLLAPSED
    one is a point at any density at all. Walked from 4 to 64 here,
    against the countersink and the drill point that live on either side
    of the distinction.
    """
    import aberp_cad_extract.holes as holes_mod

    names = {
        "countersunk_blind_bore": 11.0,
        "countersunk_through_bore": 17.0,
        "blind_hole_drill_point": None,
        "ball_nose_blind_bore": 16.0,
        "blind_bore_under_dome": None,
    }
    shapes = {}
    for name in names:
        with _silence_stdout_fd():
            shapes[name] = _load_step_shape(str(fixtures_dir / f"{name}.step"))

    def verdicts():
        out = {}
        for name, shape in shapes.items():
            for label, steps in (("upright", []), D19R5_COMPOSED[5]):
                with _silence_stdout_fd():
                    holes = mine_cylindrical_holes(_composed(shape, steps))
                out[(name, label)] = _rigid_invariants(holes)
        return out

    reference = verdicts()
    for name, want in names.items():
        if want is not None:
            got = reference[(name, "upright")]
            assert got[0][1] == pytest.approx(want, abs=TOL), (name, got)

    original = holes_mod.ISOLINE_EXTENT_SAMPLES
    try:
        for samples in (4, 5, 8, 16, 32, 64):
            holes_mod.ISOLINE_EXTENT_SAMPLES = samples
            assert verdicts() == reference, (
                f"{samples} samples answers differently from {original}. The "
                "extent is a diameter against a point; nothing here converges"
            )
    finally:
        holes_mod.ISOLINE_EXTENT_SAMPLES = original


def test_d19r5_the_only_world_axis_box_left_is_read_on_the_bores_own_axis():
    """The class, closed structurally: no world-frame ruler decides
    anything in `holes.py` any more.

    Prose cannot carry this — the whole finding of round 5 is that round
    4 fixed two instances of a class and a docstring implied it had fixed
    the class. So the module is read, and every construction of a
    bounding box in it is accounted for:

    - :func:`holes._face_axial_span` builds ONE, and builds it of a patch
      already moved into `gp_Ax3(bore origin, bore direction)`. Only the
      box's Z range is read, and Z is the one direction of that frame the
      BORE fixes; its X is an arbitrary choice of OCCT's and nothing may
      depend on it.
    - nothing else builds one. In particular `_has_flat_bottom` no longer
      does — that was blocker 2 — and neither does `_AxisMaterial`, which
      is where round 3 removed a box of the whole part.

    A new `Bnd_Box` anywhere in this module fails here, and the way to
    make it pass is to say which frame it is read in.
    """
    import inspect

    import aberp_cad_extract.holes as holes_mod

    source = inspect.getsource(holes_mod).splitlines()
    builders = [
        (n + 1, line.strip())
        for n, line in enumerate(source)
        if ("Bnd_Box()" in line or "BRepBndLib." in line)
        and not line.lstrip().startswith("#")
    ]
    owner = inspect.getsource(holes_mod._face_axial_span)
    for _line_no, line in builders:
        assert line in owner, (
            f"`{line}` builds or fills a bounding box outside "
            "`_face_axial_span`. A world-axis box is not a property of the "
            "geometry it bounds: it turns with the part. If the quantity is "
            "axial, take it in the bore's frame and read only Z; if it is "
            "lateral, it is radial and only the face can answer it — see "
            "`_face_reaches_point`"
        )
    assert len(builders) == 2, (
        f"`_face_axial_span` builds exactly one box in two lines; found "
        f"{builders}"
    )
    assert "AddOptimal_s" in owner and "Add_s(" not in owner, (
        "the surviving box is `AddOptimal`, which bounds the SURFACE; "
        "`Add` bounds a B-spline by its poles and gave a NURBS twin a "
        "different answer from its analytic original"
    )



def test_d19r5_the_intrinsic_floor_is_hardening_and_is_pinned_as_inert():
    """:data:`holes.DEGENERATE_ISOLINE_FRACTION` moves no answer, and that
    is asserted rather than assumed.

    Round 5 fixed blocker 1 by moving the intersection into the bore's
    frame, and separately made the collapse floor intrinsic — a fraction
    of the bore's radius, judged against the isoline's real extent — for
    reasons that are about the shape of the rule rather than about any
    part in the corpus. Keeping a change that moves nothing is only
    defensible if "it moves nothing" is a measurement, so here it is:
    round 4's absolute 1e-4 mm floor, on round 5's frame, over a
    countersunk bore swept across FOUR DECADES of radius — a Ø0.04
    micro-drill to a Ø200 bore. Every pair agrees.

    What the fraction buys is not visible here and is not claimed to be.
    An absolute millimetre floor is a fiftieth of the micro-drill's radius
    and a millionth of the Ø200's, so it holds the same relative
    degeneracy to standards four decades apart; the corpus simply has
    nothing that lands in the gap between them. The same posture
    ``test_d19r2_the_void_slack_is_inert_and_is_pinned_as_inert`` takes,
    for the same reason.
    """
    from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
    from OCP.BRepPrimAPI import (
        BRepPrimAPI_MakeBox,
        BRepPrimAPI_MakeCone,
        BRepPrimAPI_MakeCylinder,
    )
    from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

    import aberp_cad_extract.holes as holes_mod

    def countersunk(radius):
        plate = max(20.0 * radius, 2.0)
        depth = plate * 0.55
        wide = max(60.0 * radius, 6.0)
        block = BRepPrimAPI_MakeBox(
            gp_Pnt(-wide / 2, -wide / 2, 0.0), wide, wide, plate
        ).Shape()
        bore = BRepPrimAPI_MakeCylinder(
            gp_Ax2(gp_Pnt(0, 0, plate - depth), gp_Dir(0, 0, 1)), radius, depth + 1.0
        ).Shape()
        sink = BRepPrimAPI_MakeCone(
            gp_Ax2(gp_Pnt(0, 0, plate - 2.0 * radius), gp_Dir(0, 0, 1)),
            0.0,
            2.0 * radius,
            2.0 * radius,
        ).Shape()
        return BRepAlgoAPI_Cut(
            BRepAlgoAPI_Cut(block, bore).Shape(), sink
        ).Shape()

    radii = (0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 4.0, 20.0, 100.0)
    shapes = {r: countersunk(r) for r in radii}

    def verdicts():
        out = {}
        for radius, shape in shapes.items():
            with _silence_stdout_fd():
                out[radius] = _rigid_invariants(mine_cylindrical_holes(shape))
        return out

    intrinsic = verdicts()
    assert all(rows for rows in intrinsic.values()), (
        f"every member of the sweep must yield a hole: {intrinsic}"
    )

    original = holes_mod._crossing_normal
    holes_mod._crossing_normal = _round4_crossing_normal
    try:
        absolute = verdicts()
    finally:
        holes_mod._crossing_normal = original

    assert absolute == intrinsic, (
        "the intrinsic floor is not inert after all — it moves an answer "
        "round 4's absolute one did not, across four decades of bore "
        f"radius:\n  absolute {absolute}\n  intrinsic {intrinsic}. That is "
        "not a failure, but it IS a claim the constant's docstring makes "
        "the other way round, and one of the two has to change"
    )


def test_d19r5_the_lateral_basis_turns_with_the_part_but_keeps_its_hand():
    """The one world-frame ruler left in the lateral rules, and why it
    cannot move a verdict.

    :func:`holes._perp_basis` builds the 2-D frame the mouth-ownership
    test and the barrier tracks are measured in, and it builds it from the
    WORLD axis least parallel to the bore. So the basis itself turns with
    the part, and which world axis it picks changes DISCONTINUOUSLY as the
    bore swings past 45°. That would be a defect of exactly the class
    round 5 is closing, except for what is read through it: a signed area
    (:func:`holes._mouth_owns_axis` asks which side of a chord the axis
    falls on) and an interval union (:func:`holes._arc_union_length`).
    Both are invariant under a COMMON rotation of the basis, and a common
    rotation is all a change of pick can be — provided the pair keeps its
    HANDEDNESS.

    It does, and not by luck: ``e1`` is perpendicular to the bore and
    ``e2 = direction x e1``, so ``e1 x e2 = direction`` identically. That
    identity is the whole guarantee, so it is asserted here over a dense
    sweep of directions including the ones that straddle a pick boundary —
    a hand-flip would silently invert every sidedness test that reads it,
    which is a wrong THROUGH rather than a wrong number.
    """
    import aberp_cad_extract.holes as holes_mod

    directions = []
    for a in range(0, 360, 7):
        for b in range(-89, 90, 11):
            lat, lon = math.radians(b), math.radians(a)
            directions.append(
                (
                    math.cos(lat) * math.cos(lon),
                    math.cos(lat) * math.sin(lon),
                    math.sin(lat),
                )
            )
    # …and the exact straddles, where the pick changes.
    root = 1.0 / math.sqrt(3.0)
    directions += [
        (1.0, 0.0, 0.0),
        (0.0, 1.0, 0.0),
        (0.0, 0.0, 1.0),
        (root, root, root),
        (0.7071067811865476, 0.7071067811865476, 0.0),
        (0.0, 0.7071067811865476, 0.7071067811865476),
    ]

    picks = set()
    for d in directions:
        d = holes_mod._unit(d)
        e1, e2 = holes_mod._perp_basis(d)
        picks.add(
            min(
                ((1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0)),
                key=lambda a: abs(holes_mod._dot(a, d)),
            )
        )
        for e in (e1, e2):
            assert abs(math.sqrt(holes_mod._dot(e, e)) - 1.0) < 1e-12, (d, e)
            assert abs(holes_mod._dot(e, d)) < 1e-12, (d, e)
        assert abs(holes_mod._dot(e1, e2)) < 1e-12, (d, e1, e2)
        hand = holes_mod._cross(e1, e2)
        assert all(abs(hand[k] - d[k]) < 1e-12 for k in range(3)), (
            f"e1 x e2 must BE the bore direction, and for {d} it is {hand}. A "
            "left-handed pair inverts every signed area measured through this "
            "basis, which is a wrong end condition and not a wrong number"
        )
    assert len(picks) == 3, (
        "the sweep must actually cross all three picks, or it is not testing "
        f"the discontinuity; it crossed {picks}"
    )
