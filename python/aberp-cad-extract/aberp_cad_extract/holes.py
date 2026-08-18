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
2. Reject cylinders whose material lies INSIDE the surface — a bar OD or
   a boss. One test, the face orientation flag; see :func:`_is_bore_face`
   for why the second test this used to carry was removed.
3. Sort the survivors into a canonical order, then group them into bores:
   equal radius, coincident axis line, **and axially contiguous**. The
   contiguity arm is what stops two holes 80 mm apart in two separate
   walls from collapsing into one 100 mm-deep hole (§ "Contiguity").
4. Reject any group that does not cover a FULL 2π of circumference —
   *after* the grouping, so a bore split into quarter-faces at a seam is
   rejoined before it is judged. A group still short of 2π is a fillet,
   a slot end, or a lone split sliver (§ "Why the sweep test moved").
   This is the ONLY thing separating a concave fillet from a bore.
5. Walk the faces that CAP the bore's ends. That single walk answers
   BOTH of the remaining questions — where each end really is (§ "Why
   UVBounds is not the answer") and whether it opens to air (§ "Open or
   capped").
6. Detect a flat bottom.
7. Emit in a deterministic, frame-independent order.

Nothing in that list asks "is this point inside the solid?". The miner
used to, in two places, and both are gone — see § "Open or capped" for
why the answer is structural rather than sampled, and what it bought.

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
  concave side.

So the test now runs on the merged group, and it is a genuine
CIRCUMFERENTIAL UNION rather than a per-face sweep width: four 90° faces
that tile the circle are one bore, four 90° faces that do not are not.

And since the ADR-0112 round-2 corrections it is the ONLY thing doing
that job. :func:`_is_bore_face`'s surviving orientation arm rules out a
bar OD and nothing else — an internal-corner fillet is REVERSED, has its
axis in air, and has its material outside the cylinder, which is a bore
in every respect except that it sweeps 90° instead of 360°. The guard
that used to sit alongside this one is gone because what it really
caught was a real hole (see :func:`_is_bore_face`), so this test is now
load-bearing rather than redundant: weaken :data:`FULL_SWEEP_FRACTION`
and ``concave_fillet_step.step``, ``bore_beside_concave_fillet.step``
and the in-memory filleted block all go red.

Why UVBounds is not the answer (ADR-0112 adversarial, B4 + N1)
---------------------------------------------------------------

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
floor, a flat bottom, a shaft's own OD. The point where the bore's AXIS
meets that cap's unbounded surface is the exact entry (or exit): it is
where a drill first touches, and it is right at any trim angle, because
the trim curve's centre and the axis-surface intersection are the same
point.

Note what is deliberately NOT done: intersecting the axis with the SOLID.
That was the first attempt and it finds nothing, because a bore's axis
runs down the middle of the hole and never touches the part's skin at
either opening. Only the cap's unbounded surface has the answer.

CORRECTED (ADR-0112 adversarial round 2, N1). The first cut of this fix
only intersected PLANAR caps and let every other cap keep the parametric
bound. That left the identical defect alive on any curved cap, and made
the contract self-inconsistent besides: the same hole measured
differently depending only on whether the face next door happened to be
flat. A routine cross-drilled shaft — a Ø8 bore through a Ø30 bar, 10 mm
off the bar's centreline — came back 22.96 % too deep (27.4955 against a
true 22.3607) with its entry at x=-13.7477 where the bar's surface is at
x=-11.1803: 2.57 mm out in mid-air, on a part every shop makes.
:func:`_cap_axis_intersections` now handles ANY cap surface —
cylindrical, conical, toroidal, B-spline — via ``GeomAPI_IntCS``, with an
analytic fast path for planes so the planar answers stay bit-for-bit what
they were.

Two properties of the general case that the planar one did not have, and
that the code has to answer for:

- A curved surface can meet a line MORE THAN ONCE. A bore straight
  through a round bar cuts its OD at both ends, and both roots are inside
  the bore's parametric span. The root nearest the EDGE that led to the
  face is the one that belongs to that end; the others are somebody
  else's end, or nobody's.
- A line can meet a surface without CROSSING it, and a bore only ends
  where it crosses. A 118° point's neighbour is a CONE, and a line down
  the cone's own axis meets it at exactly one point, the APEX — which the
  axis touches and carries straight through, still in the same void. So
  the cone does not move the end, and the cylinder's own parametric end
  stands: the full-diameter depth, and the number on the drawing.

CORRECTED AGAIN (ADR-0112 adversarial round 3, blockers 1 and 2). Round 2
decided both of those by WHERE THE ROOT FELL — it had to lie inside the
bore's parametric span — and that one test was standing in for two
different questions it could not actually answer. "Is this a cap at all?"
was answered correctly for a drill point only by the accident that a
point's apex falls BELOW the bore. And "is this cap this end's?" was
answered by a bound that is not the superset it was documented to be.
The luck ran out in both directions at once:

- **Blocker 1, the countersink.** The identical cone at the MOUTH of a
  bore puts its apex INSIDE the span, where the span test admitted it as
  the cap. A Ø8 through-bore with a 90° countersink measured 13.0 deep
  against a true 17.0 — 23.5 % of the hole gone — and a blind one
  reported its ENTRY four millimetres inside solid metal. Worse, the end
  condition is read off the cap's normal and a cone's apex HAS no normal,
  so what OCCT returned there was whichever generatrix its intersector
  landed on: the 90° hole came back THROUGH and the 120° hole BLIND, for
  the same shape of part.

- **Blocker 2, the dome.** A doubly-curved CONVEX cap puts its crown
  OUTSIDE the span, where the span test refused it. A dome's trim curve
  never reaches the crown the axis leaves through, so a Ø8 bore through a
  Ø40 ball measured 39.1918 against a true 40.0 and entered 0.4 mm inside
  the metal. A singly-curved cap hid this for a whole round — a round bar
  does not curve along its own axis, so its trim runs right up to the
  crown and the root landed ON the old bound. It takes curvature in BOTH
  directions to open the gap. At its worst the bore VANISHED: through a
  torus wall, clipping the convex crown left the far end with no root but
  the near end's concave one, both ends resolved to the same place, the
  depth came out zero and a zero-deep bore is dropped. Zero holes on a
  part with one.

Both are fixed by asking the geometry the question the span was standing
in for: does the axis CROSS this surface here? :func:`_crossing_normal`
answers it, the drill point and the countersink now fall out of that ONE
rule rather than out of their positions, and the bound — no longer doing
work it was not fit for — relaxes to what it can honestly claim: a root
past the bore's FAR end belongs to the other end, or to neither.

Open or capped (ADR-0112 adversarial, B2 + N2)
------------------------------------------------

An end is OPEN when the part's material stops there and a drill would
break out. Getting this wrong is money in both directions: a blind hole
read as THROUGH drops the peck cycles, and its reported entry is at the
CLOSED end, a coordinate no machine can reach.

B2 killed the original single-point probe: a 118° drill point leaves the
bore's AXIS in void for the whole height of the cone, so a classifier
sample just past the end read "open" on a hole that is plainly blind.
Round 1 answered that by sampling the whole cross-section — a ring of
eight points near the bore wall, at two depths, at both ends. It worked,
and it cost 36 ``BRepClass3d_SolidClassifier`` queries per bore on top of
one per candidate face. Re-measured here against the round-1 module on
the same fixtures and the same box: a 600-hole plate took 29.88 s of the
wrapper's 30 s subprocess budget and a 1000-hole plate 69.62 s, putting
the crossover at roughly 600 holes. A part that is merely large is not a
part that is wrong, so that was an availability defect (N2). The ring
count was also a free parameter no fixture pinned — 8 could have been 3
or 40 and nothing would have moved.

Both problems have the same root: the question was being SAMPLED when
the topology already knows the answer. The cap-face walk above has
already found the exact face across each end. That face carries the
solid's outward normal, and "does the material stop here?" is just
whether that normal points OUT of the end:

    open  ⟺  outward_normal · outward_axial_direction > 0

A flat top face at a through-exit gives +1. A flat blind bottom gives -1
— its outward normal points back UP the bore, into the void. A 118°
point's cone gives -sin(59°), for the same reason, which is exactly the
case the axis probe could not see and needs no probe at all. A
cross-drilled shaft's OD gives the cosine of the breakout angle. No
sampling, no ring, no free parameter, and no point-in-solid query
anywhere in the miner — ``BRepClass3d_SolidClassifier``, whose
construction indexes the whole shell, is gone entirely.

Where an end has no in-bounds cap intersection (the drill point), the
verdict comes from the neighbour faces at that end evaluated on the
shared edge instead, and EVERY one of them must say "out" for the end to
be open. Where there is no neighbour at all, or the normal is undefined,
or the dot product is within :data:`CAP_OUTWARD_MIN_COS` of zero (a
grazing cap is not a breakout), the end is CAPPED. Ambiguity resolves
towards blind on purpose: an over-price is visible in the reasoning log
and an under-price is not.

What N2 actually cost, measured
---------------------------------

Deleting the ring was necessary and was not sufficient — it roughly
halved the bill and left a crossover of ~1080 holes, which clears the
1000-hole target by nothing worth calling margin. Profiling the rest
found two costs that had nothing to do with geometry and everything to
do with how OCCT is reached from Python, both fixed here and both
documented at the code: iterating the ancestor map's returned
``TopTools_ListOfShape`` (see :class:`_EdgeFaces`) and the surface
adaptor's default UV restriction (see :func:`_adaptor`). Same box, same
fixtures, end-to-end wall clock of the subprocess the 30 s budget
actually covers:

===================  ==========  ==========
part                 round 1     round 2
===================  ==========  ==========
600-hole plate         29.88 s      1.74 s
1000-hole plate        69.62 s      3.15 s
3000-hole plate             --     18.69 s
2000-hole shaft (a)         --     33.47 s
===================  ==========  ==========

(a) every cap curved, so every end takes ``GeomAPI_IntCS`` rather than
the analytic plane path — the worst case for the N1 work.

Round 3's crossing test (:func:`_crossing_normal`) adds one
``GeomLProp_SLProps`` per CURVED root, and nothing at all to a planar
cap. Re-measured on the shape that bill lands hardest on — a bar with
every cap curved, so every end takes the general path — mining goes
0.037 s -> 0.038 s at 150 bores and 0.093 s -> 0.098 s at 300, about
5 %. It buys a correct answer on a countersink and a dome, and it does
not move the crossover: mining is ~0.3 ms per bore either way, against a
STEP READER that is already the whole of the bill at these sizes.

Crossover moves from ~600 holes to ~3800 (planar caps) and ~1800
(all-curved). The miner is no longer what spends the budget at those
sizes: of the 3000-hole plate's 18.69 s, 13.75 s is OCCT's STEP READER
and 3.78 s is mining; of the curved 2000-hole bar's 33.47 s, 28.42 s is
the reader and 1.83 s is mining. Which is why ``DEFAULT_TIMEOUT`` is left
at 30 s — raising it would buy headroom for the reader, not for this
module, and that case should be argued from a measurement of the reader.

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
machine-readable marker on stderr that the Rust wrapper fails on. That
path — and only that path — classifies ``Permanent``.

TAKING TOO LONG IS A DIFFERENT PATH, and the round-1 commit conflated
them (ADR-0112 adversarial round 2, correction to N2). A miner that
overruns the subprocess deadline never writes the sentinel and never
exits: the wrapper kills the child and returns
``ExtractError::Timeout``, whose ``Display`` is "subprocess exceeded
timeout of 30s". ``classify_failure`` matches "timeout" only under
``stage == "post"``, so at ``stage == "extract"`` that reason falls
through every rule to the default — ``FailureKind::Unknown``, NOT
``Permanent``. Bounded and safe rather than wrong: ``Unknown`` auto-
retries at most ``UNKNOWN_AUTO_RETRY_CAP`` (3) times and then freezes
pending an operator click, so a part that is simply too big burns three
deadlines and stops. It is still an availability defect for the customer
whose part it is, which is why the cost of this walk is a correctness
concern and not only a tuning one; see § "Open or capped" and
``crates/aberp-cad-extract-wrapper``'s ``DEFAULT_TIMEOUT``.

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
    from OCP.BRep import BRep_Tool
    from OCP.BRepAdaptor import BRepAdaptor_Curve, BRepAdaptor_Surface
    from OCP.BRepBndLib import BRepBndLib
    from OCP.BRepTools import BRepTools
    from OCP.Geom import Geom_Line
    from OCP.GeomAbs import GeomAbs_SurfaceType
    from OCP.GeomAPI import GeomAPI_IntCS, GeomAPI_ProjectPointOnSurf
    from OCP.GeomLProp import GeomLProp_SLProps
    from OCP.gp import gp_Ax1, gp_Dir, gp_Pnt
    from OCP.TopAbs import TopAbs_EDGE, TopAbs_FACE, TopAbs_REVERSED
    from OCP.TopExp import TopExp_Explorer
    from OCP.TopoDS import TopoDS
    from OCP.TopTools import TopTools_IndexedMapOfShape

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

#: How far the cap face's outward normal must lean along the bore's own
#: axis before that end counts as OPEN — a cosine, so 1e-6 is about
#: 0.006° off tangent.
#:
#: This is a degeneracy floor, not a tuning knob. A cap whose normal is
#: perpendicular to the bore axis is a surface the bore runs ALONG rather
#: than breaks OUT of, and the sign of the dot product there is float
#: noise deciding between through and blind. Anything at or under it
#: reads CAPPED, which is the over-price direction and therefore the
#: visible one. Every real case is orders of magnitude clear of it: a
#: flat face square to the bore gives 1.0, a 45° angled entry 0.71, a
#: 118° drill point -0.86.
CAP_OUTWARD_MIN_COS: float = 1e-6

#: Below this RATIO between the two surface-derivative magnitudes at a
#: point, that point sits on a DEGENERATE isoline — a parametric line
#: that has collapsed to a single point. A cone's apex and a sphere's
#: pole are both such points, and the miner meets both: a countersink's
#: cone and a domed cap are each swept about the bore's own axis, so the
#: axis lands exactly on the degeneracy.
#:
#: A degeneracy floor, not a tuning knob, and measured with enormous
#: margin on both sides. At a real degeneracy OCCT reports the collapsed
#: derivative at ~1e-16 against ~1e0 for its partner — a ratio of 1e-16.
#: At any regular point the two are within a few orders of magnitude of
#: each other. Nothing real lives within nine orders of 1e-9.
DEGENERATE_ISOLINE_RATIO: float = 1e-9

#: How far out of ONE PLANE, as a sine, the isoline tangents around a
#: collapsed isoline may lie before that point is ruled to have no
#: tangent plane at all.
#:
#: This is the whole of the countersink fix, so it is worth stating what
#: it separates. Every curve that runs INTO a smooth point leaves it
#: along a tangent, and at a smooth point all of those tangents lie in
#: one plane — the tangent plane. Sweep a sphere's pole and the meridian
#: tangents sweep the equatorial plane: coplanar, so there is a tangent
#: plane, so the axis genuinely crosses the surface there. Sweep a cone's
#: apex and the generatrices sweep a CONE: not coplanar, so there is no
#: tangent plane, so the axis only TOUCHES the apex, and a touch caps
#: nothing.
#:
#: Asking the tangents rather than the normal is what makes this work on
#: an imported B-spline. OCCT's ``Normal()`` is ``D1U ^ D1V``, and on a
#: collapsed isoline one of those is zero, so the normal it reports is
#: noise crossed with a real vector: at the pole of a NURBS-converted
#: sphere it comes back as (0, -0.214, 0.977) and (-0.674, -0.026, 0.738)
#: at neighbouring parameters, on a surface whose true normal is
#: (0, 0, 1) everywhere along that line. Raising the derivative order
#: does not rescue it. The surviving first derivative is exact, so this
#: test uses only that, and recovers the true normal as the tangents'
#: own plane.
#:
#: The margin is not close, and it does not depend on any step size,
#: because nothing here is a limit taken numerically. Measured: an
#: analytic sphere's pole 1.2e-16, a NURBS sphere's pole 2.0e-16. A 90°
#: countersink 8.2e-01, a 120° countersink 7.7e-01, and even an absurd
#: 170°-included cone 1.7e-01. Ten orders of margin below, five above.
#: And it degrades the right way: a cone so shallow that its tangents are
#: coplanar to within this floor is geometrically a flat cap, and reading
#: it as one is right.
#:
#: Used twice, for the same kind of question: two tangents this close to
#: PARALLEL span no plane to test against either.
DEGENERATE_COPLANARITY_TOL: float = 1e-6

#: How many directions of approach a degenerate point is probed from,
#: spaced evenly over the collapsed isoline's full parametric range.
#:
#: Three is the minimum that can test coplanarity at all: two tangents
#: DEFINE the plane and the third is the first that can fall out of it.
#: Four is the smallest EVENLY SPACED set with a spare, so a degeneracy
#: whose first two probes happen to land parallel — spanning no plane —
#: still has a pair left to define one. It costs four surface
#: evaluations at a point the miner reaches once per countersink.
DEGENERATE_PROBE_DIRECTIONS: int = 4


def _adaptor(face):
    """A surface adaptor for ``face`` that does NOT compute its UV
    restriction.

    ``BRepAdaptor_Surface(face)`` defaults to ``Restriction=True``, which
    calls ``BRepTools::UVBounds`` and therefore walks every edge of the
    face. Nothing here ever reads those bounds — the miner wants the
    surface's TYPE and its underlying geometry, and where it wants the
    trimmed extent it asks ``BRepTools.UVBounds_s`` directly, once, in
    :func:`_face_to_cyl`.

    Paying for the restriction anyway is quadratic on exactly the parts
    that matter, because a plate's top face carries one wire per hole:
    3.6 ms per construction on a 2000-hole plate against 0.5 µs without,
    a factor of 7300, several times per bore. It was 29 s of a 39 s run.
    """
    return BRepAdaptor_Surface(face, False)


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
    span is resolved per bore in :func:`_walk_caps`.

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
        #: :func:`_walk_caps`. Deliberately absent from
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


def _is_bore_face(face) -> bool:
    """True when this cylindrical face carries its material OUTWARD.

    Correctness risk #1 from the ADR. A cylinder's surface normal points
    radially outward from the axis, and the face's orientation flag says
    which side the solid is on. A bar OD carries its material inward, so
    the face is FORWARD; a bore carries it outward, so the face is
    REVERSED. ``tube_od_not_a_hole.step`` pins it — delete this and the
    Ø40 outside of every turned part becomes a Ø40 drilling operation.

    This is a NECESSARY condition, not a sufficient one, and it is not
    asked to be sufficient: an internal-corner fillet is REVERSED too and
    is separated from a bore one step later, by the post-merge sweep
    union, which is where that job belongs (§ "Why the sweep test moved").

    REMOVED (ADR-0112 adversarial round 2, correction 1): a second arm
    that also required the bore's own axis to be in VOID. Round 1 shipped
    it as redundant belt-and-braces against convex fillets and recorded
    that no committed STEP file could pin it, on the argument that a
    well-formed solid cannot present a face that is REVERSED, sweeps a
    full 2π, and has material on its own axis. That argument is false —
    a plain Ø30 counterbored recess with a boss standing on its
    centreline is well formed, legal STEP, and does exactly that — and
    what the arm does on such a part is DROP A REAL BORE. It was a false
    negative wearing a guard's coat, on the under-count side, which is
    the side nobody sees. The concave-fillet sweep-union separation is
    the real guard and it stands alone; ``bore_beside_concave_fillet.step``
    pins that removing this arm does not resurrect a phantom fillet hole
    and does not drop the genuine bore beside it, and
    ``bore_into_fillet.step`` pins a bore that breaks OUT through a
    fillet surface.
    """
    return face.Orientation() == TopAbs_REVERSED


def _face_to_cyl(face) -> Optional[_CylFace]:
    """Reduce one face to a :class:`_CylFace`, or ``None`` if it is not a
    cylindrical face that bounds a hole.

    NOTE what is deliberately NOT tested here: how much of the
    circumference the face covers. A quarter-face may be one quarter of a
    real bore, and only the merged group knows. See the module docstring.
    """
    # Cheapest possible rejection, and it applies to every face in the
    # part rather than only the cylindrical ones — so it goes before the
    # adaptor is even constructed. On a 600-hole part the walk sees tens
    # of thousands of faces and roughly half leave here.
    if not _is_bore_face(face):
        return None

    adaptor = _adaptor(face)
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


def _edge_mid_point(edge) -> Optional[Tuple[float, float, float]]:
    """A point at the middle of this edge's parameter range.

    Only ever used as a place to ASK a neighbouring surface which way its
    normal points, never as a measurement, so the parametric middle is
    good enough and its being off the geometric centre does not matter.
    """
    try:
        curve = BRepAdaptor_Curve(edge)
        u0, u1 = float(curve.FirstParameter()), float(curve.LastParameter())
        p = curve.Value(0.5 * (u0 + u1))
    except Exception:  # noqa: BLE001 — an unreadable edge simply has no point
        return None
    return (float(p.X()), float(p.Y()), float(p.Z()))


def _plane_axis_intersection(
    face, origin, direction
) -> Optional[Tuple[float, Tuple[float, float, float]]]:
    """Where the bore's axis meets this face's (unbounded) plane, and the
    plane's normal there.

    ``None`` when the face is not planar, or when the axis lies so nearly
    IN the plane that the intersection is numerically meaningless — a
    grazing cap is not a cap.

    Kept as a separate ANALYTIC path even though
    :func:`_cap_axis_intersections` would handle a plane too, and
    deliberately: it is one dot product and one divide against a general
    intersector's iterative solve, planar caps are the overwhelming
    majority of every real part, and — the reason it is not merely an
    optimisation — it keeps every planar answer bit-for-bit identical to
    what it was before the curved-cap generalisation (N1) landed. A fix
    for curved caps must not move a single planar number.
    """
    adaptor = _adaptor(face)
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
    return _dot(to_plane, normal) / denom, normal


def _isoline_tangent(surface, u, v, along_u) -> Optional[Tuple[float, float, float]]:
    """Unit tangent of the isoline that is NOT collapsed at ``(u, v)``.

    ``along_u`` says the collapsed isoline is the one traced by varying
    U, so the surviving tangent is ``D1V``, and vice versa.
    """
    props = GeomLProp_SLProps(surface, u, v, 1, 1e-7)
    d = props.D1V() if along_u else props.D1U()
    vec = (float(d.X()), float(d.Y()), float(d.Z()))
    if math.sqrt(vec[0] ** 2 + vec[1] ** 2 + vec[2] ** 2) <= 0.0:
        return None
    return _unit(vec)


def _degenerate_point_normal(
    surface, u, v, along_u, lo, hi, w_pole, w_min, w_max
) -> Optional[Tuple[float, float, float]]:
    """The tangent-plane normal at a point on a COLLAPSED isoline, or
    ``None`` when the point has no tangent plane at all.

    A collapsed isoline is a parametric line that is a single point in
    space — a cone's apex, a sphere's pole, the seam of a B-spline
    stitched shut. The miner meets them because a countersink's cone and
    a domed cap are both swept about the bore's OWN axis, so the axis
    lands exactly on the degeneracy rather than near it.

    The surface has a tangent plane there iff every curve arriving at the
    point arrives in ONE plane. Those arrivals are exactly the
    non-collapsed isolines, sampled around the collapsed one at
    :data:`DEGENERATE_PROBE_DIRECTIONS` evenly spaced parameters, and
    their coplanarity is the whole test — see
    :data:`DEGENERATE_COPLANARITY_TOL` for what it separates, why it is
    asked of the TANGENTS rather than of OCCT's own reported normal, and
    the measured margin.

    The plane's normal is returned because it is the honest normal at
    that point, and the caller needs one: it is what decides whether the
    end opens to air, and at a B-spline pole it is a vector OCCT will not
    give you any other way.

    Its SENSE has to be OCCT's own, though, because what the caller does
    with it is flip it for a REVERSED face — a convention that means
    nothing unless the vector started out on the ``D1U ^ D1V`` side. A
    cross product of two sampled tangents has no such allegiance, and
    taking its sense on trust reads a domed through-hole as BLIND at
    whichever of its two poles the parametrisation runs away from.

    The correction is exact rather than sampled. Write the surface near
    the collapsed line, whose parameter is ``w_pole``, as
    ``P(t, w) ≈ P0 + (w - w_pole)·T(t)`` with ``T`` the surviving
    tangent. Then the collapsed derivative is
    ``(w - w_pole)·dT/dt``, so

        D1U ^ D1V  ∝  -(w - w_pole) · (T ^ dT/dt)

    while the tangents sampled here, taken in increasing ``t``, give
    ``T ^ dT/dt`` with a PLUS sign. The two therefore agree exactly when
    ``w - w_pole`` is negative — that is, when the surface lies BELOW the
    collapsed line, which is to say when the line sits at the top of the
    range. A sphere's north pole is at its ``v`` maximum and agrees; its
    south pole is at the minimum and is opposite. So the sense is read
    off which end of the range the degeneracy sits at, and that is all.
    """
    tangents = []
    for k in range(DEGENERATE_PROBE_DIRECTIONS):
        t = lo + (hi - lo) * k / DEGENERATE_PROBE_DIRECTIONS
        tangent = (
            _isoline_tangent(surface, t, v, along_u)
            if along_u
            else _isoline_tangent(surface, u, t, along_u)
        )
        if tangent is None:
            return None
        tangents.append(tangent)

    normal = None
    for other in tangents[1:]:
        span = _cross(tangents[0], other)
        spread = math.sqrt(span[0] ** 2 + span[1] ** 2 + span[2] ** 2)
        if spread > DEGENERATE_COPLANARITY_TOL:
            normal = _unit(span)
            break
    if normal is None:
        # Every arrival is along the same line. That is a surface pinched
        # to a spike, not a cap, and there is no plane to test the rest
        # against; unproven reads "not a cap".
        return None

    for tangent in tangents:
        if abs(_dot(tangent, normal)) > DEGENERATE_COPLANARITY_TOL:
            return None

    if abs(w_pole - w_max) > abs(w_pole - w_min):
        normal = (-normal[0], -normal[1], -normal[2])
    return normal


def _crossing_normal(surface, u, v, direction) -> Optional[Tuple[float, float, float]]:
    """The surface normal where the bore's axis CROSSES ``surface`` at
    ``(u, v)`` — or ``None`` when the axis only TOUCHES it there.

    ``GeomAPI_IntCS`` answers "the line meets the surface here". That is
    not the same question as "the bore ends here", and the difference is
    a countersink (ADR-0112 adversarial round 3, blocker 1).

    A cap ends a bore because material stops at it: the axis is on one
    side of the surface before it and the other side after. That needs a
    surface with a TANGENT PLANE at the root, met at a non-zero angle.
    Two ways a root can fail to be one, and both are met in practice:

    - **Grazing.** The normal is perpendicular to the axis, so the bore
      runs ALONG the surface rather than out of it, and which side it is
      on is float noise. Refused at :data:`CAP_OUTWARD_MIN_COS` — the
      same floor, for the same reason, that
      :func:`_plane_axis_intersection` has always applied to a grazing
      plane. This generalises that rule to every surface instead of
      leaving it to the one that happened to have an analytic path.

    - **A conical point.** The root sits on a collapsed isoline and the
      surface has no tangent plane there at all. This is the countersink,
      and it is the case round 2 got wrong: a coaxial cone meets the
      bore's axis at exactly one point, its APEX, and OCCT reports that
      as a perfectly ordinary intersection with a perfectly ordinary
      normal. It is not ordinary. The apex is a point the axis touches
      and carries on through the same void it was already in, and the
      normal OCCT hands back there is whichever generatrix the
      intersector happened to land on — a determinism hazard on top of a
      wrong answer, and the reason two countersinks that differ only in
      included angle came back one THROUGH and one BLIND.

    A degeneracy is NOT by itself a disqualification, and that is the
    whole delicacy of this test. A domed cap — a sphere, a barrel, a
    NURBS bulge swept about the bore's own axis — puts its POLE on the
    axis too, and a pole is degenerate in exactly the same parametric
    way. The difference is geometric rather than parametric, and
    :func:`_degenerate_point_normal` is where it is drawn.

    What this buys, beyond the countersink: the drill point now falls out
    of the SAME rule. Round 2 discarded a 118° point's apex because it
    lay outside the bore's parametric span — which was true of a drill
    point and an accident of where the apex happened to fall. The
    identical cone at the MOUTH of the same bore falls INSIDE the span
    and was kept. Both are conical points, neither is a cap, and neither
    is special-cased now.
    """
    props = GeomLProp_SLProps(surface, u, v, 1, 1e-7)
    d_u, d_v = props.D1U(), props.D1V()
    mag_u = math.sqrt(d_u.X() ** 2 + d_u.Y() ** 2 + d_u.Z() ** 2)
    mag_v = math.sqrt(d_v.X() ** 2 + d_v.Y() ** 2 + d_v.Z() ** 2)

    u_min, u_max, v_min, v_max = surface.Bounds()
    if mag_u <= DEGENERATE_ISOLINE_RATIO * mag_v:
        lo, hi, along_u = u_min, u_max, True
    elif mag_v <= DEGENERATE_ISOLINE_RATIO * mag_u:
        lo, hi, along_u = v_min, v_max, False
    else:
        # A regular point. OCCT's own normal is trustworthy here.
        if not props.IsNormalDefined():
            return None
        n = props.Normal()
        normal = _unit((float(n.X()), float(n.Y()), float(n.Z())))
        return normal if abs(_dot(direction, normal)) > CAP_OUTWARD_MIN_COS else None

    if not (math.isfinite(lo) and math.isfinite(hi)) or hi <= lo:
        # No parametric range to sweep, so no set of arrivals to test for
        # coplanarity and no tangent plane to be had. Unproven reads "not
        # a cap", which loses the root and leaves the bore's own
        # parametric end standing — the conservative, over-price
        # direction. (The range swept is always the COLLAPSED parameter's,
        # which is the "around" one and finite on every surface of
        # revolution; a cone's unbounded range is its v, and v is the one
        # that survives.)
        return None

    w_pole, w_min, w_max = (v, v_min, v_max) if along_u else (u, u_min, u_max)
    normal = _degenerate_point_normal(
        surface, u, v, along_u, lo, hi, w_pole, w_min, w_max
    )
    if normal is None:
        return None
    return normal if abs(_dot(direction, normal)) > CAP_OUTWARD_MIN_COS else None


def _cap_axis_intersections(
    face, origin, direction
) -> List[Tuple[float, Tuple[float, float, float]]]:
    """Every point where the bore's axis CROSSES this face's UNBOUNDED
    surface, as an axial parameter paired with the surface normal there,
    for a cap of ANY shape (N1).

    The planar case delegates to the analytic path above. Everything else
    — a shaft's OD, a spherical or barrelled top, a fillet, an imported
    B-spline skin — goes through ``GeomAPI_IntCS``, which intersects a
    ``Geom_Line`` with the face's underlying surface.

    UNBOUNDED is the load-bearing word, and it is why this asks the
    SURFACE rather than the face: a bore's axis exits through the middle
    of the opening the bore itself cut, so the trimmed cap face has a
    hole exactly where the axis crosses it. The untrimmed surface is what
    still passes through that point. (Same reason the axis is not
    intersected with the SOLID — see the module docstring.)

    Returns every root at which the axis genuinely CROSSES the surface,
    unordered. A line meets a plane at most once, but it meets a cylinder
    or a cone twice and a general surface any number of times; deciding
    which root belongs to which end of the bore needs the edge that led
    here, so the caller does it.

    Roots the axis merely TOUCHES are not returned — see
    :func:`_crossing_normal`, which is what keeps a countersink's cone and
    a drill point's cone out by the same rule. Degenerate contact along a
    whole line — a line lying IN the surface — surfaces as a
    ``GeomAPI_IntCS`` segment rather than a point and is dropped for the
    same reason: a bore running along its cap breaks out of nothing.

    The NORMAL travels with the root rather than being re-derived from
    the point later, and that is not only to save a projection. At a
    B-spline pole — a domed cap on an imported part — projecting the
    point back onto the surface and asking OCCT for a normal returns
    noise (see :data:`DEGENERATE_COPLANARITY_TOL`). The vector computed
    here is the one the geometry actually has, and the openness verdict
    is entitled to it.
    """
    planar = _plane_axis_intersection(face, origin, direction)
    if planar is not None:
        return [planar]
    if _adaptor(face).GetType() == GeomAbs_SurfaceType.GeomAbs_Plane:
        # Planar but grazing — the analytic path already refused it, and
        # handing the same grazing case to the general intersector would
        # only launder a refusal into a numerically meaningless answer.
        return []

    surface = BRep_Tool.Surface_s(face)
    if surface is None:
        return []
    line = Geom_Line(
        gp_Ax1(
            gp_Pnt(origin[0], origin[1], origin[2]),
            gp_Dir(direction[0], direction[1], direction[2]),
        )
    )
    intersector = GeomAPI_IntCS(line, surface)
    if not intersector.IsDone():
        return []
    roots: List[Tuple[float, Tuple[float, float, float]]] = []
    for i in range(1, intersector.NbPoints() + 1):
        u, v, _w = intersector.Parameters(i)
        normal = _crossing_normal(surface, u, v, direction)
        if normal is None:
            continue
        p = intersector.Point(i)
        # The axial parameter is taken from the POINT rather than from
        # the line parameter ``_w`` the intersector also returns. They are
        # the same number — ``Geom_Line`` is unit-speed from ``origin``
        # along ``direction`` — and this is the arithmetic that was here
        # before, kept so that no answer moves by a last bit.
        roots.append(
            (
                _dot(
                    (
                        float(p.X()) - origin[0],
                        float(p.Y()) - origin[1],
                        float(p.Z()) - origin[2],
                    ),
                    direction,
                ),
                normal,
            )
        )
    return roots


def _outward_normal(face, point) -> Optional[Tuple[float, float, float]]:
    """The SOLID's outward unit normal on ``face`` at ``point``.

    The geometric normal of a surface is a property of how it was
    parametrised; which side of it the material sits on is carried by the
    face's orientation flag, exactly as in :func:`_is_bore_face`. So the
    surface normal is taken and then flipped for a REVERSED face, and the
    result points out of the part.

    Planar faces answer analytically — no projection, no local-property
    solve — which is both the common case and the cheap one.
    """
    try:
        adaptor = _adaptor(face)
        if adaptor.GetType() == GeomAbs_SurfaceType.GeomAbs_Plane:
            n = adaptor.Plane().Axis().Direction()
            normal = _unit((float(n.X()), float(n.Y()), float(n.Z())))
        else:
            surface = BRep_Tool.Surface_s(face)
            if surface is None:
                return None
            projector = GeomAPI_ProjectPointOnSurf(
                gp_Pnt(point[0], point[1], point[2]), surface
            )
            if not projector.IsDone() or projector.NbPoints() < 1:
                return None
            u, v = projector.LowerDistanceParameters()
            props = GeomLProp_SLProps(surface, u, v, 1, 1e-7)
            if not props.IsNormalDefined():
                return None
            n = props.Normal()
            normal = _unit((float(n.X()), float(n.Y()), float(n.Z())))
    except Exception:  # noqa: BLE001 — an unreadable cap has no verdict
        return None
    return _orient(face, normal)


def _orient(face, normal: Sequence[float]) -> Tuple[float, float, float]:
    """Turn a GEOMETRIC surface normal into the SOLID's outward one.

    Which side of a surface the material sits on is carried by the face's
    orientation flag, exactly as in :func:`_is_bore_face`, so a REVERSED
    face flips it. Split out of :func:`_outward_normal` because a cap
    reached through :func:`_cap_axis_intersections` already HAS its
    geometric normal and only needs this half.
    """
    if face.Orientation() == TopAbs_REVERSED:
        return (-normal[0], -normal[1], -normal[2])
    return (normal[0], normal[1], normal[2])


def _cap_says_open(face, point, outward: Sequence[float], normal=None) -> bool:
    """Does the material stop at this cap, in the direction ``outward``?

    The whole of the through/blind decision, in one dot product. See the
    module docstring's "Open or capped" for why this replaced 36
    point-in-solid queries per bore, and why an undecidable answer reads
    CAPPED rather than open.

    ``normal`` is the cap's GEOMETRIC surface normal where the axis
    crossed it, when the caller already knows it — which the cap walk
    does, because :func:`_cap_axis_intersections` computed it to decide
    the root was a crossing at all. Passing it through is exact where
    re-deriving it is not: at a B-spline pole the projection route
    returns noise. Callers holding only a POINT on the face — the
    neighbours that merely touch an end — leave it ``None`` and take the
    projection route.
    """
    if normal is None:
        oriented = _outward_normal(face, point)
    else:
        oriented = _orient(face, normal)
    if oriented is None:
        return False
    return _dot(oriented, outward) > CAP_OUTWARD_MIN_COS


class _EndEvidence:
    """What the cap walk found at ONE end of a bore.

    ``caps`` are (axial parameter, face, 3-D point, surface normal)
    quadruples from neighbours whose surface the axis actually CROSSES in
    range of this end. ``touching`` is every neighbour at this end paired
    with a point on the shared edge, including the ones that produced no
    usable crossing — a drill point's cone caps the bore without its axis
    ever crossing it, and it still gets a vote on whether the end is
    open.
    """

    __slots__ = ("caps", "touching")

    def __init__(self):
        self.caps: List[
            Tuple[float, object, Tuple[float, float, float], Tuple[float, float, float]]
        ] = []
        self.touching: List[Tuple[object, Tuple[float, float, float]]] = []

    def resolve(
        self, fallback_t: float, direction: Sequence[float], sign: float
    ) -> Tuple[float, bool]:
        """This end's true axial parameter, and whether it opens to air.

        ``sign`` is -1 at the low end and +1 at the high end, so the way
        OUT of the bore at this end is ``sign * direction`` and
        "outermost" is simply the largest ``sign * t``.

        The outermost cap wins the position. That is what carries a bore
        interrupted by a slot or a counterbore floor out to the part's
        real face instead of stopping at the interruption — and, because
        the same cap then answers the openness question, it is also what
        stops the slot's own wall from voting "blind" on a through-hole.

        With no cap at all the parametric bound stands and every
        neighbour that merely TOUCHES this end votes; all of them must
        say "out" for the end to be open. That is the drill-point case,
        and it is the conservative reading of a genuinely ambiguous one.
        """
        outward = (sign * direction[0], sign * direction[1], sign * direction[2])
        if self.caps:
            furthest = max(sign * cap[0] for cap in self.caps)
            # TIED caps all get a vote, and every one of them must say
            # "out". Two faces meeting exactly at the bore's exit — a
            # chamfer landing on its own edge, a bore breaking out on the
            # seam between two skin patches — are equally the cap there,
            # and picking whichever the face walk happened to reach first
            # would make the answer depend on OCCT's explorer order. That
            # is the S3 defect, in the one place the round-2 rewrite could
            # have reintroduced it: the position was always tie-proof (the
            # two ties agree on t by definition), but the openness verdict
            # reads the FACE, which they need not agree on.
            winners = [
                (face, point, normal)
                for t_cap, face, point, normal in self.caps
                if sign * t_cap >= furthest - 1e-9
            ]
            return sign * furthest, all(
                _cap_says_open(face, point, outward, normal)
                for face, point, normal in winners
            )
        if not self.touching:
            return fallback_t, False
        return fallback_t, all(
            _cap_says_open(face, point, outward) for face, point in self.touching
        )


class _EdgeFaces:
    """Edge → the faces that bound it, built once per part.

    This is what ``TopExp.MapShapesAndAncestors_s`` gives you, and the
    round-1 miner used that map directly. It has to be rebuilt in Python
    for a reason that is entirely about the BINDING and not the geometry:
    the OCCT lookup itself is free (~1 µs), but the
    ``TopTools_ListOfShape`` it hands back costs ~4.3 ms to iterate from
    Python — pybind marshals a fresh wrapper per element per traversal.
    At two edge-lookups per bore that alone was 12.75 s of the 14.76 s a
    600-hole part spent mining, dwarfing everything the cap walk actually
    computes. Walking the faces once and keeping plain Python lists costs
    7 ms for the whole part and makes the lookup free too.

    Keyed on ``TopTools_IndexedMapOfShape``'s index rather than on the
    shape, because that map hashes with ``IsSame`` semantics: an edge is
    ONE key however many faces reach it and whichever orientation each of
    them sees it in. Python's own hashing of a ``TopoDS_Shape`` would
    split a shared edge in two by orientation and every bore would lose
    half its caps.
    """

    __slots__ = ("_index", "_faces")

    def __init__(self, faces: Sequence):
        self._index = TopTools_IndexedMapOfShape()
        self._faces: dict = {}
        for face in faces:
            explorer = TopExp_Explorer(face, TopAbs_EDGE)
            while explorer.More():
                edge = TopoDS.Edge_s(explorer.Current())
                explorer.Next()
                self._faces.setdefault(self._index.Add(edge), []).append(face)

    def of(self, edge) -> List:
        """The faces bounding ``edge``; empty if the edge is unknown."""
        return self._faces.get(self._index.FindIndex(edge), ())


def _walk_caps(group, ancestors, p_lo, p_hi) -> Tuple[_EndEvidence, _EndEvidence]:
    """Gather what caps each end of the bore. ONE walk, TWO answers.

    Walk the bore's own boundary edges, step across each to the
    neighbouring face, and intersect the bore's axis with that
    neighbour's unbounded surface. What comes back is where each end
    really is (B4/N1) and, from the same faces, whether each end opens to
    air (B2/N2) — questions the miner used to answer in two separate
    passes, the second of which cost 36 point-in-solid queries per bore.

    Evidence is split at the bore's MIDPOINT rather than pooled, so a
    feature CROSSING the bore — a slot, a counterbore floor — cannot be
    mistaken for one of its ends.

    Two filters on every candidate root:

    - It must not lie past the bore's FAR bound. A root beyond the other
      end of the bore belongs to that end, or to neither.

      This bound used to be two-sided — the root had to lie inside the
      parametric span at both ends — on the reasoning that the span is a
      strict SUPERSET of the truth because an angled trim's ellipse runs
      past the real end. That is true of a planar cap and of a concave
      one, and it is FALSE of a doubly-curved CONVEX one (ADR-0112
      adversarial round 3, blocker 2). A dome's trim curve never reaches
      its crown: a Ø8 bore through a Ø40 ball is trimmed at z = ±19.5959
      and leaves the material at ±20, so the truth sat 0.4 mm OUTSIDE the
      "superset" and was thrown away at both ends. The bore came back
      39.1918 deep against a true 40, entering 0.4 mm inside solid metal.

      A singly-curved cap hid it. A bore through a round bar is trimmed
      right up to the crown — the bar does not curve along its own axis —
      so its root lands ON the old bound and squeaked through. It takes
      curvature in BOTH directions to open the gap.

      Relaxing the bound outward can only ADD roots, never move one that
      was already accepted, which is why no planar or singly-curved
      answer shifts by a bit. What keeps the relaxation honest is that a
      root outside the span is no longer discarded for being outside it,
      but for not being a crossing at all — see
      :func:`_axis_crosses_surface`.
    - Of the roots that survive, only the one NEAREST THE EDGE that led
      to this face is kept. A line crosses a shaft's OD twice, and the
      far crossing is the other end of the bore, not this one.
    """
    origin, direction = group.origin, group.direction
    mid = 0.5 * (p_lo + p_hi)
    pad = 1e-6
    low, high = _EndEvidence(), _EndEvidence()

    for cyl_face in group.faces:
        explorer = TopExp_Explorer(cyl_face, TopAbs_EDGE)
        while explorer.More():
            edge = TopoDS.Edge_s(explorer.Current())
            explorer.Next()
            try:
                neighbours = ancestors.of(edge)
                if not neighbours:
                    continue
                t_edge = _edge_axial_mean(edge, origin, direction)
                if t_edge is None:
                    continue
                edge_point = _edge_mid_point(edge)
                if edge_point is None:
                    continue
                at_low = t_edge < mid
                end = low if at_low else high
                # The far bound for THIS end: a low-end cap may lie as far
                # below the bore as its own curvature carries it, but not
                # above the high end.
                far = p_hi + pad if at_low else p_lo - pad
                for face in neighbours:
                    if any(face.IsSame(own) for own in group.faces):
                        continue
                    end.touching.append((face, edge_point))
                    roots = [
                        root
                        for root in _cap_axis_intersections(face, origin, direction)
                        if (root[0] <= far if at_low else root[0] >= far)
                    ]
                    if not roots:
                        continue
                    t_cap, normal = min(roots, key=lambda root: abs(root[0] - t_edge))
                    end.caps.append(
                        (
                            t_cap,
                            face,
                            _point_on_axis(origin, direction, t_cap),
                            normal,
                        )
                    )
            except Exception:  # noqa: BLE001 — one odd edge must not kill the bore
                continue

    return low, high


def _resolve_span(group, ancestors, p_lo, p_hi) -> Tuple[float, float, bool, bool]:
    """The bore's real axial span AND the openness of both its ends.

    One cap walk, both answers — see :func:`_walk_caps` and the module
    docstring's "Why UVBounds is not the answer" and "Open or capped".
    """
    low, high = _walk_caps(group, ancestors, p_lo, p_hi)
    direction = group.direction
    t_lo, lo_open = low.resolve(p_lo, direction, -1.0)
    t_hi, hi_open = high.resolve(p_hi, direction, +1.0)
    return t_lo, t_hi, lo_open, hi_open


def _classify_ends(lo_open: bool, hi_open: bool) -> HoleEndCondition:
    """Decide through / blind / unknown from the two end verdicts.

    - both ends open  -> THROUGH
    - exactly one     -> BLIND, and the open one is the entry
    - neither         -> UNKNOWN (an internal cavity, or a bore running
      into another feature)

    Every way of failing to decide an end — no cap face, an undefined
    normal, a grazing dot product, an OCCT throw inside the walk — lands
    on "not open" upstream in :meth:`_EndEvidence.resolve`, so a bore the
    extractor genuinely cannot read comes out UNKNOWN rather than
    guessed. UNKNOWN is a value Part C can refuse to price; a wrong
    THROUGH is not.
    """
    if lo_open and hi_open:
        return HoleEndCondition.THROUGH
    if lo_open != hi_open:
        # Terminates in material at exactly one end. Whether that end is
        # FLAT is a separate structural question — see `_has_flat_bottom`.
        return HoleEndCondition.BLIND
    return HoleEndCondition.UNKNOWN


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
            adaptor = _adaptor(face)
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
    # faces that cap each of its ends, which is where BOTH its true span
    # (B4/N1) and its end conditions (B2/N2) come from; see `_walk_caps`.
    # Built from `faces` rather than from `shape` so that the S3
    # reversed-walk fixture exercises this table too.
    ancestors = _EdgeFaces(faces)

    cylinders: List[_CylFace] = []
    for face in faces:
        try:
            cyl = _face_to_cyl(face)
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
            t_lo, t_hi, lo_open, hi_open = _resolve_span(
                group, ancestors, group.lo, group.hi
            )
            depth = t_hi - t_lo
            if depth <= 0.0:
                continue

            end_condition = _classify_ends(lo_open, hi_open)

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
