"""Generate the committed STEP test fixtures. Run BY HAND, never by the suite.

ADR-0112 Q6 asked who authors the fixtures and assumed a CAD seat. This
script is the answer instead: OCCT's own primitive + boolean kernel
(``BRepPrimAPI_MakeBox`` / ``MakeCylinder`` / ``BRepAlgoAPI_Cut``) builds
each part from *exact, written-down* dimensions, so the expected
``LocatedHole`` values in ``tests/test_holes.py`` are derived from the
same numbers the geometry was built from — not measured off a drawing
and not eyeballed in a viewer. A CAD seat would give a part whose true
dimensions we would then have to *trust*; this gives one whose true
dimensions we *stated*.

Conservative choice, flagged: the parts are simple by construction
(axis-aligned boxes with drilled bores). They exercise every branch the
miner has — through, blind, flat-bottom, stepped, coaxial-split, and the
bar-OD-is-not-a-hole rejection — but they are not a customer part with
draft angles, fillets and a messy tessellated import. Validating against
a real customer STEP is worth doing before Part C prices off this.

Output is committed because OCCT does not write byte-identical STEP
across minor versions; regenerating per-test would couple the suite to
the host OCCT build.

    python tools/generate_step_fixtures.py               # every fixture
    python tools/generate_step_fixtures.py seam_split_bore.step  # just one

Prefer the second form. An OCCT STEP header carries a WRITE TIMESTAMP, so
regenerating a fixture that did not change still rewrites its bytes and
puts unreviewable churn in the diff. Name the ones you actually mean.
"""

from __future__ import annotations

import math
import sys
from pathlib import Path

from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut, BRepAlgoAPI_Fuse
from OCP.BRepAdaptor import BRepAdaptor_Curve
from OCP.BRepFilletAPI import BRepFilletAPI_MakeFillet
from OCP.BRepPrimAPI import (
    BRepPrimAPI_MakeBox,
    BRepPrimAPI_MakeCone,
    BRepPrimAPI_MakeCylinder,
)
from OCP.GeomAbs import GeomAbs_CurveType
from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt
from OCP.Interface import Interface_Static
from OCP.ShapeUpgrade import ShapeUpgrade_ShapeDivideAngle
from OCP.STEPControl import STEPControl_AsIs, STEPControl_Writer
from OCP.TopAbs import TopAbs_EDGE
from OCP.TopExp import TopExp_Explorer
from OCP.TopoDS import TopoDS

OUT = Path(__file__).resolve().parent.parent / "aberp_cad_extract" / "tests" / "fixtures"


def _cyl(x, y, z, dx, dy, dz, radius, height):
    """A cylinder of `radius`/`height` whose base sits at (x,y,z), axis (dx,dy,dz)."""
    axis = gp_Ax2(gp_Pnt(x, y, z), gp_Dir(dx, dy, dz))
    return BRepPrimAPI_MakeCylinder(axis, radius, height).Shape()


def _edges(shape):
    """Every edge of `shape`, each yielded ONCE.

    ``TopExp_Explorer`` visits an edge once per face it bounds, so a plain
    walk hands back every edge twice and a fillet gets added twice.
    """
    seen = []
    explorer = TopExp_Explorer(shape, TopAbs_EDGE)
    while explorer.More():
        edge = TopoDS.Edge_s(explorer.Current())
        explorer.Next()
        if any(edge.IsSame(other) for other in seen):
            continue
        seen.append(edge)
        yield edge


def _write(shape, name: str) -> None:
    Interface_Static.SetCVal_s("write.step.unit", "MM")
    writer = STEPControl_Writer()
    writer.Transfer(shape, STEPControl_AsIs)
    path = OUT / name
    status = writer.Write(str(path))
    print(f"  {name}: status={status}")


def plate_4_through_holes():
    """100 x 60 x 12 plate, four Ø8.0 through-holes on a 20 mm inset rectangle.

    Holes are drilled along +Z through the full 12 mm thickness, entering
    at z=0. Expected: 4 holes, Ø8.0, depth 12.0, axis (0,0,1), entries
    (20,20,0) (20,40,0) (80,20,0) (80,40,0), all THROUGH, none flat.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 100.0, 60.0, 12.0).Shape()
    for x, y in ((20.0, 20.0), (20.0, 40.0), (80.0, 20.0), (80.0, 40.0)):
        # Start below and run past, so the cut is unambiguously through.
        tool = _cyl(x, y, -5.0, 0, 0, 1, 4.0, 22.0)
        shape = BRepAlgoAPI_Cut(shape, tool).Shape()
    return shape


def blind_hole_flat_bottom():
    """40 x 40 x 30 block, one Ø10.0 flat-bottomed blind bore 18 mm deep.

    Cut with a plain cylinder (flat end), entering at the top face z=30
    and running down to z=12. Expected: 1 hole, Ø10.0, depth 18.0, axis
    (0,0,-1), entry (20,20,30), BLIND, flat_bottom=True.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 30.0).Shape()
    # Base at z=12, axis +Z, height 18 -> reaches z=30 (the top face).
    tool = _cyl(20.0, 20.0, 12.0, 0, 0, 1, 5.0, 18.0)
    return BRepAlgoAPI_Cut(shape, tool).Shape()


def stepped_bore():
    """50 x 50 x 25 block: Ø6.0 through-hole with a Ø14.0 x 8.0 counterbore.

    Two DIFFERENT diameters on one axis. Expected: 2 holes (not 1, not
    3) — a counterbore is two drilling operations with two tools, so
    merging them would be wrong.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 50.0, 50.0, 25.0).Shape()
    through = _cyl(25.0, 25.0, -5.0, 0, 0, 1, 3.0, 35.0)
    shape = BRepAlgoAPI_Cut(shape, through).Shape()
    counter = _cyl(25.0, 25.0, 17.0, 0, 0, 1, 7.0, 12.0)
    return BRepAlgoAPI_Cut(shape, counter).Shape()


def coaxial_split_faces():
    """30 x 30 x 40 block, ONE Ø9.0 through-hole crossed by a slot.

    The transverse slot severs the bore's cylindrical surface into two
    separate faces on the SAME axis at the SAME radius. Without the
    coaxial merge this reports as two Ø9.0 holes — a 2x drilling
    over-price. Expected: exactly 1 hole spanning the full 40 mm.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 30.0, 30.0, 40.0).Shape()
    bore = _cyl(15.0, 15.0, -5.0, 0, 0, 1, 4.5, 50.0)
    shape = BRepAlgoAPI_Cut(shape, bore).Shape()
    # A through-slot across the middle, splitting the bore wall in two.
    slot = BRepPrimAPI_MakeBox(gp_Pnt(-5.0, 10.0, 18.0), 40.0, 10.0, 4.0).Shape()
    return BRepAlgoAPI_Cut(shape, slot).Shape()


def tube_od_not_a_hole():
    """Ø40 x 50 tube with a Ø20 bore — the OD must NOT count as a hole.

    Correctness risk #1. Both a bore and a bar OD are full-sweep
    cylindrical faces; only the material side distinguishes them.
    Expected: exactly 1 hole (the Ø20.0 bore), never 2.
    """
    outer = _cyl(0.0, 0.0, 0.0, 0, 0, 1, 20.0, 50.0)
    inner = _cyl(0.0, 0.0, -5.0, 0, 0, 1, 10.0, 60.0)
    return BRepAlgoAPI_Cut(outer, inner).Shape()


# ── the ADR-0112 adversarial fixtures (B1-B5, S3) ────────────────────────
#
# Every one of these RED-lights the miner as it was shipped. They exist
# because the first cut's fixture set had a hole in exactly the shape of
# its own assumptions: simple axis-aligned bores, no seam splits, no
# fillets, no angled entries, no drill points, and no part where two
# coaxial holes are genuinely two holes.


def seam_split_bore():
    """40 x 40 x 20 block, ONE Ø8.0 through-bore, cylinder split at 90°.

    B1. ``ShapeUpgrade_ShapeDivideAngle`` severs the bore's cylindrical
    surface into four quarter-faces — which real CAD exports do too. The
    shipped miner rejected each quarter as a "partial sweep" BEFORE the
    coaxial merge could rejoin them, so the hole vanished from the output
    entirely: an under-count, an under-price, and silent.

    Expected: 1 hole, Ø8.0, depth 20.0, axis (0,0,1), entry (20,20,0),
    THROUGH.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    shape = BRepAlgoAPI_Cut(shape, _cyl(20.0, 20.0, -5.0, 0, 0, 1, 4.0, 30.0)).Shape()
    divider = ShapeUpgrade_ShapeDivideAngle(math.pi / 2.0)
    divider.Init(shape)
    divider.Perform()
    return divider.Result()


def blind_hole_drill_point():
    """40 x 40 x 40 block, Ø8.0 blind bore with a 118° conical drill point.

    B2. The ONLY blind fixture the first cut committed was flat-bottomed,
    which is the one blind shape its axis-only end probe happened to get
    right. A real twist drill leaves a cone, and the miner then read the
    hole as THROUGH with its entry at the CLOSED end and its axis pointing
    out of the part — a blind hole priced without peck cycles, entering
    somewhere no drill can reach.

    Full diameter runs z=15..40; the 118° point (half angle 59°) tapers
    below z=15 to an apex at z=15 - 4/tan(59°).

    Expected: 1 hole, Ø8.0, depth 25.0 (the FULL-DIAMETER depth — see
    `_true_axial_span`), axis (0,0,-1), entry (20,20,40), BLIND,
    flat_bottom=False.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 40.0).Shape()
    bore = _cyl(20.0, 20.0, 15.0, 0, 0, 1, 4.0, 25.0)
    tip = 4.0 / math.tan(math.radians(59.0))
    point = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(20.0, 20.0, 15.0 - tip), gp_Dir(0, 0, 1)), 0.0, 4.0, tip
    ).Shape()
    return BRepAlgoAPI_Cut(shape, BRepAlgoAPI_Fuse(bore, point).Shape()).Shape()


def _two_walls(gap: float):
    """Two 10 mm walls joined by an off-axis spine, one Ø8.0 bore through both.

    A clevis, in other words — ONE solid, so it is a legal single-part
    STEP, with two genuinely separate 10 mm drilling operations on one
    axis. The spine sits at x=0..10 and the bore at x=25, well clear of it.
    """
    top = 10.0 + gap
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 10.0).Shape()
    shape = BRepAlgoAPI_Fuse(
        shape, BRepPrimAPI_MakeBox(gp_Pnt(0, 0, top), 40.0, 40.0, 10.0).Shape()
    ).Shape()
    shape = BRepAlgoAPI_Fuse(
        shape, BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 10.0, 40.0, top + 10.0).Shape()
    ).Shape()
    return BRepAlgoAPI_Cut(
        shape, _cyl(25.0, 20.0, -5.0, 0, 0, 1, 4.0, top + 20.0)
    ).Shape()


def two_walls_far_apart():
    """B3, the adversarial's own repro: 80 mm of air between the walls.

    The shipped merge had no axial-contiguity test at all, so it collapsed
    these into ONE hole of depth 100.0 — two operations priced as one, at
    a depth nothing in the shop could drill. This is the UNDER-count
    direction, which is the expensive one: an over-count shows up in the
    reasoning log and an under-count does not.

    Expected: 2 holes, Ø8.0, depth 10.0 each, entries (25,20,0) and
    (25,20,90), axis (0,0,1), THROUGH.
    """
    return _two_walls(80.0)


def two_walls_gapped():
    """B3's tight bracket: 20 mm of air, i.e. 2.5 diameters on a Ø8 bore.

    ``two_walls_far_apart`` alone would leave ``MAX_MERGE_GAP_DIAMETERS``
    pinned only somewhere below 10 D, which is no pin at all. Together
    with ``coaxial_split_faces`` (which merges across 0.44 D and must keep
    doing so) this brackets the constant to 0.45 … 2.5 diameters.

    Expected: 2 holes, Ø8.0, depth 10.0 each, entries (25,20,0) and
    (25,20,30).
    """
    return _two_walls(20.0)


def angled_through_hole():
    """60 x 60 x 20 block, Ø8.0 through-bore at 30° to the plate normal.

    B4. The miner took its depth and entry from ``BRepTools.UVBounds_s``
    — the PARAMETRIC bounding box, which for an angled bore stretches to
    the extreme of the entry ellipse rather than stopping at its centre.
    Reported depth ran ~20% long and the entry point sat 2 mm off the
    part, in mid-air.

    Truth is stated, not measured: the bore crosses 20 mm of plate at 30°,
    so it is exactly 20/cos(30°) = 23.0940107675850304 mm long, and it
    meets the z=0 face at exactly (31.5470053837925, 30, 0).

    Expected: 1 hole, Ø8.0, that depth, that entry, axis
    (0.5, 0, cos 30°), THROUGH.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 60.0, 60.0, 20.0).Shape()
    s30, c30 = math.sin(math.radians(30.0)), math.cos(math.radians(30.0))
    bore = _cyl(30.0 - 20.0 * s30, 30.0, -20.0, s30, 0.0, c30, 4.0, 80.0)
    return BRepAlgoAPI_Cut(shape, bore).Shape()


def angled_blind_hole():
    """60 x 60 x 40 block, Ø8.0 flat-bottomed blind bore at 45°.

    B4's blind arm — the worst of the family (+15% on depth) and the one
    that put the entry point ABOVE the top face. Entry is exactly
    (20, 30, 40) on the z=40 face and the bore is exactly 20.0 mm deep
    along its own axis, both by construction.

    Expected: 1 hole, Ø8.0, depth 20.0, entry (20,30,40), axis
    (cos 45°, 0, -cos 45°), BLIND, flat_bottom=True.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 60.0, 60.0, 40.0).Shape()
    k = math.sqrt(0.5)  # sin 45° == cos 45°
    # Tool base AT the intended flat bottom, running back up and out
    # through the top face, so the flat end is the hole's bottom.
    bx, bz = 20.0 + 20.0 * k, 40.0 - 20.0 * k
    tool = _cyl(bx, 30.0, bz, -k, 0.0, k, 4.0, 30.0)
    return BRepAlgoAPI_Cut(shape, tool).Shape()


def both_sides_drilled():
    """40 x 40 x 40 block, ONE Ø8.0 through-bore drilled from BOTH sides.

    S3. The two half-bores meet at z=20 and carry OPPOSITE authored axis
    senses, so the reported ``axis_unit`` and ``entry_point_mm`` used to
    depend on which half OCCT's face walk reached first — (0,0,+1) entering
    at z=0, or (0,0,-1) entering at z=40, for the same part. The final sort
    could not repair it because it sorts on the very field that flipped.

    Expected: 1 hole, Ø8.0, depth 40.0, axis (0,0,1), entry (20,20,0),
    THROUGH — and identically so under a reversed face walk.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 40.0).Shape()
    shape = BRepAlgoAPI_Cut(shape, _cyl(20.0, 20.0, -5.0, 0, 0, 1, 4.0, 25.0)).Shape()
    return BRepAlgoAPI_Cut(shape, _cyl(20.0, 20.0, 45.0, 0, 0, -1, 4.0, 25.0)).Shape()


def filleted_block():
    """40 x 30 x 20 block, every edge filleted R5.0. NO holes at all.

    B5, the convex arm, and the fixture whose absence made the whole
    partial-sweep guard VACUOUS: deleting that guard left the shipped
    suite 57/0 green, and this part then reported SIX phantom Ø10 holes.
    (Twelve edge fillets; OCCT hands back six of the twelve
    quarter-cylinders as REVERSED, which was the only test the miner had.)

    Expected: 0 holes.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 30.0, 20.0).Shape()
    maker = BRepFilletAPI_MakeFillet(shape)
    for edge in _edges(shape):
        maker.Add(5.0, edge)
    return maker.Shape()


def concave_fillet_step():
    """60 x 40 x 30 L-block with its internal concave edge filleted R5.0.

    B5, the concave arm — and the one `_is_bore_face` does NOT catch. A
    fillet in an internal corner is REVERSED, has its axis in AIR and its
    material OUTSIDE the cylinder, exactly like a bore; only the fact that
    it sweeps 90° rather than 360° tells them apart. So this is the
    fixture that keeps the post-merge sweep union honest, and it fails the
    instant that union is weakened to accept a partial sweep.

    Since the ADR-0112 round-2 corrections that union is the ONLY guard
    standing between a concave fillet and a phantom hole — see
    `bore_beside_concave_fillet`, which asks the same question on a part
    that also carries a real bore.

    Expected: 0 holes.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 60.0, 40.0, 30.0).Shape()
    shape = BRepAlgoAPI_Cut(
        shape, BRepPrimAPI_MakeBox(gp_Pnt(30.0, -5.0, 15.0), 40.0, 50.0, 25.0).Shape()
    ).Shape()
    maker = BRepFilletAPI_MakeFillet(shape)
    added = 0
    for edge in _edges(shape):
        curve = BRepAdaptor_Curve(edge)
        if curve.GetType() != GeomAbs_CurveType.GeomAbs_Line:
            continue
        mid = curve.Value(0.5 * (curve.FirstParameter() + curve.LastParameter()))
        direction = curve.Line().Direction()
        # The one edge running along Y at the inside corner (x=30, z=15).
        if abs(abs(direction.Y()) - 1.0) > 1e-9:
            continue
        if abs(mid.X() - 30.0) > 1e-6 or abs(mid.Z() - 15.0) > 1e-6:
            continue
        maker.Add(5.0, edge)
        added += 1
    if added != 1:
        raise RuntimeError(f"expected exactly 1 concave edge to fillet, found {added}")
    return maker.Shape()


# ── the ADR-0112 adversarial fixtures, round 2 (N1, correction 1) ────────
#
# Round 1 fixed depth and entry by intersecting the bore's axis with the
# face that caps it — but only when that cap was PLANAR. Every fixture it
# committed had planar caps, so nothing noticed.
#
# The first three put a CURVED cap on each shape that matters: a
# through-exit, a blind entry, and a breakout into a fillet. The last two
# belong to correction 1 — one asks whether the sweep union alone still
# separates a concave fillet from the real bore beside it, and one is the
# ordinary part that the removed axis-in-void guard would have silently
# stripped a hole from.


def _one_edge(shape, want):
    """The single edge of `shape` that `want(direction, midpoint)` picks.

    Raises rather than filleting the wrong edge — a fixture that quietly
    rounds a different corner is a fixture whose expected numbers are
    fiction.
    """
    chosen = []
    for edge in _edges(shape):
        curve = BRepAdaptor_Curve(edge)
        if curve.GetType() != GeomAbs_CurveType.GeomAbs_Line:
            continue
        mid = curve.Value(0.5 * (curve.FirstParameter() + curve.LastParameter()))
        if want(curve.Line().Direction(), mid):
            chosen.append(edge)
    if len(chosen) != 1:
        raise RuntimeError(f"expected exactly 1 matching edge, found {len(chosen)}")
    return chosen[0]


def cross_drilled_shaft():
    """Ø30 x 60 round bar, one Ø8 cross-hole 10 mm off the centreline.

    N1, and the headline repro: a bore that exits through a CYLINDRICAL
    outer surface. There is no planar cap anywhere on this hole, so the
    round-1 fix did nothing and the parametric bound stood — 22.96 % too
    deep with the entry 2.57 mm outside the bar, in mid-air. An off-centre
    cross-drilling (a lube port, a cross-pin hole) is not an exotic part.

    Truth is stated, not measured: the bore axis runs along +X at y=10,
    z=30 and meets the Ø30 OD (x² + y² = 15²) at x = ±sqrt(225 - 100).

    Expected: 1 hole, Ø8.0, depth 2·sqrt(125) = 22.360679774997898, entry
    (-sqrt(125), 10, 30), axis (1,0,0), THROUGH.

    (Centred on the bar this defect VANISHES — with y=0 the trim curve's
    extreme and the axis crossing are the same point. The offset is what
    makes the fixture bite, and it is why "cross-drilled" alone would not
    have been enough.)
    """
    bar = _cyl(0.0, 0.0, 0.0, 0, 0, 1, 15.0, 60.0)
    bore = _cyl(-30.0, 10.0, 30.0, 1, 0, 0, 4.0, 60.0)
    return BRepAlgoAPI_Cut(bar, bore).Shape()


def blind_hole_curved_top():
    """R25 D-section bar, Ø10 flat-bottomed blind bore under the CURVE.

    N1's blind arm. The bore is drilled straight down at x=10, where the
    barrelled top sits at z = sqrt(625 - 100), so its entry is on a
    cylindrical surface and its depth is measured from there. Round 1
    reported it 1.58 mm deep-and-high — the entry floating above the
    material, the same defect B4 closed for flat-topped parts.

    Expected: 1 hole, Ø10.0, depth sqrt(525) - 5 = 17.912878474779199,
    entry (10, 20, sqrt(525)), axis (0,0,-1), BLIND, flat_bottom=True.
    """
    bar = _cyl(0.0, 0.0, 0.0, 0, 1, 0, 25.0, 40.0)
    flat = BRepPrimAPI_MakeBox(gp_Pnt(-30.0, -5.0, -30.0), 60.0, 50.0, 30.0).Shape()
    shape = BRepAlgoAPI_Cut(bar, flat).Shape()
    return BRepAlgoAPI_Cut(shape, _cyl(10.0, 20.0, 5.0, 0, 0, 1, 5.0, 30.0)).Shape()


def bore_into_fillet():
    """60 x 60 x 30 block, top-right edge filleted R10, Ø6 bore through it.

    N1's fillet arm AND correction 1's positive control: a real bore that
    breaks OUT through a fillet surface must keep its true depth and must
    not be dropped. The bore is placed at x=55 so its whole Ø6 footprint
    (x = 52..58) lands inside the fillet's x = 50..60 band — clear of the
    tangent line, so the cap is unambiguously the curved face.

    The fillet is the quarter-cylinder of radius 10 about the line
    x=50, z=20, so the bore's axis leaves it at z = 20 + sqrt(75).

    Expected: 1 hole, Ø6.0, depth 20 + sqrt(75) = 28.660254037844386,
    entry (55, 30, 0), axis (0,0,1), THROUGH.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 60.0, 60.0, 30.0).Shape()
    maker = BRepFilletAPI_MakeFillet(shape)
    maker.Add(
        10.0,
        _one_edge(
            shape,
            lambda d, p: abs(abs(d.Y()) - 1.0) <= 1e-9
            and abs(p.X() - 60.0) <= 1e-6
            and abs(p.Z() - 30.0) <= 1e-6,
        ),
    )
    return BRepAlgoAPI_Cut(
        maker.Shape(), _cyl(55.0, 30.0, -5.0, 0, 0, 1, 3.0, 50.0)
    ).Shape()


def bore_beside_concave_fillet():
    """`concave_fillet_step` plus a genuine Ø8 bore through the wall.

    Correction 1's negative-and-positive control in one part. Round 1's
    `_is_bore_face` carried an axis-in-the-void arm on the argument that
    it was redundant belt-and-braces against fillets; that arm is gone,
    which leaves the post-merge sweep union as the ONLY thing separating
    an internal-corner fillet from a bore. This part asks both halves of
    that at once: the R5 concave fillet must still contribute NO hole,
    and the real bore 1 mm above it must still contribute exactly one.

    Expected: 1 hole, Ø8.0, depth 30.0, entry (0, 20, 25), axis (1,0,0),
    THROUGH.
    """
    return BRepAlgoAPI_Cut(
        concave_fillet_step(), _cyl(-5.0, 20.0, 25.0, 1, 0, 0, 4.0, 40.0)
    ).Shape()


def bore_over_centre_post():
    """60 x 60 x 40 block, Ø30 recess 20 deep, with a Ø10 post up its axis.

    Correction 1's revert-proof, and the counter-example that removed the
    arm. Round 1 recorded the axis-in-the-void test as unpinnable by any
    valid STEP file, reasoning that a well-formed solid cannot present a
    face that is REVERSED, sweeps a full 2π, and has material on its own
    axis. This part does exactly that, from two primitives and two
    booleans, and it is an ordinary shape — a bored recess with a raised
    centre boss.

    What the arm would do here is not catch a phantom: it would DROP the
    Ø30 recess entirely, reporting zero holes on a part with one. A false
    negative, on the under-count side, which is the side nobody sees.

    Expected: 1 hole, Ø30.0 (never the Ø10 post's OD), depth 20.0, entry
    (30, 30, 40), axis (0,0,-1), BLIND, flat_bottom=True.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 60.0, 60.0, 40.0).Shape()
    shape = BRepAlgoAPI_Cut(shape, _cyl(30.0, 30.0, 20.0, 0, 0, 1, 15.0, 25.0)).Shape()
    return BRepAlgoAPI_Fuse(
        shape, _cyl(30.0, 30.0, 20.0, 0, 0, 1, 5.0, 20.0)
    ).Shape()


FIXTURES = {
    "plate_4_through_holes.step": plate_4_through_holes,
    "blind_hole_flat_bottom.step": blind_hole_flat_bottom,
    "stepped_bore.step": stepped_bore,
    "coaxial_split_faces.step": coaxial_split_faces,
    "tube_od_not_a_hole.step": tube_od_not_a_hole,
    # ADR-0112 adversarial round 1.
    "seam_split_bore.step": seam_split_bore,
    "blind_hole_drill_point.step": blind_hole_drill_point,
    "two_walls_far_apart.step": two_walls_far_apart,
    "two_walls_gapped.step": two_walls_gapped,
    "angled_through_hole.step": angled_through_hole,
    "angled_blind_hole.step": angled_blind_hole,
    "both_sides_drilled.step": both_sides_drilled,
    "filleted_block.step": filleted_block,
    "concave_fillet_step.step": concave_fillet_step,
    # ADR-0112 adversarial round 2.
    "cross_drilled_shaft.step": cross_drilled_shaft,
    "blind_hole_curved_top.step": blind_hole_curved_top,
    "bore_into_fillet.step": bore_into_fillet,
    "bore_beside_concave_fillet.step": bore_beside_concave_fillet,
    "bore_over_centre_post.step": bore_over_centre_post,
}


def main(argv: list[str]) -> int:
    wanted = argv or list(FIXTURES)
    unknown = [name for name in wanted if name not in FIXTURES]
    if unknown:
        print(f"unknown fixture(s): {', '.join(unknown)}", file=sys.stderr)
        print(f"known: {', '.join(FIXTURES)}", file=sys.stderr)
        return 2
    OUT.mkdir(parents=True, exist_ok=True)
    print(f"writing STEP fixtures to {OUT}")
    for name in wanted:
        _write(FIXTURES[name](), name)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
