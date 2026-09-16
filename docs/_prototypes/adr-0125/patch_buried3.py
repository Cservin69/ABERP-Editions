"""Prototype C: a cap is BURIED when another cap's face has material
between them ON THE AXIS.

Under P the entry is the first material the axis meets coming in. If another
rim face crosses the axis strictly OUTSIDE cap C, and the axis BETWEEN the two
is inside that face's material, then the part does not end at C -- C is under
that face's skin and cannot be the entry.

Level-aware and pairwise, which is what prototype B lacked: an untrimmed
half-space contains almost every point of a solid, so the test only carries
information when it is asked about the segment between two crossings.
"""
import sys
sys.path.insert(0,"/private/tmp/claude-501/-Users-aben-Documents-Claude-Projects-ABERP-Editions/eef498db-c4ed-4805-adb7-98a4033940df/scratchpad")
import aberp_cad_extract.holes as H
from buried import is_inside_material
from OCP.TopExp import TopExp_Explorer
from OCP.TopAbs import TopAbs_EDGE
from OCP.TopoDS import TopoDS

def _edges_of(face):
    out=[]; ex=TopExp_Explorer(face,TopAbs_EDGE)
    while ex.More():
        out.append(TopoDS.Edge_s(ex.Current())); ex.Next()
    return out

from OCP.TopAbs import TopAbs_VERTEX

def _vertices_of(edge):
    out=[]; ex=TopExp_Explorer(edge,TopAbs_VERTEX)
    while ex.More():
        out.append(TopoDS.Vertex_s(ex.Current())); ex.Next()
    return out

def faces_are_adjacent(f1,f2,mouth=()):
    """Do these two faces still meet ON THIS MOUTH?

    Scoped to the mouth, not to the whole solid: two faces meeting on the
    far side of a part say nothing about whether the bore consumed their
    junction HERE. A shared edge counts only when it touches this rim.
    """
    try:
        e2=_edges_of(f2)
        shared=[a for a in _edges_of(f1) if any(a.IsSame(b) for b in e2)]
        if not shared:
            return False
        if not mouth:
            return True
        ends=[v for e in mouth for v in _vertices_of(e)]
        for e in shared:
            if any(v.IsSame(w) for v in _vertices_of(e) for w in ends):
                return True
        return False
    except Exception:
        return True

STATS={"dropped":0,"errors":0}

ADJ=True

def install(use_skin=True):
    EE=H._EndEvidence
    def _rim_winner(self, origin, direction, sign, radius):
        rims=H._mouth_rims(self.mouths)
        if not rims: return None
        ranked=[]
        for keys,edges in rims:
            means=[H._edge_axial_mean(e,origin,direction) for e in edges]
            outer=[sign*m for m in means if m is not None]
            levels=[sign*c[0] for c in self.caps if c[4] in keys]
            if outer and levels:
                ranked.append((max(outer),min(levels),len(edges),keys,edges))
        if not ranked: return None
        ranked.sort(key=lambda e:(-e[0],e[1],-e[2],e[3]))
        _o,_l,_c,keys,edges=ranked[0]
        standing=self._skin_over_axis(keys,edges,origin,direction,radius) if use_skin else []
        allcaps=[c for c in self.caps if c[4] in keys]
        pool=allcaps
        def pt(c):
            p=c[2]
            return (float(p.X()),float(p.Y()),float(p.Z())) if hasattr(p,"X") else tuple(p)
        try:
            alive=[]
            for c in pool:
                lc=sign*c[0]; buried=False
                for d in allcaps:
                    ld=sign*d[0]
                    if ld <= lc + 1e-9:        # not strictly outside
                        continue
                    if d[1].IsSame(c[1]):      # same face proves nothing
                        continue
                    # ADJACENCY GUARD: if the two faces still MEET on the
                    # solid, the edge between them survived and the existing
                    # barrier machinery already has the evidence. The veto is
                    # only for the case that machinery cannot see -- where the
                    # bore CONSUMED their junction, so they no longer touch.
                    if ADJ and faces_are_adjacent(c[1],d[1],edges):
                        continue
                    a,b=pt(c),pt(d)
                    mid=((a[0]+b[0])/2.,(a[1]+b[1])/2.,(a[2]+b[2])/2.)
                    if is_inside_material(mid,d[1]):
                        buried=True; break
                if not buried: alive.append(c)
            STATS["dropped"]+=len(pool)-len(alive)
        except Exception:
            STATS["errors"]+=1; alive=allcaps
        # Reachability narrows; buriedness VETOES. If every cap reachability
        # chose is buried, its choice was wrong and buriedness stands alone.
        standing_alive=[c for c in standing if any(c is a for a in alive)]
        pool = standing_alive or alive or standing or allcaps
        level=min(sign*c[0] for c in pool)
        winners=[c for c in self.caps if c[4] in keys and abs(sign*c[0]-level)<=1e-9]
        return level,winners
    EE._rim_winner=_rim_winner
