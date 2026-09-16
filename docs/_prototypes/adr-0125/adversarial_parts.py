"""Adversarial parts for ADR-0125 §5. Not in the corpus."""
from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut, BRepAlgoAPI_Fuse
from OCP.BRepPrimAPI import (BRepPrimAPI_MakeBox, BRepPrimAPI_MakeCone,
                             BRepPrimAPI_MakeCylinder, BRepPrimAPI_MakeSphere)
from OCP.BRepFilletAPI import BRepFilletAPI_MakeFillet
from OCP.TopExp import TopExp_Explorer
from OCP.TopAbs import TopAbs_EDGE
from OCP.TopoDS import TopoDS
from OCP.BRepAdaptor import BRepAdaptor_Curve
from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt

def _bore(shape, bx, by, br):
    tool = BRepPrimAPI_MakeCylinder(
        gp_Ax2(gp_Pnt(bx, by, -5.0), gp_Dir(0, 0, 1)), br, 120.0).Shape()
    return BRepAlgoAPI_Cut(shape, tool).Shape()

def filleted_boss(cone_x, cone_y, cone_r, cone_h, fillet_r, bx, by, br,
                  plate_z=20.0):
    """A conical boss standing on the plate with a FILLETED base.

    The fillet sits between the cone and the plate top, so those two faces
    never touch -- which is precisely where ADR-0125's adjacency gate stops
    suppressing the buried veto.
    """
    block = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), 40.0, 40.0, plate_z).Shape()
    cone = BRepPrimAPI_MakeCone(
        gp_Ax2(gp_Pnt(cone_x, cone_y, plate_z), gp_Dir(0, 0, 1)),
        cone_r, 0.0, cone_h).Shape()
    fused = BRepAlgoAPI_Fuse(block, cone).Shape()
    # fillet every circular edge sitting at the plate's top level
    mk = BRepFilletAPI_MakeFillet(fused)
    n = 0
    ex = TopExp_Explorer(fused, TopAbs_EDGE)
    while ex.More():
        e = TopoDS.Edge_s(ex.Current()); ex.Next()
        try:
            c = BRepAdaptor_Curve(e)
            if int(c.GetType()) != 1:      # circles only
                continue
            p = c.Value(c.FirstParameter())
            if abs(p.Z() - plate_z) < 1e-6:
                mk.Add(fillet_r, e); n += 1
        except Exception:
            continue
    if n == 0:
        raise RuntimeError("no base edge found to fillet")
    return _bore(mk.Shape(), bx, by, br)
