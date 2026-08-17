"""Located-hole mining from a STEP B-rep, via OCCT/OCP.

**ADR-0112 Part B.** Turns "the part has four holes" into "the part has a
Ø6.0 hole 12 mm deep entering at (10, 10, 20) along −Z". The former
cannot be drilled-priced and cannot be toolpathed; the latter can be both.

No new dependency and no new process model: the same optional ``[step]``
extra, the same ``_load_step_shape`` reader, one extra face-walk inside
the caller's existing stdout-silencing discipline.

The algorithm
-------------

1. Walk every ``TopAbs_FACE`` of the solid.
2. Keep the cylindrical ones (``BRepAdaptor_Surface.GetType() ==
   GeomAbs_Cylinder``).
3. Reject cylinders whose material lies OUTSIDE the surface — that is the
   bar OD or a boss, not a hole. See :func:`_is_internal_cylinder`; this
   is correctness risk #1 in the ADR and has its own fixture.
4. Reject partial sweeps: a real bore covers a full 2π of circumference.
   A 90° sweep is a fillet, a slot end, or a split face.
5. Merge coaxial faces. One drilled hole is frequently several faces
   (split at a seam, or stepped). Without merging, a Ø8 through-hole
   reports as two Ø8 holes — a 2x drilling over-price. Correctness risk
   #2, also with its own fixture.
6. Classify the end condition; detect a flat bottom.
7. Sort deterministically before emitting.

Failure posture (deliberate, and the opposite of ``extract_step``'s)
--------------------------------------------------------------------

Hole mining must NEVER fail the extraction. Any exception in
:func:`mine_cylindrical_holes` is caught by the caller, ``located_holes``
comes back empty, and the part prices at today's hole-blind number rather
than the quote dying. ``extract_step`` fails LOUD on *shape* errors
because the geometry is then unusable; an empty hole list is a
known-conservative degradation with an established meaning (it is exactly
what every pre-v6 graph carries), whereas a corrupt hole list is not.

Determinism
-----------

The emitted order is contractual. OCCT's explorer order is not
guaranteed stable across versions, and the whole ``feature_graph_hash`` +
golden regime depends on byte-identical output for identical input, so
the final sort in :func:`mine_cylindrical_holes` is non-negotiable.
"""

from __future__ import annotations

import math
from typing import List, Optional, Sequence, Tuple

from aberp_cad_extract.feature_graph import HoleEndCondition, LocatedHole

try:
    from OCP.BRep import BRep_Tool
    from OCP.BRepAdaptor import BRepAdaptor_Surface
    from OCP.BRepClass3d import BRepClass3d_SolidClassifier
    from OCP.BRepTools import BRepTools
    from OCP.GeomAbs import GeomAbs_SurfaceType
    from OCP.gp import gp_Dir, gp_Pnt, gp_Vec
    from OCP.TopAbs import TopAbs_FACE, TopAbs_IN, TopAbs_ON, TopAbs_REVERSED
    from OCP.TopExp import TopExp_Explorer
    from OCP.TopoDS import TopoDS

    _OCP_AVAILABLE = True
except ImportError:  # pragma: no cover — mirrors extractors/step.py
    _OCP_AVAILABLE = False


# ── tolerances ───────────────────────────────────────────────────────────
#
# All three are geometric, not statistical. They are deliberately LOOSE
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

#: A cylindrical face must sweep at least this fraction of 2π to be a
#: bore rather than a fillet / slot end / split-cylinder sliver.
FULL_SWEEP_FRACTION: float = 0.999

#: Radii within this many mm are the same drill. Also the rounding used
#: when deciding whether a stepped bore is one hole or two.
RADIUS_TOL_MM: float = 1e-4

#: How far outside a hole's end to probe when classifying through/blind.
#: Must exceed OCCT's own point-classification fuzz but stay well under
#: any realistic wall thickness.
END_PROBE_MM: float = 1e-3


def _unit(v: Sequence[float]) -> Tuple[float, float, float]:
    """Normalise a 3-vector. Raises on a zero vector (a real OCCT axis is
    never zero, so this is a corrupt-input signal, not a normal path)."""
    n = math.sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2])
    if n == 0.0:
        raise ValueError("cannot normalise a zero-length axis vector")
    return (v[0] / n, v[1] / n, v[2] / n)


def _dot(a: Sequence[float], b: Sequence[float]) -> float:
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


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


class _CylFace:
    """A cylindrical face reduced to the numbers the merge step needs.

    ``t_min``/``t_max`` are the face's extent along the axis, measured as
    a signed distance from ``origin`` — so two faces of the same bore are
    directly comparable once re-expressed on a shared axis.
    """

    __slots__ = ("radius", "origin", "direction", "t_min", "t_max", "internal")

    def __init__(self, radius, origin, direction, t_min, t_max, internal):
        self.radius = radius
        self.origin = origin
        self.direction = direction
        self.t_min = t_min
        self.t_max = t_max
        self.internal = internal


def _is_internal_cylinder(face, adaptor) -> bool:
    """True when the MATERIAL is outside the cylinder — i.e. it's a hole.

    Correctness risk #1 from the ADR. A cylindrical face is a bore only
    if the solid lies on the *outside* of the surface; the same test run
    the other way describes a shaft OD or a cylindrical boss, and
    counting those as holes would invent drilling operations that never
    happen.

    OCCT gives this cheaply and exactly: the surface normal of a
    cylinder points radially OUTWARD from the axis, and a face's
    orientation flag says whether the solid is on the normal's side. For
    an outer surface (a bar OD) the material is on the inward side, so
    the face is FORWARD; for a bore the material is on the outward side,
    so the face is REVERSED. `tube_od_not_a_hole.step` pins both arms in
    one fixture.
    """
    del adaptor  # signature kept explicit; the decision is orientation-only
    return face.Orientation() == TopAbs_REVERSED


def _face_to_cyl(face) -> Optional[_CylFace]:
    """Reduce one face to a :class:`_CylFace`, or ``None`` if it isn't a
    full-sweep cylindrical bore face."""
    adaptor = BRepAdaptor_Surface(face)
    if adaptor.GetType() != GeomAbs_SurfaceType.GeomAbs_Cylinder:
        return None

    u_min, u_max, v_min, v_max = BRepTools.UVBounds_s(face)
    # U is the circumferential parameter (radians), V the axial one (mm).
    sweep = abs(u_max - u_min)
    if sweep < FULL_SWEEP_FRACTION * 2.0 * math.pi:
        # A partial sweep is a fillet, a slot end, or one half of a
        # cylinder split at a seam that the merge step could not have
        # rejoined into a full bore anyway. Not a drilled hole.
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
    origin = (float(loc.X()), float(loc.Y()), float(loc.Z()))
    direction = _unit((float(dvec.X()), float(dvec.Y()), float(dvec.Z())))

    # V is measured from the cylinder's own origin along its own axis, so
    # the V-bounds ARE the signed axial extent we want.
    t_min, t_max = float(min(v_min, v_max)), float(max(v_min, v_max))

    return _CylFace(
        radius=radius,
        origin=origin,
        direction=direction,
        t_min=t_min,
        t_max=t_max,
        internal=_is_internal_cylinder(face, adaptor),
    )


def _same_bore(a: _CylFace, b: _CylFace) -> bool:
    """Do these two faces belong to the SAME drilled hole?

    Requires all three of: equal radius, parallel axes, and coincident
    axis lines. Correctness risk #2 — merging too eagerly under-counts
    (under-price), so every condition is required, none is inferred.

    Note this deliberately does NOT merge a stepped bore (Ø10 counterbore
    over a Ø6 through-hole) into one hole: the radii differ, so they stay
    two entries. That is right for drilling — they are two operations
    with two tools.
    """
    if abs(a.radius - b.radius) > RADIUS_TOL_MM:
        return False
    if not _axes_parallel(a.direction, b.direction):
        return False
    return _axis_line_distance(a.origin, a.direction, b.origin) <= AXIS_POSITIONAL_TOL_MM


def _merge_group(group: List[_CylFace]) -> Tuple[float, Tuple[float, float, float], Tuple[float, float, float], float]:
    """Collapse coaxial faces into (radius, origin, direction, spans).

    Returns ``(radius, origin, direction, (t_lo, t_hi))`` re-expressed on
    the FIRST face's axis frame, so callers get one consistent parameter
    line for the whole merged bore.
    """
    base = group[0]
    lo, hi = base.t_min, base.t_max
    for other in group[1:]:
        # Re-express the other face's span on `base`'s axis. The axes are
        # already known coincident; only the sense and the origin offset
        # can differ.
        offset = _dot(
            (
                other.origin[0] - base.origin[0],
                other.origin[1] - base.origin[1],
                other.origin[2] - base.origin[2],
            ),
            base.direction,
        )
        sense = 1.0 if _dot(other.direction, base.direction) >= 0.0 else -1.0
        a = offset + sense * other.t_min
        b = offset + sense * other.t_max
        lo = min(lo, a, b)
        hi = max(hi, a, b)
    return base.radius, base.origin, base.direction, (lo, hi)


def _point_on_axis(
    origin: Sequence[float], direction: Sequence[float], t: float
) -> Tuple[float, float, float]:
    return (
        origin[0] + t * direction[0],
        origin[1] + t * direction[1],
        origin[2] + t * direction[2],
    )


def _classify_ends(
    solid, origin, direction, t_lo, t_hi
) -> Tuple[HoleEndCondition, bool]:
    """Decide through/blind/unknown, and whether the bottom is flat.

    Probes just OUTSIDE each end of the bore span with OCCT's solid
    classifier:

    - both probes outside the solid  -> the bore opens at both ends -> THROUGH
    - exactly one probe inside       -> it terminates in material    -> BLIND
    - both probes inside             -> ambiguous (an internal cavity,
      or a bore that runs into another feature) -> UNKNOWN

    UNKNOWN is returned rather than guessed whenever the classifier
    itself errors, too: an extractor that cannot tell says so.
    """
    try:
        classifier = BRepClass3d_SolidClassifier(solid)

        def inside(t: float) -> bool:
            px, py, pz = _point_on_axis(origin, direction, t)
            classifier.Perform(gp_Pnt(px, py, pz), 1e-7)
            return classifier.State() in (TopAbs_IN, TopAbs_ON)

        lo_inside = inside(t_lo - END_PROBE_MM)
        hi_inside = inside(t_hi + END_PROBE_MM)
    except Exception:  # noqa: BLE001 — an unclassifiable end is UNKNOWN, not a crash
        return HoleEndCondition.UNKNOWN, False

    if not lo_inside and not hi_inside:
        return HoleEndCondition.THROUGH, False
    if lo_inside != hi_inside:
        # Blind. Flat-bottom detection is left to the caller's face scan;
        # see `_has_flat_bottom`.
        return HoleEndCondition.BLIND, False
    return HoleEndCondition.UNKNOWN, False


def _has_flat_bottom(faces, origin, direction, radius, t_bottom) -> bool:
    """True when a PLANAR face perpendicular to the axis caps the bore.

    A flat-bottom drill / end-mill is a different, slower cycle than a
    standard 118°/135° point, so Part C will want to price it
    differently. Detected structurally (a plane, normal parallel to the
    bore axis, sitting at the bore's closed end, within the bore radius)
    rather than inferred.
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
            # …and it must be laterally within the bore, not a far-away
            # face of the part that happens to be coplanar.
            if _axis_line_distance(origin, direction, plane_pt) > radius + RADIUS_TOL_MM:
                continue
            return True
        except Exception:  # noqa: BLE001 — one odd face must not kill the scan
            continue
    return False


def mine_cylindrical_holes(shape) -> List[LocatedHole]:
    """Mine located holes from a STEP shape. Never raises for geometry
    reasons — see the module docstring's failure posture.

    Returns a DETERMINISTICALLY SORTED list; see the module docstring.
    """
    if not _OCP_AVAILABLE:  # pragma: no cover — guarded by the caller too
        return []

    faces = []
    explorer = TopExp_Explorer(shape, TopAbs_FACE)
    while explorer.More():
        faces.append(TopoDS.Face_s(explorer.Current()))
        explorer.Next()

    cylinders: List[_CylFace] = []
    for face in faces:
        try:
            cyl = _face_to_cyl(face)
        except Exception:  # noqa: BLE001 — skip the face, keep the part
            continue
        if cyl is not None and cyl.internal:
            cylinders.append(cyl)

    # Merge coaxial, equal-radius faces into single bores.
    groups: List[List[_CylFace]] = []
    for cyl in cylinders:
        for group in groups:
            if _same_bore(group[0], cyl):
                group.append(cyl)
                break
        else:
            groups.append([cyl])

    holes: List[LocatedHole] = []
    for group in groups:
        try:
            radius, origin, direction, (t_lo, t_hi) = _merge_group(group)
            depth = t_hi - t_lo
            if depth <= 0.0:
                continue

            end_condition, _ = _classify_ends(shape, origin, direction, t_lo, t_hi)

            # Entry point + drill direction. For a blind hole the entry is
            # the OPEN end, so the axis points from open end into the
            # material; probe which end is open.
            entry_t, bottom_t = t_lo, t_hi
            axis = direction
            if end_condition == HoleEndCondition.BLIND:
                try:
                    classifier = BRepClass3d_SolidClassifier(shape)
                    px, py, pz = _point_on_axis(origin, direction, t_lo - END_PROBE_MM)
                    classifier.Perform(gp_Pnt(px, py, pz), 1e-7)
                    lo_is_material = classifier.State() in (TopAbs_IN, TopAbs_ON)
                except Exception:  # noqa: BLE001
                    lo_is_material = False
                if lo_is_material:
                    # t_lo is buried; the open end is t_hi, so the drill
                    # travels in -direction.
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
                    axis_unit=list(axis),
                    entry_point_mm=list(_point_on_axis(origin, direction, entry_t)),
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
