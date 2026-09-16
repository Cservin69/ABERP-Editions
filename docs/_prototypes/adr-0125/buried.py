"""Prototype: a cap the axis cannot leave through is BURIED and out of the reckoning."""
import math
from OCP.BRep import BRep_Tool
from OCP.BRepAdaptor import BRepAdaptor_Surface
from OCP.GeomAPI import GeomAPI_ProjectPointOnSurf
from OCP.GeomLProp import GeomLProp_SLProps
from OCP.gp import gp_Pnt
from OCP.TopAbs import TopAbs_REVERSED

BURIED_TOL = 1.0e-6

def _outward_normal(surf, u, v, reversed_face):
    props = GeomLProp_SLProps(surf, u, v, 1, 1.0e-9)
    if not props.IsNormalDefined():
        return None
    n = props.Normal()
    return (-n.X(), -n.Y(), -n.Z()) if reversed_face else (n.X(), n.Y(), n.Z())

def is_inside_material(point3d, face):
    """Is `point3d` strictly inside the material bounded by `face`'s carrier?"""
    try:
        surf = BRep_Tool.Surface_s(face)
        p = gp_Pnt(*point3d)
        proj = GeomAPI_ProjectPointOnSurf(p, surf)
        if not proj.IsDone() or proj.NbPoints() < 1:
            return False
        q = proj.NearestPoint()
        u, v = proj.LowerDistanceParameters()
        n = _outward_normal(surf, u, v, face.Orientation() == TopAbs_REVERSED)
        if n is None:
            return False
        d = (p.X() - q.X(), p.Y() - q.Y(), p.Z() - q.Z())
        return (d[0]*n[0] + d[1]*n[1] + d[2]*n[2]) < -BURIED_TOL
    except Exception:
        return False

def buried_caps(caps, faces):
    """Caps whose crossing point lies inside another rim face's material."""
    out = []
    for cap in caps:
        pt = cap[2]
        p3 = (float(pt.X()), float(pt.Y()), float(pt.Z())) if hasattr(pt, "X") else tuple(pt)
        for f in faces:
            if f.IsSame(cap[1]):
                continue
            if is_inside_material(p3, f):
                out.append(cap)
                break
    return out
