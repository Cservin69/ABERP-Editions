"""Located-hole mining from a STEP B-rep, via OCCT/OCP.

**ADR-0112 Part B.** Turns "the part has four holes" into "the part has a
Ø6.0 hole 12 mm deep entering at (10, 10, 20) along −Z". The former
cannot be drilled-priced and cannot be toolpathed; the latter can be both.

No new dependency and no new process model: the same optional ``[step]``
extra, the same ``_load_step_shape`` reader, one extra face-walk inside
the caller's existing stdout-silencing discipline.

The algorithm
-------------

1. Walk every ``TopAbs_FACE`` of the solid; keep the cylindrical ones.
2. Reject cylinders whose material lies INSIDE the surface — a bar OD, a
   boss, or a convex edge fillet. Two independent tests, and BOTH are
   required; see :func:`_is_bore_face` for why neither alone is enough.
3. Sort the survivors into a canonical order, then group them into bores:
   equal radius, coincident axis line, **and axially contiguous**. The
   contiguity arm is what stops two holes 80 mm apart in two separate
   walls from collapsing into one 100 mm-deep hole (§ "Contiguity").
4. Reject any group that does not cover a FULL 2π of circumference —
   *after* the grouping, so a bore split into quarter-faces at a seam is
   rejoined before it is judged. A group still short of 2π is a fillet,
   a slot end, or a lone split sliver (§ "Why the sweep test moved").
5. Measure the bore's TRUE axial span from the faces that CAP its ends,
   not by reading the parametric bounding box (§ "Why UVBounds is not the
   answer").
6. Classify the end condition; detect a flat bottom.
7. Emit in a deterministic, frame-independent order.

Why the sweep test moved (ADR-0112 adversarial, B1/B5)
------------------------------------------------------

The first cut rejected partial sweeps PER FACE, before the coaxial merge.
That is the wrong place twice over:

- A Ø8 through-bore whose cylindrical surface is split into quarter-faces
  — which any ``ShapeUpgrade_ShapeDivideAngle`` pass, and plenty of real
  CAD exports, will do — had all four quarters rejected individually and
  vanished from the output entirely. An under-count, so an under-price,
  and silent. ``seam_split_bore.step`` pins it.
- It also made the fillet defence VACUOUS. Deleting the rejection left
  the whole suite green, because no fixture had a fillet in it; a plain
  filleted block then reported six phantom Ø10 holes. The guard was doing
  real work and nothing proved it. ``filleted_block.step`` and
  ``concave_fillet_step.step`` now cover it from both the convex and the
  concave side — see :func:`_is_bore_face` for which of the two guards
  each one actually pins, which is not symmetric.

So the test now runs on the merged group, and it is a genuine
CIRCUMFERENTIAL UNION rather than a per-face sweep width: four 90° faces
that tile the circle are one bore, four 90° faces that do not are not.

Why UVBounds is not the answer (ADR-0112 adversarial, B4)
----------------------------------------------------------

``BRepTools.UVBounds_s`` returns the parametric bounding box of the
trimmed face. For a bore that meets the part face at an angle, the
trimming curve is an ELLIPSE, and the box stretches to the ellipse's
extreme rather than stopping at its centre — so the reported depth ran
long and the reported entry point floated off the part, in mid-air. A 30°
through-hole came back ~20 % too deep with its entry 2 mm outside the
material.

The fix is to ask the topology instead of the parametrisation. Each end
of a bore is bounded by an edge, and across that edge sits the face that
CAPS the bore — the part's outer face for a through-hole, a counterbore
floor, a flat bottom. Where that neighbour is planar, the point where the
bore's axis meets its plane is the exact entry (or exit): it is where a
drill first touches, and it is right at any trim angle, because the
ellipse's centre and the axis-plane intersection are the same point.

Note what is deliberately NOT done: intersecting the axis with the SOLID.
That was the first attempt and it finds nothing, because a bore's axis
runs down the middle of the hole and never touches the part's skin at
either opening. Only the cap's unbounded surface has the answer.

Where the neighbour is not planar the parametric bound stands, and that
is the correct answer for the case that produces it rather than a fudge:
a 118° drill point's neighbour is a CONE whose apex lies beyond the
full-diameter cylinder, so the cylinder's own parametric end — the
full-diameter depth, which is the number on the drawing — is what is
wanted, and a perpendicular trim has no ellipse to distort it.

Contiguity (ADR-0112 adversarial, B3)
--------------------------------------

Coaxial equal-radius faces used to merge unconditionally. Two 10 mm walls
80 mm apart, bored on one axis, therefore reported as ONE 100 mm-deep
hole: two drilling operations priced as one, plus a depth nothing in the
shop could drill. That is the under-count direction, which is the
expensive one, because an over-count shows up in the reasoning log and an
under-count does not.

Faces now only merge across a gap of at most
:data:`MAX_MERGE_GAP_DIAMETERS` × diameter (with a small absolute floor).
The rationale is physical: a drill passing through a crossing feature
narrower than its own diameter never loses guidance and is one operation;
an interruption wider than that is two. Both sides are pinned —
``coaxial_split_faces.step`` (a 4 mm slot across a Ø9 bore) must stay ONE
hole, ``two_walls_far_apart.step`` (80 mm) and ``two_walls_gapped.step``
(20 mm on Ø8) must stay TWO.

Failure posture (deliberate, and the opposite of ``extract_step``'s)
--------------------------------------------------------------------

Hole mining does not fail the extraction from *inside*: any exception in
:func:`mine_cylindrical_holes` is caught by the caller. But it is no
longer SILENT either — see ``extractors/step.py`` and ADR-0112 S2. A part
with 200 holes that failed to mine used to be wire-identical to a blank
part, which is a silent under-quote; the caller now stamps a
machine-readable marker on stderr that the Rust wrapper fails on.

Determinism
-----------

The emitted order is contractual, and so is every value in it. OCCT's
explorer order is not guaranteed stable across versions, so nothing here
may depend on it: the candidate faces are sorted into a canonical order
before grouping, every bore is re-expressed on a CANONICAL axis frame
(:func:`_canonical_direction` / :func:`_canonical_origin`) rather than on
whichever face the walk happened to visit first, and the emitted list is
sorted at the end. Before the frame was canonicalised, a bore drilled
from both sides flipped its axis between (0,0,−1) and (0,0,+1) — and the
final sort could not repair that, because it sorts on the flipped field.
``both_sides_drilled.step`` pins it, under a deliberately reversed walk.
"""

from __future__ import annotations

import math
from typing import List, Optional, Sequence, Tuple

from aberp_cad_extract.feature_graph import HoleEndCondition, LocatedHole

try:
    from OCP.Bnd import Bnd_Box
    from OCP.BRepAdaptor import BRepAdaptor_Curve, BRepAdaptor_Surface
    from OCP.BRepBndLib import BRepBndLib
    from OCP.BRepClass3d import BRepClass3d_SolidClassifier
    from OCP.BRepTools import BRepTools
    from OCP.GeomAbs import GeomAbs_SurfaceType
    from OCP.gp import gp_Pnt
    from OCP.TopAbs import TopAbs_EDGE, TopAbs_FACE, TopAbs_IN, TopAbs_ON, TopAbs_REVERSED
    from OCP.TopExp import TopExp, TopExp_Explorer
    from OCP.TopoDS import TopoDS
    from OCP.TopTools import TopTools_IndexedDataMapOfShapeListOfShape

    _OCP_AVAILABLE = True
except ImportError:  # pragma: no cover — mirrors extractors/step.py
    _OCP_AVAILABLE = False


# ── tolerances ───────────────────────────────────────────────────────────
#
# All of these are geometric, not statistical. They are deliberately LOOSE
# enough to survive OCCT's own float noise on a re-exported STEP and
# TIGHT enough that two genuinely different holes never merge: the
# failure mode of merging too eagerly (under-count -> under-price) is
# worse than splitting too eagerly (over-count -> over-price), because
# an over-price is visible in the reasoning log and an under-price is not.

#: Two axes count as parallel when their unit directions agree to this
#: many degrees. 0.1° over a 100 mm part is ~0.17 mm of lateral drift.
AXIS_ANGULAR_TOL_DEG: float = 0.1

#: Two parallel axes count as the SAME line when the perpendicular
#: distance between them is under this, in mm.
AXIS_POSITIONAL_TOL_MM: float = 1e-4

#: A merged bore must cover at least this fraction of 2π of
#: CIRCUMFERENCE, summed as a union across the group's faces. Applied
#: after grouping, never per face — see the module docstring.
FULL_SWEEP_FRACTION: float = 0.999

#: Radii within this many mm are the same drill. Also the rounding used
#: when deciding whether a stepped bore is one hole or two.
RADIUS_TOL_MM: float = 1e-4

#: How far outside a hole's end to probe when classifying through/blind.
#: Must exceed OCCT's own point-classification fuzz but stay well under
#: any realistic wall thickness.
END_PROBE_MM: float = 1e-3

#: Coaxial equal-radius faces separated by more than this many DIAMETERS
#: of axial air are separate bores, not one interrupted bore. See the
#: module docstring's "Contiguity" section for the physical argument and
#: for the two fixtures that bracket the value.
#:
#: The bracket the committed fixtures actually pin is 0.45 … 2.5
#: diameters — ``coaxial_split_faces`` merges across 0.44 D and
#: ``two_walls_gapped`` splits across 2.5 D. 1.0 sits in the middle of
#: that window rather than on either edge.
MAX_MERGE_GAP_DIAMETERS: float = 1.0

#: Absolute floor for the same rule, in mm. A hairline gap is always the
#: same bore no matter how small the drill.
MAX_MERGE_GAP_FLOOR_MM: float = 1.0

#: Number of points sampled around the bore's circumference when asking
#: whether an end is open. Fixed (not adaptive) so the answer is
#: deterministic.
END_PROBE_RING_POINTS: int = 8

#: Radial fraction of the bore at which that ring is sampled. Inside the
#: bore wall, but far enough out to be in material the instant a
#: conical drill point starts closing the hole.
END_PROBE_RING_FRACTION: float = 0.9

#: Second probe distance past an end, as a fraction of the RADIUS. The
#: first probe sits at :data:`END_PROBE_MM`, flush against the end, which
#: catches a flat bottom or a counterbore shoulder; this one reaches far
#: enough in for a conical drill point to have closed the bore across
#: :data:`END_PROBE_RING_FRACTION` of its width.
#:
#: 0.5 covers the whole realistic drill-point range with margin. A point
#: of included angle 2θ has a tip length r/tan(θ), and at 0.5 r in from
#: the shoulder its remaining void radius is under 0.9 r for everything
#: from a 60° spot drill (0.71 r) through the common 118° (0.17 r) to a
#: near-flat 150°. Scaled by radius rather than absolute so it behaves
#: the same on a Ø2 hole and a Ø50 one.
#:
#: Kept as SHORT as that argument allows on purpose: every millimetre
#: further out is another millimetre in which some unrelated feature can
#: sit and make a genuine through-hole read as blind.
END_PROBE_CONE_FRACTION: float = 0.5


def _unit(v: Sequence[float]) -> Tuple[float, float, float]:
    """Normalise a 3-vector. Raises on a zero vector (a real OCCT axis is
    never zero, so this is a corrupt-input signal, not a normal path)."""
    n = math.sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2])
    if n == 0.0:
        raise ValueError("cannot normalise a zero-length axis vector")
    return (v[0] / n, v[1] / n, v[2] / n)


def _dot(a: Sequence[float], b: Sequence[float]) -> float:
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def _cross(a: Sequence[float], b: Sequence[float]) -> Tuple[float, float, float]:
    return (
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    )


def _axes_parallel(a: Sequence[float], b: Sequence[float]) -> bool:
    """True when two unit axes are parallel OR antiparallel.

    Direction-agnostic on purpose: OCCT's cylinder axis direction is a
    property of how the surface was authored, not of which way a drill
    would enter. Two faces of the same through-hole can come back with
    opposite axis senses.
    """
    c = min(1.0, max(-1.0, abs(_dot(a, b))))
    return math.degrees(math.acos(c)) <= AXIS_ANGULAR_TOL_DEG


def _axis_line_distance(
    p_a: Sequence[float], dir_a: Sequence[float], p_b: Sequence[float]
) -> float:
    """Perpendicular distance from point ``p_b`` to the line (p_a, dir_a)."""
    d = (p_b[0] - p_a[0], p_b[1] - p_a[1], p_b[2] - p_a[2])
    along = _dot(d, dir_a)
    perp = (
        d[0] - along * dir_a[0],
        d[1] - along * dir_a[1],
        d[2] - along * dir_a[2],
    )
    return math.sqrt(_dot(perp, perp))


def _canonical_direction(
    d: Sequence[float],
) -> Tuple[float, float, float]:
    """Force an axis into a canonical hemisphere.

    ADR-0112 S3. A bore's axis LINE is geometry; the SENSE of the axis
    direction OCCT hands back is an authoring artefact — for a hole
    drilled from both sides the two half-faces carry opposite senses, and
    whichever the face walk reached first used to decide the reported
    ``axis_unit`` and ``entry_point_mm``. That made the output
    walk-order-dependent, and the final sort could not repair it because
    it sorts on the very field that flipped.

    The rule: the first component whose magnitude clears the noise floor
    must be positive. Deterministic, frame-free, and total.

    Note this canonicalises the FRAME, not the reported drill direction.
    A blind hole still reports the axis pointing from its open end into
    the material — that is real geometry and is decided later, by
    :func:`_classify_ends`, not by the walk.
    """
    for c in d:
        if abs(c) > 1e-9:
            return (d[0], d[1], d[2]) if c > 0.0 else (-d[0], -d[1], -d[2])
    raise ValueError("cannot canonicalise a zero-length axis vector")


def _canonical_origin(
    p: Sequence[float], d: Sequence[float]
) -> Tuple[float, float, float]:
    """The point on the axis line closest to the world origin.

    Any point on the line describes the same line, but the axial
    parameter ``t`` is measured FROM this point, so it has to be picked
    from the line itself rather than from whichever face the walk found
    first. The foot of the perpendicular from (0,0,0) is the one choice
    that depends on nothing but the line.
    """
    along = _dot(p, d)
    return (p[0] - along * d[0], p[1] - along * d[1], p[2] - along * d[2])


class _CylFace:
    """A cylindrical face reduced to the numbers the merge step needs.

    ``t_min``/``t_max`` are the face's PARAMETRIC extent along the axis,
    measured as a signed distance from ``origin``. They bound the true
    extent but are not it — for an angled trim they run past the real end
    by the ellipse's half-height, which is exactly the B4 defect. The true
    span is resolved per bore in :func:`_true_axial_span`.

    ``arc_start``/``arc_len`` are the face's circumferential coverage,
    expressed as an absolute angle about the CANONICAL axis frame so that
    coverage from several faces can be unioned (B1/B5).
    """

    __slots__ = (
        "radius",
        "origin",
        "direction",
        "t_min",
        "t_max",
        "arc_start",
        "arc_len",
        "face",
    )

    def __init__(self, radius, origin, direction, t_min, t_max, arc_start, arc_len, face):
        self.radius = radius
        self.origin = origin
        self.direction = direction
        self.t_min = t_min
        self.t_max = t_max
        self.arc_start = arc_start
        self.arc_len = arc_len
        #: The TopoDS face itself, kept for the cap-face walk in
        #: :func:`_true_axial_span`. Deliberately absent from
        #: :meth:`sort_key` — a shape handle has no canonical order.
        self.face = face

    def sort_key(self):
        """Total order over candidate faces, so grouping cannot depend on
        OCCT's explorer order. Rounded, because two faces of one bore
        agree only to float noise."""
        return (
            round(self.radius, 9),
            tuple(round(c, 9) for c in self.direction),
            tuple(round(c, 9) for c in self.origin),
            round(self.t_min, 9),
            round(self.t_max, 9),
            round(self.arc_start, 9),
            round(self.arc_len, 9),
        )


def _perp_basis(d: Sequence[float]) -> Tuple[Tuple[float, ...], Tuple[float, ...]]:
    """A deterministic orthonormal pair perpendicular to ``d``.

    Derived from ``d`` alone — never from a face's own placement — so two
    faces of the same bore measure their circumferential coverage against
    the SAME reference and their arcs can be unioned.
    """
    world = ((1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0))
    least = min(world, key=lambda a: abs(_dot(a, d)))
    e1 = _unit(_cross(least, d))
    e2 = _unit(_cross(d, e1))
    return e1, e2


def _axis_point_is_material(classifier, origin, direction, t) -> bool:
    """Is the point ``t`` along the axis inside the solid?

    ``TopAbs_ON`` counts as material deliberately: an axis that grazes a
    face is not evidence of a void, and treating it as one would invent a
    hole. Ambiguity resolves towards "not a hole".
    """
    px, py, pz = _point_on_axis(origin, direction, t)
    classifier.Perform(gp_Pnt(px, py, pz), 1e-7)
    return classifier.State() in (TopAbs_IN, TopAbs_ON)


def _is_bore_face(face, classifier, origin, direction, t_mid) -> bool:
    """True when this cylindrical face bounds a HOLE.

    Correctness risk #1 from the ADR, plus the B5 fillet arm the first cut
    missed. TWO tests, and both are required, because each one alone
    admits something that is not a hole:

    - **Orientation.** A cylinder's surface normal points radially
      outward from the axis, and the face's orientation flag says which
      side the solid is on. A bar OD carries its material inward, so the
      face is FORWARD; a bore carries it outward, so the face is
      REVERSED. ``tube_od_not_a_hole.step`` pins both arms.

      Not sufficient on its own: on a filleted block, OCCT hands back six
      of the twelve edge-fillet quarter-cylinders as REVERSED. Those are
      the six phantom Ø10 "holes" the adversarial found.

    - **Axis in the void.** A bore is empty along its own centreline; a
      convex fillet has solid material there. This kills all twelve
      fillet faces outright, orientation flag or not.

      Not sufficient on its own either: a TUBE's outer face shares its
      centreline with the tube's own bore, so the axis of the Ø40 OD sits
      in the Ø20 void and the test would wave it through as a Ø40 hole —
      on every turned part in the shop.

    **Which arm is actually PINNED, stated honestly.** The orientation arm
    is: delete it and ``tube_od_not_a_hole`` goes red. The axis arm is
    NOT pinned by any committed STEP fixture, and structurally cannot be —
    a well-formed solid cannot present a face that is REVERSED, sweeps a
    full 2π, and has material on its own axis, because REVERSED already
    says the material is on the other side. The arm earns its place
    against inputs that are NOT well formed: a mirrored or negative-volume
    import, or a kernel whose orientation bookkeeping does not survive a
    round-trip. Straight out of ``BRepFilletAPI`` — before OCCT's STEP
    writer normalises it away — six of a filleted block's twelve
    quarter-cylinders really are REVERSED, and this arm is what stops
    them; that state is pinned in memory by
    ``test_b5_in_memory_filleted_block_is_where_the_guards_bite``, because
    no committed STEP file can carry it.

    So the two arms are deliberately REDUNDANT on a convex fillet, and the
    redundancy IS the B5 fix rather than an accident of it: now that the
    sweep test runs after the merge, "delete the sweep guard and the
    phantom holes come back" has to stop being true.
    """
    if face.Orientation() != TopAbs_REVERSED:
        return False
    return not _axis_point_is_material(classifier, origin, direction, t_mid)


def _face_to_cyl(face, classifier) -> Optional[_CylFace]:
    """Reduce one face to a :class:`_CylFace`, or ``None`` if it is not a
    cylindrical face that bounds a hole.

    NOTE what is deliberately NOT tested here: how much of the
    circumference the face covers. A quarter-face may be one quarter of a
    real bore, and only the merged group knows. See the module docstring.
    """
    adaptor = BRepAdaptor_Surface(face)
    if adaptor.GetType() != GeomAbs_SurfaceType.GeomAbs_Cylinder:
        return None

    u_min, u_max, v_min, v_max = BRepTools.UVBounds_s(face)
    # U is the circumferential parameter (radians), V the axial one (mm).
    sweep = abs(u_max - u_min)
    if sweep <= 0.0:
        return None

    depth = abs(v_max - v_min)
    if depth <= 0.0:
        return None

    cyl = adaptor.Cylinder()
    radius = float(cyl.Radius())
    if radius <= 0.0:
        return None

    axis = cyl.Axis()
    loc = axis.Location()
    dvec = axis.Direction()
    face_dir = _unit((float(dvec.X()), float(dvec.Y()), float(dvec.Z())))
    face_origin = (float(loc.X()), float(loc.Y()), float(loc.Z()))

    # Re-express on the canonical frame immediately, so nothing
    # downstream can inherit this face's authored sense (S3).
    direction = _canonical_direction(face_dir)
    origin = _canonical_origin(face_origin, direction)

    # V is measured from the cylinder's own origin along its own axis; the
    # canonical origin sits somewhere else on the same line and may run
    # the other way, so shift and flip into the canonical parameter.
    offset = _dot(
        (
            face_origin[0] - origin[0],
            face_origin[1] - origin[1],
            face_origin[2] - origin[2],
        ),
        direction,
    )
    sense = 1.0 if _dot(face_dir, direction) >= 0.0 else -1.0
    a = offset + sense * float(v_min)
    b = offset + sense * float(v_max)
    t_min, t_max = min(a, b), max(a, b)

    if not _is_bore_face(face, classifier, origin, direction, 0.5 * (t_min + t_max)):
        return None

    arc_start, arc_len = _face_arc(cyl, direction, u_min, u_max)
    return _CylFace(
        radius=radius,
        origin=origin,
        direction=direction,
        t_min=t_min,
        t_max=t_max,
        arc_start=arc_start,
        arc_len=arc_len,
        face=face,
    )


def _face_arc(cyl, direction, u_min, u_max) -> Tuple[float, float]:
    """The face's circumferential coverage as an ABSOLUTE arc.

    OCCT parametrises a cylinder as ``P(u,v) = Loc + r(cos u·X + sin u·Y)
    + v·Dir`` against the surface's own ``gp_Ax3``. Two faces of one bore
    can carry different ``X``/``Y`` and opposite ``Dir``, so raw ``u``
    intervals from different faces are not comparable and unioning them
    would be meaningless. Both endpoints are therefore projected onto the
    canonical perpendicular basis and returned as (start angle, length)
    measured the same way for every face on the axis.
    """
    pos = cyl.Position()
    xd, yd = pos.XDirection(), pos.YDirection()
    x = (float(xd.X()), float(xd.Y()), float(xd.Z()))
    y = (float(yd.X()), float(yd.Y()), float(yd.Z()))
    fd = pos.Direction()
    face_dir = (float(fd.X()), float(fd.Y()), float(fd.Z()))

    e1, e2 = _perp_basis(direction)

    def angle_at(u: float) -> float:
        rad = (
            math.cos(u) * x[0] + math.sin(u) * y[0],
            math.cos(u) * x[1] + math.sin(u) * y[1],
            math.cos(u) * x[2] + math.sin(u) * y[2],
        )
        return math.atan2(_dot(rad, e2), _dot(rad, e1))

    length = min(abs(float(u_max) - float(u_min)), 2.0 * math.pi)
    # Increasing u sweeps the canonical frame forwards only when the
    # face's own axis agrees with the canonical one; otherwise it runs
    # backwards and the interval starts at the far endpoint.
    start = angle_at(u_min if _dot(face_dir, direction) >= 0.0 else u_max)
    return start % (2.0 * math.pi), length


def _arc_union_length(arcs: Sequence[Tuple[float, float]]) -> float:
    """Total angular measure covered by a set of arcs on a circle.

    Plain interval union after splitting anything that wraps past 2π.
    Touching intervals contribute their exact sum, so four 90° faces that
    tile the circle measure a clean 2π and nothing has to be fudged.
    """
    two_pi = 2.0 * math.pi
    spans: List[Tuple[float, float]] = []
    for start, length in arcs:
        if length >= two_pi:
            return two_pi
        end = start + length
        if end <= two_pi:
            spans.append((start, end))
        else:
            spans.append((start, two_pi))
            spans.append((0.0, end - two_pi))

    spans.sort()
    total = 0.0
    cur_lo, cur_hi = None, None
    for lo, hi in spans:
        if cur_hi is None:
            cur_lo, cur_hi = lo, hi
        elif lo <= cur_hi:
            cur_hi = max(cur_hi, hi)
        else:
            total += cur_hi - cur_lo
            cur_lo, cur_hi = lo, hi
    if cur_hi is not None:
        total += cur_hi - cur_lo
    return min(total, two_pi)


class _BoreGroup:
    """Coaxial, equal-radius, axially contiguous faces of one bore."""

    __slots__ = ("radius", "origin", "direction", "lo", "hi", "arcs", "faces")

    def __init__(self, face: _CylFace):
        self.radius = face.radius
        self.origin = face.origin
        self.direction = face.direction
        self.lo = face.t_min
        self.hi = face.t_max
        self.arcs = [(face.arc_start, face.arc_len)]
        self.faces = [face.face]

    def gap_tolerance(self) -> float:
        return max(
            MAX_MERGE_GAP_FLOOR_MM,
            MAX_MERGE_GAP_DIAMETERS * 2.0 * self.radius,
        )

    def accepts(self, face: _CylFace) -> bool:
        """Is ``face`` part of THIS bore?

        Equal radius, coincident axis line, and axially contiguous. Every
        condition is required and none is inferred: merging too eagerly
        under-counts, and an under-count is an under-price nobody sees.
        A stepped bore (Ø10 counterbore over a Ø6 through-hole) fails the
        radius arm and stays two entries, which is right — two tools, two
        operations.
        """
        if abs(self.radius - face.radius) > RADIUS_TOL_MM:
            return False
        if not _axes_parallel(self.direction, face.direction):
            return False
        if _axis_line_distance(self.origin, self.direction, face.origin) > AXIS_POSITIONAL_TOL_MM:
            return False
        # Axial contiguity (B3). Overlap is a gap of zero or less.
        gap = max(self.lo - face.t_max, face.t_min - self.hi)
        return gap <= self.gap_tolerance()

    def add(self, face: _CylFace) -> None:
        self.lo = min(self.lo, face.t_min)
        self.hi = max(self.hi, face.t_max)
        self.arcs.append((face.arc_start, face.arc_len))
        self.faces.append(face.face)

    def is_full_sweep(self) -> bool:
        return _arc_union_length(self.arcs) >= FULL_SWEEP_FRACTION * 2.0 * math.pi


def _wire_vec(v: Sequence[float]) -> List[float]:
    """A 3-vector cleaned up for the wire.

    Negating a canonical axis to point a blind hole's drill INTO the
    material turns 0.0 into -0.0, and ``json.dumps`` writes that as
    ``-0.0``. It compares equal to 0.0 in Python, so no test would catch
    it, but the daemon blake3-hashes the encoded BYTES into
    `feature_graph_hash` — two runs that agree numerically would hash
    differently. Fold the sign of zero here, once, at the boundary.
    """
    return [c + 0.0 if c == 0.0 else c for c in v]


def _point_on_axis(
    origin: Sequence[float], direction: Sequence[float], t: float
) -> Tuple[float, float, float]:
    return (
        origin[0] + t * direction[0],
        origin[1] + t * direction[1],
        origin[2] + t * direction[2],
    )


def _edge_axial_mean(edge, origin, direction) -> Optional[float]:
    """Roughly where along the axis this edge sits.

    Used only to decide WHICH END of the bore an edge belongs to, never
    as a measurement — so a coarse sample of the curve is plenty and a
    quarter-arc's mean being off-centre does not matter.
    """
    try:
        curve = BRepAdaptor_Curve(edge)
        u0, u1 = float(curve.FirstParameter()), float(curve.LastParameter())
    except Exception:  # noqa: BLE001 — an unreadable edge is simply not a cap
        return None
    samples = 9
    total = 0.0
    for k in range(samples):
        u = u0 + (u1 - u0) * k / (samples - 1)
        p = curve.Value(u)
        total += _dot(
            (float(p.X()) - origin[0], float(p.Y()) - origin[1], float(p.Z()) - origin[2]),
            direction,
        )
    return total / samples


def _plane_axis_intersection(face, origin, direction) -> Optional[float]:
    """Where the bore's axis meets this face's (unbounded) plane.

    ``None`` when the face is not planar, or when the axis lies so nearly
    IN the plane that the intersection is numerically meaningless — a
    grazing cap is not a cap.
    """
    adaptor = BRepAdaptor_Surface(face)
    if adaptor.GetType() != GeomAbs_SurfaceType.GeomAbs_Plane:
        return None
    plane = adaptor.Plane()
    n = plane.Axis().Direction()
    normal = _unit((float(n.X()), float(n.Y()), float(n.Z())))
    denom = _dot(direction, normal)
    if abs(denom) < 1e-6:
        return None
    p = plane.Location()
    to_plane = (
        float(p.X()) - origin[0],
        float(p.Y()) - origin[1],
        float(p.Z()) - origin[2],
    )
    return _dot(to_plane, normal) / denom


def _true_axial_span(group, ancestors, p_lo, p_hi) -> Tuple[float, float]:
    """The bore's REAL axial span, not its parametric bounding box (B4).

    ``p_lo``/``p_hi`` are the parametric bounds — a strict SUPERSET of the
    truth, since an angled trim's ellipse runs past the real end. The
    exact ends come from the faces that CAP the bore: walk the bore's own
    boundary edges, step across each one to the neighbouring face, and
    where that neighbour is planar take the point at which the axis meets
    its plane. For any trim angle that point is both the ellipse's centre
    and the spot a drill touches first, which is the definition wanted.

    Candidates are split at the bore's midpoint rather than taken as a
    global min/max, so a feature CROSSING the bore — a slot, a
    counterbore floor — cannot be mistaken for one of its ends. Within an
    end the outermost candidate wins, which is what carries a bore
    interrupted by a slot out to the part's real faces.

    Either end with no planar cap keeps its parametric bound. See the
    module docstring for why that is the right answer and not a fallback.
    """
    origin, direction = group.origin, group.direction
    mid = 0.5 * (p_lo + p_hi)
    pad = 1e-6
    low: List[float] = []
    high: List[float] = []

    for cyl_face in group.faces:
        explorer = TopExp_Explorer(cyl_face, TopAbs_EDGE)
        while explorer.More():
            edge = TopoDS.Edge_s(explorer.Current())
            explorer.Next()
            try:
                if not ancestors.Contains(edge):
                    continue
                t_edge = _edge_axial_mean(edge, origin, direction)
                if t_edge is None:
                    continue
                for neighbour in ancestors.FindFromKey(edge):
                    if any(neighbour.IsSame(own) for own in group.faces):
                        continue
                    t_cap = _plane_axis_intersection(
                        TopoDS.Face_s(neighbour), origin, direction
                    )
                    if t_cap is None or not (p_lo - pad <= t_cap <= p_hi + pad):
                        continue
                    (low if t_edge < mid else high).append(t_cap)
            except Exception:  # noqa: BLE001 — one odd edge must not kill the bore
                continue

    return (min(low) if low else p_lo), (max(high) if high else p_hi)


def _end_is_open(classifier, origin, direction, radius, t_end, outward) -> bool:
    """Does the bore open to air at this end, or is it capped?

    Probing a single point on the axis is not enough, and that is B2. A
    118° conical drill point leaves the axis in VOID for the whole height
    of the cone, so an axis-only probe just inside the point reads "open"
    and the hole is reported THROUGH — with its entry at the closed end
    and its axis pointing out of the part. Real money: a blind hole read
    as through under-counts peck cycles, and the entry point is somewhere
    a drill cannot go.

    So the whole CROSS-SECTION is probed, at a ring near the bore wall as
    well as on the axis, and at several distances out. A cone closes on
    the wall long before it closes on the axis, so the ring catches it
    immediately. The end is OPEN only if EVERY sample is outside the
    solid — one material sample is enough to call it capped, which is the
    conservative direction (blind costs more than through, and an
    over-price is visible where an under-price is not).
    """
    two_pi = 2.0 * math.pi
    e1, e2 = _perp_basis(direction)
    ring_r = END_PROBE_RING_FRACTION * radius
    offsets = sorted({END_PROBE_MM, END_PROBE_CONE_FRACTION * radius})

    for off in offsets:
        t = t_end + outward * off
        cx, cy, cz = _point_on_axis(origin, direction, t)
        classifier.Perform(gp_Pnt(cx, cy, cz), 1e-7)
        if classifier.State() in (TopAbs_IN, TopAbs_ON):
            return False
        for k in range(END_PROBE_RING_POINTS):
            a = two_pi * k / END_PROBE_RING_POINTS
            ox = ring_r * (math.cos(a) * e1[0] + math.sin(a) * e2[0])
            oy = ring_r * (math.cos(a) * e1[1] + math.sin(a) * e2[1])
            oz = ring_r * (math.cos(a) * e1[2] + math.sin(a) * e2[2])
            classifier.Perform(gp_Pnt(cx + ox, cy + oy, cz + oz), 1e-7)
            if classifier.State() in (TopAbs_IN, TopAbs_ON):
                return False
    return True


def _classify_ends(
    classifier, origin, direction, radius, t_lo, t_hi
) -> Tuple[HoleEndCondition, bool, bool]:
    """Decide through / blind / unknown, and report which ends are open.

    - both ends open  -> THROUGH
    - exactly one     -> BLIND, and the open one is the entry
    - neither         -> UNKNOWN (an internal cavity, or a bore running
      into another feature)

    UNKNOWN is returned rather than guessed whenever the classifier
    itself errors, too: an extractor that cannot tell says so.

    Takes the caller's `classifier` rather than building its own.
    ``BRepClass3d_SolidClassifier`` does real work at construction — it
    indexes the whole shell — and end classification is per BORE, so a
    200-hole plate was paying for 200 of them. Reuse is the documented
    usage pattern (``Perform`` carries the per-query state) and it is what
    the face walk already does.
    """
    try:
        lo_open = _end_is_open(classifier, origin, direction, radius, t_lo, -1.0)
        hi_open = _end_is_open(classifier, origin, direction, radius, t_hi, +1.0)
    except Exception:  # noqa: BLE001 — an unclassifiable end is UNKNOWN, not a crash
        return HoleEndCondition.UNKNOWN, False, False

    if lo_open and hi_open:
        return HoleEndCondition.THROUGH, lo_open, hi_open
    if lo_open != hi_open:
        # Terminates in material at exactly one end. Whether that end is
        # FLAT is a separate structural question — see `_has_flat_bottom`.
        return HoleEndCondition.BLIND, lo_open, hi_open
    return HoleEndCondition.UNKNOWN, lo_open, hi_open


def _has_flat_bottom(faces, origin, direction, radius, t_bottom) -> bool:
    """True when a PLANAR face perpendicular to the axis caps the bore.

    A flat-bottom drill / end-mill is a different, slower cycle than a
    standard 118°/135° point, so Part C will want to price it
    differently. Detected structurally (a plane, normal parallel to the
    bore axis, sitting at the bore's closed end, covering the axis)
    rather than inferred — and a 118° point has no such face, so it
    correctly reports False.

    The lateral test is against the face's BOUNDING BOX, not against the
    plane's placement point: OCCT puts a cut face's surface origin
    wherever the boolean left it, which for an annular counterbore floor
    can be metres from the bore. The box is a property of the face.
    """
    bottom_pt = _point_on_axis(origin, direction, t_bottom)
    for face in faces:
        try:
            adaptor = BRepAdaptor_Surface(face)
            if adaptor.GetType() != GeomAbs_SurfaceType.GeomAbs_Plane:
                continue
            plane = adaptor.Plane()
            n = plane.Axis().Direction()
            normal = _unit((float(n.X()), float(n.Y()), float(n.Z())))
            if not _axes_parallel(normal, direction):
                continue
            p = plane.Location()
            plane_pt = (float(p.X()), float(p.Y()), float(p.Z()))
            # Distance from the bore's closed end to this plane, along
            # the axis. A cap sits AT that end.
            d = (
                plane_pt[0] - bottom_pt[0],
                plane_pt[1] - bottom_pt[1],
                plane_pt[2] - bottom_pt[2],
            )
            if abs(_dot(d, direction)) > 1e-6:
                continue
            # …and the face itself must reach the bore, not merely be a
            # far-away face of the part that happens to be coplanar.
            box = Bnd_Box()
            BRepBndLib.Add_s(face, box)
            if box.IsVoid():
                continue
            xmin, ymin, zmin, xmax, ymax, zmax = box.Get()
            slack = radius + RADIUS_TOL_MM
            if not (
                xmin - slack <= bottom_pt[0] <= xmax + slack
                and ymin - slack <= bottom_pt[1] <= ymax + slack
                and zmin - slack <= bottom_pt[2] <= zmax + slack
            ):
                continue
            return True
        except Exception:  # noqa: BLE001 — one odd face must not kill the scan
            continue
    return False


def _collect_faces(shape) -> List:
    """Every ``TopAbs_FACE`` of the shape, in OCCT's walk order.

    Factored out so the determinism fixture can hand the miner a
    deliberately REVERSED walk and prove that nothing downstream depends
    on the order (ADR-0112 S3). OCCT does not contractually guarantee
    this order across versions, so neither may we.
    """
    faces = []
    explorer = TopExp_Explorer(shape, TopAbs_FACE)
    while explorer.More():
        faces.append(TopoDS.Face_s(explorer.Current()))
        explorer.Next()
    return faces


def mine_cylindrical_holes(shape) -> List[LocatedHole]:
    """Mine located holes from a STEP shape. Never raises for geometry
    reasons — see the module docstring's failure posture.

    Returns a DETERMINISTICALLY SORTED list; see the module docstring.
    """
    if not _OCP_AVAILABLE:  # pragma: no cover — guarded by the caller too
        return []

    faces = _collect_faces(shape)

    # Edge -> adjacent faces, built once. This is how a bore finds the
    # face that caps each of its ends (B4); see `_true_axial_span`.
    ancestors = TopTools_IndexedDataMapOfShapeListOfShape()
    TopExp.MapShapesAndAncestors_s(shape, TopAbs_EDGE, TopAbs_FACE, ancestors)

    classifier = BRepClass3d_SolidClassifier(shape)
    cylinders: List[_CylFace] = []
    for face in faces:
        try:
            cyl = _face_to_cyl(face, classifier)
        except Exception:  # noqa: BLE001 — skip the face, keep the part
            continue
        if cyl is not None:
            cylinders.append(cyl)

    # Canonical order BEFORE grouping. Contiguity is a pairwise test
    # against a group's running span, so the order faces arrive in would
    # otherwise decide which of two adjacent segments a third joins.
    cylinders.sort(key=_CylFace.sort_key)

    groups: List[_BoreGroup] = []
    for cyl in cylinders:
        for group in groups:
            if group.accepts(cyl):
                group.add(cyl)
                break
        else:
            groups.append(_BoreGroup(cyl))

    holes: List[LocatedHole] = []
    for group in groups:
        try:
            # The sweep test, at last — on the REJOINED bore, not on the
            # faces it was cut into. See the module docstring.
            if not group.is_full_sweep():
                continue

            radius, origin, direction = group.radius, group.origin, group.direction
            t_lo, t_hi = _true_axial_span(group, ancestors, group.lo, group.hi)
            depth = t_hi - t_lo
            if depth <= 0.0:
                continue

            end_condition, lo_open, _hi_open = _classify_ends(
                classifier, origin, direction, radius, t_lo, t_hi
            )

            # Entry point + drill direction. For a blind hole the entry is
            # the OPEN end, so the axis points from that end into the
            # material. For a through (or unclassifiable) hole both ends
            # are equally entries and the CANONICAL frame decides, so that
            # the same part always answers the same way (S3).
            entry_t, bottom_t = t_lo, t_hi
            axis = direction
            if end_condition == HoleEndCondition.BLIND and not lo_open:
                entry_t, bottom_t = t_hi, t_lo
                axis = (-direction[0], -direction[1], -direction[2])

            flat_bottom = (
                _has_flat_bottom(faces, origin, direction, radius, bottom_t)
                if end_condition == HoleEndCondition.BLIND
                else False
            )

            holes.append(
                LocatedHole(
                    diameter_mm=2.0 * radius,
                    depth_mm=depth,
                    axis_unit=_wire_vec(axis),
                    entry_point_mm=_wire_vec(_point_on_axis(origin, direction, entry_t)),
                    end_condition=end_condition,
                    flat_bottom=flat_bottom,
                )
            )
        except Exception:  # noqa: BLE001 — drop the hole, keep the part
            continue

    # DETERMINISTIC ORDER — non-negotiable. OCCT's explorer order is not
    # contractually stable across versions, and `feature_graph_hash` +
    # every golden depend on byte-identical output for identical input.
    holes.sort(
        key=lambda h: (
            round(h.entry_point_mm[0], 6),
            round(h.entry_point_mm[1], 6),
            round(h.entry_point_mm[2], 6),
            round(h.diameter_mm, 6),
            round(h.depth_mm, 6),
        )
    )
    return holes
