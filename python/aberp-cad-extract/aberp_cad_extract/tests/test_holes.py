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
        lambda roots, t_edge, at_low: min(
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
        lambda face, origin, direction: list(reversed(original(face, origin, direction))),
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
