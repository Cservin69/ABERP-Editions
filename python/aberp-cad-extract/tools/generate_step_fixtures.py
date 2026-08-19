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
from OCP.BRepFilletAPI import BRepFilletAPI_MakeChamfer, BRepFilletAPI_MakeFillet
from OCP.BRepBuilderAPI import BRepBuilderAPI_NurbsConvert
from OCP.BRepPrimAPI import (
    BRepPrimAPI_MakeBox,
    BRepPrimAPI_MakeCone,
    BRepPrimAPI_MakeCylinder,
    BRepPrimAPI_MakeSphere,
    BRepPrimAPI_MakeTorus,
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


# ── the ADR-0112 adversarial round-3 fixtures (blockers 1 and 2) ─────────
#
# Round 2 generalised the cap walk from planar caps to any cap, and left
# two families wrong. Both are here, and every one of these RED-lights the
# round-2 miner with the number recorded in its test.
#
# Blocker 1 is the COUNTERSINK family: a cap that is a cone COAXIAL with
# the bore. Round 2 met such a cone only ever at a drill POINT, where its
# apex falls below the bore and was discarded for being out of parametric
# range. The identical cone at the bore's MOUTH puts its apex INSIDE the
# bore, where it was taken for the cap and cut the reported depth by the
# apex's whole height.
#
# Blocker 2 is the DOUBLY-CURVED CONVEX cap: a dome, a barrel, an
# imported bulge. A bore's trim curve on such a cap never reaches the
# crown the axis leaves through, so the truth lies OUTSIDE the parametric
# span that round 2 required roots to be inside, and was thrown away at
# both ends. A singly-curved cap hid this — a round bar does not curve
# along its own axis, so the trim runs right up to the crown.


def _countersunk_bore(half_angle_deg: float, cs_top_radius: float):
    """40 x 40 x 20 block, Ø8.0 through-bore, countersunk at the mouth.

    The countersink runs from the bore's own Ø8 up to `cs_top_radius` at
    the top face z=20 with the given half angle, so the full-diameter
    cylinder ends at z = 20 - (cs_top_radius - 4) / tan(half_angle).
    THAT is the depth: a countersink is not drilled depth, and neither is
    a drill point.
    """
    drop = (cs_top_radius - 4.0) / math.tan(math.radians(half_angle_deg))
    z_cs = 20.0 - drop
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    shape = BRepAlgoAPI_Cut(shape, _cyl(20.0, 20.0, -5.0, 0, 0, 1, 4.0, 30.0)).Shape()
    # Run the cone tool past the top face so the cut is unambiguous; the
    # half angle, not the tool's height, sets where it lands.
    over = drop + 3.0
    cone = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(20.0, 20.0, z_cs), gp_Dir(0, 0, 1)),
        4.0,
        4.0 + over * math.tan(math.radians(half_angle_deg)),
        over,
    ).Shape()
    return BRepAlgoAPI_Cut(shape, cone).Shape()


def countersunk_through_bore():
    """Blocker 1's headline: a 90°-included countersink (45° half angle).

    Full diameter runs z=0..17, then the countersink opens out to Ø14 at
    the top face. The cone's apex sits at z=13, 4 mm INSIDE the bore, and
    round 2 reported that as the hole's end: depth 13.0 against a true
    17.0, 23.5 % of the hole missing.

    Expected: 1 hole, Ø8.0, depth 17.0, axis (0,0,1), entry (20,20,0),
    THROUGH.
    """
    return _countersunk_bore(45.0, 7.0)


def countersunk_bore_120():
    """The same defect at the other standard countersink angle.

    120° included, so a 60° half angle: full diameter runs to
    z = 20 - 3/tan(60°) = 18.2679 and the apex sits at 15.9585.

    Two angles rather than one because round 2 did not merely mis-measure
    this hole, it mis-CLASSIFIED it, and inconsistently: the 90° hole came
    back THROUGH and the 120° hole BLIND. The end condition is read off
    the cap's normal, and at a cone's apex there is no normal — OCCT
    returns whichever generatrix the intersector happened to land on, so
    the verdict turned on solver internals. Two angles pin that both are
    now decided by geometry.

    Expected: 1 hole, Ø8.0, depth 20 - 3/tan(60°), entry (20,20,0),
    THROUGH.
    """
    return _countersunk_bore(60.0, 7.0)


def chamfered_mouth_bore():
    """A plain CHAMFERED mouth — the same cone, without the countersink name.

    45° half angle again but only 1.5 mm across — an ordinary broken
    edge, not a countersink — so full diameter runs z=0..18.5 and the
    apex sits at 14.5. Here because a chamfered bore is the commonest
    part in any shop and nothing about the defect needed a countersink;
    anything conical and coaxial at the mouth triggered it.

    Expected: 1 hole, Ø8.0, depth 18.5, entry (20,20,0), THROUGH.
    """
    return _countersunk_bore(45.0, 5.5)


def countersunk_blind_bore():
    """The countersink over a BLIND flat-bottomed bore.

    40 x 40 x 20 block, Ø8 flat-bottomed bore from z=6 to z=17, 90°
    countersink above it. Round 2 put the ENTRY at z=13 — the apex, a
    point 4 mm inside solid metal that no drill can be started at — and
    called the hole 7.0 deep against a true 11.0.

    Expected: 1 hole, Ø8.0, depth 11.0, axis (0,0,-1), entry (20,20,17),
    BLIND, flat_bottom=True.
    """
    shape = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    shape = BRepAlgoAPI_Cut(shape, _cyl(20.0, 20.0, 6.0, 0, 0, 1, 4.0, 11.0)).Shape()
    cone = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(20.0, 20.0, 17.0), gp_Dir(0, 0, 1)), 4.0, 10.0, 6.0
    ).Shape()
    return BRepAlgoAPI_Cut(shape, cone).Shape()


def bore_through_spherical_dome():
    """Blocker 2's headline: a Ø8 bore straight through a Ø40 ball.

    The bore's axis runs through the centre, so it meets the sphere at
    TRUE NORMAL INCIDENCE at both ends — the worst case, because that is
    where a dome's trim curve falls furthest short of the crown. The trim
    sits at z = ±sqrt(20² - 4²) = ±19.5959 and the material ends at ±20.

    Expected: 1 hole, Ø8.0, depth 40.0, axis (0,0,1), entry (0,0,-20),
    THROUGH. Round 2: 39.1918 deep, entering 0.4 mm inside solid metal.
    """
    ball = BRepPrimAPI_MakeSphere(gp_Pnt(0, 0, 0), 20.0).Shape()
    return BRepAlgoAPI_Cut(ball, _cyl(0, 0, -30.0, 0, 0, 1, 4.0, 60.0)).Shape()


def blind_bore_under_dome():
    """The dome defect on a BLIND hole, where it moves the entry point.

    A Ø10 flat-bottomed bore drilled DOWN into the same Ø40 ball from the
    crown, bottoming at z=8. Depth is 12.0 and the entry is the crown
    itself, (0,0,20).

    Round 2 reported 11.3649 and put the entry at z=19.3649 — inside the
    ball. Separate from the through arm because a through hole's error is
    only a number, and a blind hole's error is also a coordinate somebody
    posts to a machine.

    Expected: 1 hole, Ø10.0, depth 12.0, axis (0,0,-1), entry (0,0,20),
    BLIND, flat_bottom=True.
    """
    ball = BRepPrimAPI_MakeSphere(gp_Pnt(0, 0, 0), 20.0).Shape()
    return BRepAlgoAPI_Cut(ball, _cyl(0, 0, 8.0, 0, 0, 1, 5.0, 30.0)).Shape()


def bore_through_torus_wall():
    """A Ø4 bore radially outward through the wall of a torus.

    Major radius 12, minor 8, so the tube's wall runs from x=4 to x=20 on
    the axis the bore is drilled along. Both caps are TOROIDAL: the bore
    enters through the concave inner wall and leaves through the convex
    outer one. Depth is 20 - 4 = 16.

    This is the arm that shows the defect was not only a wrong number.
    Round 2 clipped the convex crown at x=20 for being outside the
    parametric span, so the far end had no root left but the CONCAVE
    one at x=4 — the near end's. Both ends resolved to x=4, the bore
    measured zero deep, and a hole that measures zero deep is dropped.
    The part came back with NO holes at all: an under-count, silent, on
    the side nobody sees.

    Expected: 1 hole, Ø4.0, depth 16.0, axis (1,0,0), entry (4,0,0),
    THROUGH. Round 2: zero holes.
    """
    torus = BRepPrimAPI_MakeTorus(
        gp_Ax2(gp_Pnt(0, 0, 0), gp_Dir(0, 0, 1)), 12.0, 8.0
    ).Shape()
    return BRepAlgoAPI_Cut(torus, _cyl(-1.0, 0, 0, 1, 0, 0, 2.0, 40.0)).Shape()


def bore_through_nurbs_dome():
    """The dome again, as a B-SPLINE rather than an analytic sphere.

    ``BRepBuilderAPI_NurbsConvert`` turns the ball of
    :func:`bore_through_spherical_dome` into the kind of surface an
    imported customer part actually carries, and the geometry is
    unchanged: depth 40.0, entry (0,0,-20).

    It earns its place because the analytic sphere cannot see the harder
    half of the problem. Both surfaces put a degenerate parametric POLE
    exactly where the bore's axis leaves them, and a pole has to be told
    apart from a cone's apex — but OCCT special-cases the analytic sphere
    and returns the right normal there, while at the B-spline's pole it
    returns noise ((0,-0.214,0.977) and (-0.674,-0.026,0.738) at
    neighbouring parameters of a surface whose normal is (0,0,1) along
    that whole line), and raising the derivative order does not help. A
    fix that reads OCCT's normal at the pole passes the sphere and fails
    this.

    Expected: 1 hole, Ø8.0, depth 40.0, axis (0,0,1), entry (0,0,-20),
    THROUGH — to fitting tolerance, not to the bit; it is a fitted
    surface.
    """
    ball = BRepPrimAPI_MakeSphere(gp_Pnt(0, 0, 0), 20.0).Shape()
    nurbs = BRepBuilderAPI_NurbsConvert(ball, True).Shape()
    return BRepAlgoAPI_Cut(nurbs, _cyl(0, 0, -30.0, 0, 0, 1, 4.0, 60.0)).Shape()


# ── ADR-0112 adversarial round 4: the FOREIGN-ROOT HIJACK ────────────────
#
# Round 3 relaxed the outward parametric bound and left the cross-face
# contest as "the outermost cap wins". With the bound gone, a face that
# merely NEIGHBOURS the bore's mouth wins the end whenever its UNBOUNDED
# carrier surface crosses the axis further out than the true cap — and
# reports a hole deeper than the part with its entry in mid-air.
#
# All three parts put an ordinary bore close enough to an ordinary edge
# treatment that its mouth bites into it. None of them is exotic; a bore
# 2 mm inboard of a chamfered edge is a hole near the edge of a plate.
#
# The 45° chamfer and the concave corner fillet are the clean pins: on
# both, the hijacking face's own trimmed extent provably excludes the
# crossing, so the answer is a hard number rather than a judgement.


def _chamfered_block():
    """40 x 40 x 20 block, top edge at x=40 chamfered 45° x 6 mm.

    The chamfer face runs from (34, z=20) to (40, z=14) and therefore
    spans x in [34, 40]; its plane, unbounded, is ``x + z = 54``.
    """
    box = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    maker = BRepFilletAPI_MakeChamfer(box)
    maker.Add(
        6.0,
        _one_edge(
            box,
            lambda d, p: abs(abs(d.Y()) - 1.0) <= 1e-9
            and abs(p.X() - 40.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    return maker.Shape()


def bore_beside_chamfered_edge():
    """`_chamfered_block` with a Ø8 THROUGH bore at x=32.

    The bore spans x = 28..36, so its mouth straddles x=34 and the
    chamfer is a NEIGHBOUR of the bore — but the AXIS is at x=32, two
    millimetres inboard of where the chamfer face begins. The chamfer's
    unbounded plane meets that axis at z = 54 - 32 = 22, which is 2 mm
    above a part that stops at z=20 and 2 mm outside the chamfer's own
    x range. Round 3 took it for the cap.

    Stated by construction: the axis leaves through the flat top at
    z=20, and the chamfer owns only the 120° of the bore's mouth with
    x >= 34 (cos θ >= 1/2).

    Expected: 1 hole, Ø8.0, depth 20.0, entry (32, 20, 0), axis (0,0,1),
    THROUGH.  Round 3: depth 22.0, entry z=22, off the part.
    """
    return BRepAlgoAPI_Cut(
        _chamfered_block(), _cyl(32.0, 20.0, -5.0, 0, 0, 1, 4.0, 30.0)
    ).Shape()


def blind_bore_beside_chamfered_edge():
    """The same chamfer on a BLIND bore, where the error is a coordinate.

    A Ø8 flat-bottomed bore 12 mm deep from the top face. Depth is wrong
    by the same 2 mm, and because the OPEN end of a blind hole is the one
    that carries the entry point, the reported entry moves off the part
    into air — a coordinate no machine can reach, which is exactly the
    failure B4 and round 3's blind-dome arm were about.

    Expected: 1 hole, Ø8.0, depth 12.0, entry (32, 20, 20), axis
    (0,0,-1), BLIND, flat_bottom=True.  Round 3: depth 14.0, entry
    (32, 20, 22) — 2 mm in mid-air.
    """
    return BRepAlgoAPI_Cut(
        _chamfered_block(), _cyl(32.0, 20.0, 8.0, 0, 0, 1, 4.0, 30.0)
    ).Shape()


def bore_inside_a_chamfer():
    """The POSITIVE control: a chamfer that genuinely IS the cap.

    The three parts above all rule a chamfer or a fillet OUT of the
    contest, and a miner that simply refused every chamfer would pass all
    three. This is the part that refuses it: the same 40 x 40 x 20 block
    with a 14 mm chamfer, so the chamfer face spans x in [26, 40] and the
    Ø8 bore at x=32 sits entirely INSIDE it. The whole mouth is cut in
    the chamfer, the chamfer owns all 360° of it, and the bore really
    does leave through the chamfer's plane.

    The chamfer plane runs (26, z=20) to (40, z=6), so it is ``x + z =
    46`` and the axis at x=32 leaves it at z=14.

    Expected: 1 hole, Ø8.0, depth 14.0, entry (32, 20, 0), axis (0,0,1),
    THROUGH.
    """
    box = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    maker = BRepFilletAPI_MakeChamfer(box)
    maker.Add(
        14.0,
        _one_edge(
            box,
            lambda d, p: abs(abs(d.Y()) - 1.0) <= 1e-9
            and abs(p.X() - 40.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    return BRepAlgoAPI_Cut(
        maker.Shape(), _cyl(32.0, 20.0, -5.0, 0, 0, 1, 4.0, 30.0)
    ).Shape()


def bore_beside_concave_corner_fillet():
    """A Ø14 through bore beside a CONCAVE R6 corner fillet.

    The second half of the blocker, and the reason restricting round 3's
    relaxation to non-planar caps would not have been enough: this
    hijacker is a cylinder, not a plane, and it hijacks anyway.

    A 60 x 40 x 20 base plate carries an upstand for x >= 30. The
    internal edge at (x=30, z=20) is filleted R6, giving a concave
    quarter-cylinder about the line x=24, z=26 that spans x in [24, 30].
    The bore is at x=21 — OUTSIDE the fillet's real extent — and its Ø14
    footprint reaches x=28, so the fillet is a neighbour. The fillet's
    carrier cylinder, unbounded, still meets the axis:
    z = 26 - sqrt(36 - 9) = 20.8038.

    Expected: 1 hole, Ø14.0, depth 20.0, entry (21, 20, 0), axis
    (0,0,1), THROUGH.  Round 3: depth 20.803847577293368.
    """
    base = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 60.0, 40.0, 20.0).Shape()
    upstand = BRepPrimAPI_MakeBox(gp_Pnt(30.0, 0, 20.0), 30.0, 40.0, 30.0).Shape()
    shape = BRepAlgoAPI_Fuse(base, upstand).Shape()
    maker = BRepFilletAPI_MakeFillet(shape)
    maker.Add(
        6.0,
        _one_edge(
            shape,
            lambda d, p: abs(abs(d.Y()) - 1.0) <= 1e-9
            and abs(p.X() - 30.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    return BRepAlgoAPI_Cut(
        maker.Shape(), _cyl(21.0, 20.0, -5.0, 0, 0, 1, 7.0, 40.0)
    ).Shape()


# ---------------------------------------------------------------- round 5.
#
# A bore beside a DOUBLY-chamfered part corner. Round 4 taught the miner
# that a face has to have the bore's mouth cut in it before its surface
# may say where the bore ends, and tested that with ONE neighbouring
# chamfer or fillet, where the true cap keeps 240 deg of the mouth and
# clears half a turn on its own. Chamfer the ADJACENT top edge as well —
# an ordinary detail, on an ordinary plate — and the mouth splits three
# ways with no face holding half of it, which is the round-5 blocker.
#
# Every number below is stated by construction. On the 40 x 40 x 20
# block the x=40 chamfer of leg `a` runs from (40 - a, z=20) to (40,
# z=20 - a), so its plane is `x + z = 60 - a`; the y=40 chamfer of leg
# `b` is `y + z = 60 - b`. A bore on the axis (x0, y0) therefore meets
# those two planes at z = 60 - a - x0 and z = 60 - b - y0, both ABOVE
# the plate's real top at z=20 whenever the axis is inboard of the
# chamfer. The plate still stops at 20.


def _corner_chamfered_block(leg_x: float, leg_y: float):
    """40 x 40 x 20 block, BOTH top edges at x=40 and y=40 chamfered.

    `leg_x` chamfers the edge at x=40 (plane ``x + z = 60 - leg_x``) and
    `leg_y` the one at y=40 (plane ``y + z = 60 - leg_y``). The two
    chamfers meet over the corner, so a bore placed near it has three
    different faces around its mouth.
    """
    box = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    maker = BRepFilletAPI_MakeChamfer(box)
    maker.Add(
        leg_x,
        _one_edge(
            box,
            lambda d, p: abs(abs(d.Y()) - 1.0) <= 1e-9
            and abs(p.X() - 40.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    maker.Add(
        leg_y,
        _one_edge(
            box,
            lambda d, p: abs(abs(d.X()) - 1.0) <= 1e-9
            and abs(p.Y() - 40.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    return maker.Shape()


def bore_beside_two_chamfers_corner():
    """The headline: equal 6 mm chamfers, Ø8 through-bore at (32, 32).

    Both chamfer planes are ``· + z = 54`` and both cross the axis at
    z = 22 — 2 mm above a plate that stops at 20. The mouth divides
    150 deg / 105 deg / 105 deg between the flat top and the two
    chamfers, so NO face holds half of it and round 4's per-face
    ownership abstained on all three. Worse, the two chamfers TIE at
    z=22, and pooling their sectors made one 210 deg chain that beat the
    real top outright.

    Expected: 1 hole, Ø8.0, depth 20.0, entry (32, 32, 0), axis (0,0,1),
    THROUGH.  Round 4: depth 22.0, entry z=22, off the part.
    """
    return BRepAlgoAPI_Cut(
        _corner_chamfered_block(6.0, 6.0), _cyl(32.0, 32.0, -5.0, 0, 0, 1, 4.0, 30.0)
    ).Shape()


def bore_beside_uneven_chamfer_corner():
    """The same corner with UNEQUAL legs — 6 mm and 5 mm — at (32, 32).

    No tie to pool this time: the chamfers cross the axis at z=22 and
    z=23, and the outermost of them simply won. The mouth splits
    168.59 / 115.18 / 76.23 deg, so the flat top misses half a turn by
    11.41 deg and abstains — the escape hatch, reached by an ordinary
    part with two different chamfers on it.

    Expected: 1 hole, Ø8.0, depth 20.0, entry (32, 32, 0), axis (0,0,1),
    THROUGH.  Round 4: depth 23.0, entry z=23, 3 mm off the part.
    """
    return BRepAlgoAPI_Cut(
        _corner_chamfered_block(6.0, 5.0), _cyl(32.0, 32.0, -5.0, 0, 0, 1, 4.0, 30.0)
    ).Shape()


def bore_on_a_chamfer_corner_boundary():
    """Equal 6 mm chamfers, bore at (32, 34) — the axis ON a chamfer's edge.

    y=34 is exactly where the y=40 chamfer begins, so that chamfer's
    plane meets the axis at z=20 — the same level as the flat top, and
    the right answer for once. The position was never wrong here; the
    OPENNESS was. The second chamfer OCCT builds on a block carries an
    INDIRECT (left-handed) plane, whose parametric normal is the
    negation of its ``Axis().Direction()``, and reading the axis
    direction alone flipped that one face's outward normal. It voted
    "material continues" at a genuine exit and, tied at the winning
    level, vetoed the opening — so a through-hole came back BLIND with a
    flat bottom, which prices as a different cycle.

    Expected: 1 hole, Ø8.0, depth 20.0, entry (32, 34, 0), axis (0,0,1),
    THROUGH, flat_bottom False.  Round 4: 20.0 BLIND, flat_bottom True.
    """
    return BRepAlgoAPI_Cut(
        _corner_chamfered_block(6.0, 6.0), _cyl(32.0, 34.0, -5.0, 0, 0, 1, 4.0, 30.0)
    ).Shape()


def blind_bore_beside_two_chamfers_corner():
    """The corner's NEGATIVE control: a bore that really is blind.

    Same equal-chamfer corner and the same axis at (32, 32), but the Ø8
    bore is flat-bottomed and stops 12 mm down. A miner that answered
    the three parts above by simply calling every corner bore THROUGH
    would pass all three and fail this one, and the entry point is the
    coordinate that moves: on a blind hole the OPEN end carries it, so
    the round-4 answer put the entry 2 mm above the plate in mid-air.

    Expected: 1 hole, Ø8.0, depth 12.0, entry (32, 32, 20), axis
    (0,0,-1), BLIND, flat_bottom=True.  Round 4: depth 14.0, entry
    (32, 32, 22).
    """
    return BRepAlgoAPI_Cut(
        _corner_chamfered_block(6.0, 6.0), _cyl(32.0, 32.0, 8.0, 0, 0, 1, 4.0, 30.0)
    ).Shape()


# ── ADR-0112 adversarial round 6 ─────────────────────────────────────────
#
# A bore whose mouth STRADDLES an edge of the part reaches two faces of
# the outer skin, and only one of them is the skin over the AXIS. Round 5
# took the innermost crossing over every face of the winning rim, which is
# right while no part edge crosses the mouth's footprint and wrong the
# moment one does: the face on the far side of that edge contributes the
# crossing of its UNBOUNDED carrier, which runs on under the neighbouring
# face and under the material.
#
# Every one of these under-reports on round 5, which is the direction that
# costs money — a hole mined shallower than it is gets quoted for less
# metal than the machine has to move, and on the blind bore it puts the
# entry point inside solid stock.
#
# The straddle is the uncovered gap between `bore_into_fillet`, whose bore
# lies WHOLLY INSIDE the fillet band, and the round-4 chamfer parts, whose
# neighbour crosses ABOVE the true top and is thrown out by the innermost
# rule for free. Straddling puts the neighbour's carrier BELOW it.


def _rounded_edge_block(radius: float):
    """40 x 40 x 20 block, top edge at x=40 rounded to `radius`.

    The fillet face spans x in [40 - radius, 40]; its cylinder, unbounded,
    has its axis at (40 - radius, y, 20 - radius) and dives away below
    z=20 the moment x drops under 40 - radius. That dive is the whole
    finding: at 6 mm of round the carrier is at z=18.4721 over an axis
    4 mm inboard of where the fillet begins, and the plate is 20 thick.
    """
    box = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    maker = BRepFilletAPI_MakeFillet(box)
    maker.Add(
        radius,
        _one_edge(
            box,
            lambda d, p: abs(abs(d.Y()) - 1.0) <= 1e-9
            and abs(p.X() - 40.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    return maker.Shape()


def bore_straddling_a_rounded_edge():
    """`_rounded_edge_block(6)` with a Ø10 THROUGH bore at x=30.

    The bore spans x = 25..35 and the round begins at x=34, so the mouth
    straddles it: 286.26 deg of flat top and 73.74 deg of fillet. The axis
    is 4 mm inboard of the fillet and the plate under it stops at z=20.

    Expected: 1 hole, Ø10.0, depth 20.0, entry (30, 20, 0), THROUGH.
    Round 5: depth 18.472136 — 14 + sqrt(36 - 16), the fillet's unbounded
    cylinder, 1.53 mm of plate not quoted for.
    """
    return BRepAlgoAPI_Cut(
        _rounded_edge_block(6.0), _cyl(30.0, 20.0, -5.0, 0, 0, 1, 5.0, 30.0)
    ).Shape()


def blind_bore_straddling_a_rounded_edge():
    """The same straddle on a BLIND bore, where the error is a coordinate.

    Ø10 from z=8 up. The bore ends on the plate's flat top at z=20, so it
    is 12 deep and its entry point is on the skin. Round 5 put the entry
    at z=18.472136 — 1.53 mm INSIDE solid metal, a point no drill ever
    touches — and called the hole 10.472136 deep.

    Expected: 1 hole, Ø10.0, depth 12.0, entry (30, 20, 20), axis
    (0, 0, -1), BLIND, flat bottom.
    """
    return BRepAlgoAPI_Cut(
        _rounded_edge_block(6.0), _cyl(30.0, 20.0, 8.0, 0, 0, 1, 5.0, 30.0)
    ).Shape()


def bore_straddling_a_concave_fillet():
    """A straddle where the neighbour is CONCAVE and the axis is under IT.

    A 40 x 40 x 20 block with a 10 mm rib standing 10 mm proud along
    x >= 40, and the inside corner filleted R6. The fillet's cylinder has
    its axis at (34, y, 26) and the fillet face spans x in [34, 40]; the
    flat top is what is left, x < 34.

    The Ø6 bore at x=36 spans x = 33..39, so the mouth straddles x=34 the
    other way round from `bore_straddling_a_rounded_edge`: the flat top
    holds 96.38 deg of it and the fillet 263.62 deg, and the AXIS is under
    the fillet. The part over the axis therefore stops at
    26 - sqrt(36 - 4) = 20.343146, ABOVE the flat top — so this is the
    case where the innermost crossing of the whole rim is the one face
    that is NOT there, and picking it loses material rather than
    inventing it. It also pins that the fix is not "take the outermost":
    the corner parts still need the innermost.

    Expected: 1 hole, Ø6.0, depth 20.343146, entry (36, 20, 0), THROUGH.
    Round 5: depth 20.0, the flat top's plane, 0.343 mm short.
    """
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    rib = BRepPrimAPI_MakeBox(gp_Pnt(40.0, 0, 0), 10.0, 40.0, 30.0).Shape()
    fused = BRepAlgoAPI_Fuse(block, rib).Shape()
    maker = BRepFilletAPI_MakeFillet(fused)
    maker.Add(
        6.0,
        _one_edge(
            fused,
            lambda d, p: abs(abs(d.Y()) - 1.0) <= 1e-9
            and abs(p.X() - 40.0) <= 1e-6
            and abs(p.Z() - 20.0) <= 1e-6,
        ),
    )
    return BRepAlgoAPI_Cut(
        maker.Shape(), _cyl(36.0, 20.0, -5.0, 0, 0, 1, 3.0, 40.0)
    ).Shape()


def bore_through_a_domed_shoulder():
    """A straddle at the LOW end, on a doubly-curved neighbour.

    An R8 ball fused onto the block's top corner at (40, 20, 20), so the
    sphere is the part's skin both ABOVE the block and OUTBOARD of it —
    one face, reached at BOTH ends of a bore beside it.

    The Ø8 bore at x=37 leaves through the dome at
    20 + sqrt(64 - 9) = 27.416198 and enters through the block's flat
    BOTTOM at z=0. Round 5 got the top right and the bottom wrong: the
    sphere's carrier crosses the axis a second time at z=12.583802, deep
    inside the block, and being the innermost crossing of the low end's
    rim it won. Both ends of the reported hole were then inside metal and
    the depth came back 14.832397 against a true 27.416198 — 45.9% short,
    the largest single under-report of the round.

    Expected: 1 hole, Ø8.0, depth 27.416198, entry (37, 20, 0), THROUGH.
    """
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    ball = BRepPrimAPI_MakeSphere(gp_Pnt(40.0, 20.0, 20.0), 8.0).Shape()
    return BRepAlgoAPI_Cut(
        BRepAlgoAPI_Fuse(block, ball).Shape(),
        _cyl(37.0, 20.0, -5.0, 0, 0, 1, 4.0, 90.0),
    ).Shape()


def ball_nose_blind_bore():
    """A BLIND Ø8 bore with a ball-nose bottom — a tangency, not a corner.

    A ball-nose cutter leaves a hemisphere of its own radius, so the
    sphere is TANGENT to the bore it ends. The mouth between them is the
    sphere's EQUATOR, which puts both poles exactly one radius from it,
    and "keep the root nearest the mouth" has nothing left to say. The tie
    fell to `GeomAPI_IntCS`'s list order and landed on the INWARD pole —
    inside the void the bore itself hollowed out — so the pocket read 8
    deep and THROUGH against a true 16 and BLIND, and the answer FLIPPED
    if OCCT listed its roots the other way (an S3 defect as well as a 50%
    under-report).

    Expected: 1 hole, Ø8.0, depth 16.0, entry (20, 20, 20), axis
    (0, 0, -1), BLIND, and NOT a flat bottom.
    """
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    bored = BRepAlgoAPI_Cut(block, _cyl(20.0, 20.0, 8.0, 0, 0, 1, 4.0, 30.0)).Shape()
    nose = BRepPrimAPI_MakeSphere(gp_Pnt(20.0, 20.0, 8.0), 4.0).Shape()
    return BRepAlgoAPI_Cut(bored, nose).Shape()


def ball_nose_blind_bore_d6():
    """The ball-nose tie at a depth whose arithmetic is NOT bit-symmetric.

    Identical in kind to :func:`ball_nose_blind_bore` and different in
    exactly one respect: the nose centre is at z=13.2 rather than z=8.0,
    so the two tied root distances round to 2.9999999999999982 and
    3.0000000000000018 instead of to the same double. Round 6 broke the
    tie with ``==``, which fires on the committed fixture and on nothing
    else, so this pocket mined 3.8 deep and THROUGH against a true 9.8
    and BLIND — the whole of round 6's defect, intact (round 7, blocker
    1).

    Expected: 1 hole, Ø6.0, depth 9.8, entry (20, 20, 20), axis
    (0, 0, -1), BLIND, and NOT a flat bottom.
    """
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    bored = BRepAlgoAPI_Cut(block, _cyl(20.0, 20.0, 13.2, 0, 0, 1, 3.0, 30.0)).Shape()
    nose = BRepPrimAPI_MakeSphere(gp_Pnt(20.0, 20.0, 13.2), 3.0).Shape()
    return BRepAlgoAPI_Cut(bored, nose).Shape()


def ball_nose_blind_bore_d4_deep():
    """The same tangency again, at a DIFFERENT diameter and depth.

    Ø4 at a nose centre of 5.7398492 — an untidy number on purpose, so
    that no arithmetic coincidence of the plate's or the cutter's can be
    what makes the answer come out. Round 6 mined 12.2601508 and THROUGH
    against a true 16.2601508 and BLIND.

    Two of them, at two diameters, is what makes the pair a MECHANISM
    rather than a second coincidence: the tie-break has to hold across
    the cutter sizes, not at one of them.

    Expected: 1 hole, Ø4.0, depth 16.2601508, entry (20, 20, 20), axis
    (0, 0, -1), BLIND, and NOT a flat bottom.
    """
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    bored = BRepAlgoAPI_Cut(
        block, _cyl(20.0, 20.0, 5.7398492, 0, 0, 1, 2.0, 30.0)
    ).Shape()
    nose = BRepPrimAPI_MakeSphere(gp_Pnt(20.0, 20.0, 5.7398492), 2.0).Shape()
    return BRepAlgoAPI_Cut(bored, nose).Shape()


def bore_beside_a_conical_boss():
    """A Ø8 bore under a conical boss that OVERHANGS the plate's edge.

    R10 x 20 cone based at z=10 on the corner at (40, 20), so it stands
    10 mm proud of the plate and hangs off the side of it. The bore at
    (38.5, 22) is 2.5 mm from the cone's axis, so the skin over the axis
    is the CONE at z = 30 - 2*2.5 = 25, not the plate top at 20.

    The rim here is pinched: the cone's share of the mouth is free over
    only its first 15%, and round 6's five evenly spaced rays start at
    16.7% and miss it. `_skin_over_axis` then found nothing, `_rim_winner`
    fell back to the innermost crossing of the whole rim, and the bore
    read 20.0 — 20% short, exiting 5 mm inside solid boss (round 7,
    blocker 2).

    Expected: 1 hole, Ø8.0, depth 25.0, axis (0,0,1), entry
    (38.5, 22, 0), THROUGH. Round 6: 20.0, exiting 5 mm inside the boss.
    """
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    cone = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(40.0, 20.0, 10.0), gp_Dir(0, 0, 1)), 10.0, 0.0, 20.0
    ).Shape()
    return BRepAlgoAPI_Cut(
        BRepAlgoAPI_Fuse(block, cone).Shape(),
        _cyl(38.5, 22.0, -5.0, 0, 0, 1, 4.0, 120.0),
    ).Shape()


def bore_beside_a_taller_conical_boss():
    """:func:`bore_beside_a_conical_boss` with the cone 5 mm taller.

    The neighbour, and the reason the fix is not the ray COUNT. Steepen
    the cone and the free run of its mouth moves; five rays happen to
    find this one and miss the shorter cone, six find the shorter one,
    and neither count is a property of the part. Committing both means a
    fix that merely re-tunes the count cannot pass.

    Expected: 1 hole, Ø8.0, depth 28.75, axis (0,0,1), entry
    (38.5, 22, 0), THROUGH — and round 6 gets this one RIGHT, which is
    the point of committing it.
    """
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, 20.0).Shape()
    cone = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(40.0, 20.0, 10.0), gp_Dir(0, 0, 1)), 10.0, 0.0, 25.0
    ).Shape()
    return BRepAlgoAPI_Cut(
        BRepAlgoAPI_Fuse(block, cone).Shape(),
        _cyl(38.5, 22.0, -5.0, 0, 0, 1, 4.0, 120.0),
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
    # ADR-0112 adversarial round 3.
    "countersunk_through_bore.step": countersunk_through_bore,
    "countersunk_bore_120.step": countersunk_bore_120,
    "chamfered_mouth_bore.step": chamfered_mouth_bore,
    "countersunk_blind_bore.step": countersunk_blind_bore,
    "bore_through_spherical_dome.step": bore_through_spherical_dome,
    "blind_bore_under_dome.step": blind_bore_under_dome,
    "bore_through_torus_wall.step": bore_through_torus_wall,
    "bore_through_nurbs_dome.step": bore_through_nurbs_dome,
    # ADR-0112 adversarial round 4.
    "bore_beside_chamfered_edge.step": bore_beside_chamfered_edge,
    "blind_bore_beside_chamfered_edge.step": blind_bore_beside_chamfered_edge,
    "bore_beside_concave_corner_fillet.step": bore_beside_concave_corner_fillet,
    "bore_inside_a_chamfer.step": bore_inside_a_chamfer,
    # ADR-0112 adversarial round 5.
    "bore_beside_two_chamfers_corner.step": bore_beside_two_chamfers_corner,
    "bore_beside_uneven_chamfer_corner.step": bore_beside_uneven_chamfer_corner,
    "bore_on_a_chamfer_corner_boundary.step": bore_on_a_chamfer_corner_boundary,
    "blind_bore_beside_two_chamfers_corner.step": blind_bore_beside_two_chamfers_corner,
    # ADR-0112 adversarial round 6.
    "bore_straddling_a_rounded_edge.step": bore_straddling_a_rounded_edge,
    "blind_bore_straddling_a_rounded_edge.step": blind_bore_straddling_a_rounded_edge,
    "bore_straddling_a_concave_fillet.step": bore_straddling_a_concave_fillet,
    "bore_through_a_domed_shoulder.step": bore_through_a_domed_shoulder,
    "ball_nose_blind_bore.step": ball_nose_blind_bore,
    # ADR-0112 adversarial round 7.
    "ball_nose_blind_bore_d6.step": ball_nose_blind_bore_d6,
    "ball_nose_blind_bore_d4_deep.step": ball_nose_blind_bore_d4_deep,
    "bore_beside_a_conical_boss.step": bore_beside_a_conical_boss,
    "bore_beside_a_taller_conical_boss.step": bore_beside_a_taller_conical_boss,
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
