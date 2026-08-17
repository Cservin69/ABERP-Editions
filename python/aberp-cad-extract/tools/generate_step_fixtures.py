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

    python tools/generate_step_fixtures.py
"""

from __future__ import annotations

import sys
from pathlib import Path

from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox, BRepPrimAPI_MakeCylinder
from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt
from OCP.Interface import Interface_Static
from OCP.STEPControl import STEPControl_AsIs, STEPControl_Writer

OUT = Path(__file__).resolve().parent.parent / "aberp_cad_extract" / "tests" / "fixtures"


def _cyl(x, y, z, dx, dy, dz, radius, height):
    """A cylinder of `radius`/`height` whose base sits at (x,y,z), axis (dx,dy,dz)."""
    axis = gp_Ax2(gp_Pnt(x, y, z), gp_Dir(dx, dy, dz))
    return BRepPrimAPI_MakeCylinder(axis, radius, height).Shape()


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


FIXTURES = {
    "plate_4_through_holes.step": plate_4_through_holes,
    "blind_hole_flat_bottom.step": blind_hole_flat_bottom,
    "stepped_bore.step": stepped_bore,
    "coaxial_split_faces.step": coaxial_split_faces,
    "tube_od_not_a_hole.step": tube_od_not_a_hole,
}


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    print(f"writing STEP fixtures to {OUT}")
    for name, build in FIXTURES.items():
        _write(build(), name)
    return 0


if __name__ == "__main__":
    sys.exit(main())
