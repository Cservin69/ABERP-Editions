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


def test_n2_no_point_in_solid_classifier_reaches_the_miner():
    """N2: `BRepClass3d_SolidClassifier` is gone, not merely called less.

    Its CONSTRUCTION indexes the whole shell, and every one of its
    queries was a walk. Round 1 already shared one instance across bores
    to cut the cost; removing the last caller removes the class. Stated
    as an import-level assertion because "we only call it twice now" is
    the kind of claim that decays — a future edit that reaches for a
    point classifier has to justify bringing the dependency back rather
    than quietly extending an existing one.
    """
    imported = _imported_names(_miner_ast())
    assert not [name for name in imported if "BRepClass3d" in name], (
        "the miner must not reach for a solid classifier at all; "
        f"openness comes from the cap face's outward normal (N2). Found: "
        f"{[n for n in imported if 'BRepClass3d' in n]}"
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


# ── determinism ──────────────────────────────────────────────────────────


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
