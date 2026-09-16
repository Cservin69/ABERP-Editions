import math
from OCP.BRep import BRep_Tool
from OCP.GeomAPI import GeomAPI_IntSS

def carrier_tracks(faces, origin, e1, e2, radius, across):
    """The divisions between rim faces that the bore CONSUMED.

    Two faces of one rim met along an edge before the bore; where the bore
    ate that junction whole, no edge survives to divide the mouth and the
    ray test cannot see which skin owns the axis. The junction is still
    recoverable from the faces' own UNTRIMMED carriers -- the same reason
    _cap_axis_intersections asks carriers rather than faces.
    """
    out = []
    surfs = []
    for f in faces:
        try: surfs.append(BRep_Tool.Surface_s(f))
        except Exception: surfs.append(None)
    for i in range(len(faces)):
        for j in range(i + 1, len(faces)):
            a, b = surfs[i], surfs[j]
            if a is None or b is None: continue
            try:
                inter = GeomAPI_IntSS(a, b, 1.0e-7)
                if not inter.IsDone(): continue
                n = inter.NbLines()
            except Exception:
                continue
            for k in range(1, n + 1):
                try:
                    c = inter.Line(k)
                    u0, u1 = float(c.FirstParameter()), float(c.LastParameter())
                except Exception:
                    continue
                if not (math.isfinite(u0) and math.isfinite(u1)): continue
                steps = 240
                pts = []
                for s in range(steps + 1):
                    try:
                        p = c.Value(u0 + (u1 - u0) * s / steps)
                    except Exception:
                        break
                    pts.append(across((float(p.X()), float(p.Y()), float(p.Z())),
                                      origin, e1, e2))
                if len(pts) > 1 and any(math.hypot(q[0], q[1]) <= radius for q in pts):
                    out.append(pts)
    return out
