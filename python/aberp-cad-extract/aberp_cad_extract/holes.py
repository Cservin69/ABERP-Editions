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
- A curved surface can cross the axis INSIDE THE BORE'S OWN HOLLOW, past
  the mouth where the bore's wall stops, and there it bounds nothing at
  all — the bore itself cut the material away. An undercut spherical
  seat, of which a ball-nose bottom is the zero-undercut member, puts a
  pole there every time. :func:`_root_for_end` throws those crossings
  out; it is the one filter of the three that is about the BORE rather
  than about the cap.

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

CORRECTED AGAIN (ADR-0112 adversarial round 4, the foreign-root hijack).
Round 3 relaxed that outward bound and left the CROSS-FACE contest in
:meth:`_EndEvidence.resolve` as "the outermost cap wins". Those two were
co-designed and only one of them was changed. Round 2's inward clip had
been holding every outward root down at the bore's own end, so
outermost-wins could not reach past the part; without it, a face that
merely NEIGHBOURS the bore's mouth wins the end whenever its UNBOUNDED
carrier surface happens to cross the axis further out than the true cap
does. Three ordinary parts, all measured against the real kernel:

- A Ø8 through-bore 2 mm inboard of a 6 mm 45° CHAMFERED part edge, close
  enough that its mouth bites into the chamfer. The chamfer's plane is
  ``x + z = 54``; the bore's axis at x=32 meets it at z=22, two
  millimetres above a part that stops at z=20. Depth 22.0 against a true
  20.0, entry off the part.
- The same chamfer on a BLIND bore, where the error becomes a
  coordinate: depth 14.0 against 12.0, entry at z=22 — 2 mm in mid-air.
- A Ø14 through-bore beside a concave R6 corner FILLET whose axis lies
  outside the fillet's real extent. The fillet's carrier cylinder still
  crosses the axis, at z=20.8038 on a 20 mm plate.

``nearest-to-edge`` does not touch any of this: it disambiguates the
several roots of ONE face, and the defect is a contest BETWEEN faces.
Restricting the round-3 relaxation to non-planar caps does not close it
either — the concave fillet is not planar, and hijacks anyway.

What the neighbour is missing is not a bound but OWNERSHIP. A cap ends a
bore because the bore's mouth is cut in THAT face, and a face that owns
the mouth surrounds the axis with it. The chamfer owns 120° of the
mouth's 360° and the fillet 129°; the true cap owns the rest. So the
contest is now run over the faces that own the mouth, and only over
them: the outermost OWNED cap wins the end, and outermost-wins survives
untouched among the owned — which is what still carries a bore through a
slot or a counterbore floor out to the part's real face. Every genuine
cap on every committed fixture, curved ones included, owns its whole
mouth as a closed loop and none of their answers moves by a bit.
:func:`_mouth_owns_axis` is the test, and it is exact — vertex parity and
one sidedness comparison, no sampling and no tolerance to tune.

CORRECTED AGAIN (ADR-0112 adversarial round 5, the corner). Ownership
asked of ONE FACE is sufficient only while one face holds more than half
the mouth, and round 4's three parts all had a single neighbour, where
the true cap keeps 240 deg and clears half a turn comfortably. Chamfer
the ADJACENT top edge of the same plate — a detail on almost every real
part — and a bore beside the corner divides its mouth three ways: 150 /
105 / 105 deg with equal 6 mm legs, 168.59 / 115.18 / 76.23 with legs of
6 and 5. NO face clears half a turn, every one of them abstains, and the
end falls through to the outermost carrier surface again. Measured
against the real kernel on a 40 x 40 x 20 plate whose Zmax is 20.0:
depth 22.0 with equal legs, 23.0 with uneven ones, and a blind variant
entering at z=22 in mid-air. Sweeping twelve corner configurations,
through and blind, 22 of 24 answers were wrong.

Worse than abstention, the fall-back could be actively outvoted. Two
faces TIED at one axial level had their mouth sectors pooled, on the
reasoning that a bore breaking out across a seam reaches one cap along
several arcs. The corner's two equal chamfers both cross the axis at
z=22 and are pooled into a single 210 deg chain — which does clear half
a turn, so the foreign PAIR won ownership outright. Pooling is sound for
faces sharing one cap seam and unsound for distinct faces that merely
agree on a number.

So ownership moved from faces to RIMS. Pool the whole mouth at one end,
split it into connected components by vertex identity, and keep the ones
that CLOSE: those are the rims where the bore's wall stops. A rim has no
blind spot, because its arcs sum to the full turn however the faces
divide it — there is no threshold on any one face's share left to miss.
It also draws the pooling line where the topology does: faces sharing a
rim are one cap however far apart their carriers cross, and faces merely
tying at a level are not. The outermost RIM wins the end, ranked on its
own edges rather than on where an unbounded surface happens to cross, so
round 4's hijack cannot come back; within that rim the bore ends at the
INNERMOST crossing, because every face of a rim bounds the solid at the
mouth and the axis leaves through the first one it reaches. That reading
also cannot put an exit above the part, which is the whole failure class
of rounds 4 and 5. A bore CROSSED by a slot still has two rims at one
end and still measures out to the part's real face.
:func:`_mouth_rims` is the test; round 4's per-face rule stays as the
fall-back for an end whose mouth does not close, and still carries its
own four fixtures on its own.

One more thing round 5 found, unrelated to ownership and quieter. A
plane's ``Axis().Direction()`` is its Z direction, which equals the
parametric normal ``dU x dV`` only while the ``gp_Ax3`` is right-handed.
OCCT hands out LEFT-handed ones readily — chamfer two adjacent top edges
of a block and the second chamfer gets one — and a face's
FORWARD/REVERSED flag refers to the PARAMETRIC normal, so reading the
axis direction alone inverts that face's outward normal. It then votes
"material continues" at a genuine exit and, tied at the winning level,
vetoes the opening: a Ø8 through-hole read BLIND with a flat bottom,
which prices as a different cycle. :func:`_plane_normal` is the
correction, ``BRepClass3d_SolidClassifier`` is what settles which side
the material is on, and the curved branch never had the bug because
``GeomLProp_SLProps`` returns the parametric normal already. It is inert
on every committed fixture — OCCT's STEP writer re-parametrises planes,
so every one of them is direct — which is why the part that proves it is
built in memory.

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
    from OCP.BRepClass3d import BRepClass3d_SolidClassifier
    from OCP.BRepTools import BRepTools
    from OCP.Geom import Geom_Line
    from OCP.GeomAbs import GeomAbs_SurfaceType
    from OCP.GeomAPI import GeomAPI_IntCS, GeomAPI_ProjectPointOnSurf
    from OCP.GeomLProp import GeomLProp_SLProps
    from OCP.gp import gp_Ax1, gp_Dir, gp_Pnt, gp_Vec
    from OCP.TopAbs import (
        TopAbs_EDGE,
        TopAbs_FACE,
        TopAbs_IN,
        TopAbs_REVERSED,
        TopAbs_VERTEX,
    )
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
#:
#: RECONSIDERED and DELIBERATELY LEFT (ADR-0112 adversarial round 4). A
#: floor this low admits a near-tangent neighbour, and once round 3 made
#: outward roots unbounded a near-tangent neighbour is exactly the one
#: whose carrier crosses the axis furthest out — so this looked like an
#: amplifier of the foreign-root hijack and a candidate for tightening.
#: It is not, and tightening it would have been a second defect. What the
#: hijack needed was not a smaller admission angle but the OWNERSHIP gate
#: in :meth:`_EndEvidence.resolve`: a near-tangent face can only carry an
#: end now if it owns the bore's whole mouth, and a face that owns the
#: whole mouth IS the cap however tangent it is. Meanwhile a tighter
#: floor would start refusing real caps — a 0.5° draft face reads 0.0087
#: — and would trade a wrong THROUGH for a wrong depth. So the number
#: keeps the one job it was ever fit for, deciding GRAZING, and the
#: distance a root may travel is bounded by ownership instead.
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
#: And it degrades the right way at BOTH limits, which is worth writing
#: down because only the shallow one was (ADR-0112 adversarial round 4).
#:
#: SHALLOW — a cone so nearly flat that its tangents are coplanar to
#: within this floor is geometrically a flat cap, and reading it as one is
#: right. That is the first arm below, and the answer it gives is "cap".
#:
#: SHARP — a cone so nearly a needle that its generatrices are all but
#: PARALLEL to the axis spans no plane at all, and the second arm below
#: refuses it before coplanarity is ever asked. The answer there is "not a
#: cap", which is also right: a spike the axis runs down does not end a
#: bore. The two arms meet nowhere near anything real. With
#: :data:`DEGENERATE_PROBE_DIRECTIONS` sampling the collapsed isoline at
#: quarter turns, two tangents of a cone of half-angle α span
#: ``sin α·sqrt(1 + cos²α)``, so the sharp arm bites only below
#: ``sin α ≈ 7.1e-7`` — an INCLUDED angle under 1e-4 degrees, which is a
#: needle 1 mm across and 700 metres long. A 170°-included cone, already
#: absurd, measures 1.7e-01 at the shallow arm.
#:
#: Used twice, for the same kind of question: two tangents this close to
#: PARALLEL span no plane to test against either.
DEGENERATE_COPLANARITY_TOL: float = 1e-6

#: Twice the signed triangle area, in mm², that the bore MOUTH's chord and
#: a point on the mouth itself must span before the mouth is ruled to lie
#: on one side of that chord — the last step of :func:`_mouth_owns_axis`.
#:
#: A degeneracy floor, not a tuning knob. Vanishing area means the mouth's
#: two loose ends and the mouth between them are COLLINEAR, which is to
#: say the face owns exactly half a turn of the mouth and there is no side
#: to be had; the axis is ON the chord and neither answer is the true one.
#: Unproven reads NOT owned, which simply leaves the pre-round-4 contest
#: standing for that end rather than inventing a winner.
#:
#: The margin is the mouth's own size: a face owning any definite sector
#: of a bore of radius r spans an area of order r², so a Ø0.5 micro-drill
#: — the smallest thing a shop puts through a plate — clears this by ten
#: orders of magnitude, and a Ø8 bore by thirteen.
MOUTH_CHORD_MIN_AREA_MM2: float = 1e-12

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

#: How far two surfaces may disagree and still count as the SAME surface
#: — OCCT's own ``Precision::Confusion``, in mm, restated here so the
#: feature-off import guard stays a pure-Python affair.
#:
#: Used only to size :func:`_tangency_band`, never to compare positions.
SURFACE_CONFUSION_MM: float = 1e-7

#: Tolerance handed to `BRepClass3d_SolidClassifier.Perform` when
#: `_AxisMaterial` asks whether a point on the bore's axis is metal.
#: OCCT treats a point within this of the boundary as ON it, and ON is
#: neither of the two answers the probe wants — so it is kept far BELOW
#: the distance the probe steps off the surface (a `_tangency_band`,
#: 1.8e-3 mm on a O8 bore) and far ABOVE float noise. Nothing selects it
#: within that range: `test_d19_the_material_probe_is_not_a_tuned_epsilon`
#: moves the step, which is the quantity that could matter, over four
#: decades and no answer follows.
MATERIAL_PROBE_TOL_MM: float = 1e-9


#: How far past the bore's MOUTH a part edge the bore CUT THROUGH is
#: followed, in multiples of the bore's radius, when working out which
#: face of a rim is the skin the part presents over the AXIS.
#:
#: Such an edge enters the mouth's footprint at one rim vertex and leaves
#: it at another, so what has to be recovered is at most a footprint
#: crossing — one diameter for a straight edge, and longer only in
#: proportion to how far a curved one wanders on the way. Three radii is
#: a diameter and a half. It is a REACH and not a cost: the march stops
#: the instant the track leaves the footprint, which on the straight edge
#: that cuts an ordinary chamfer or fillet happens within a step or two.
BARRIER_REACH_RADII: float = 3.0

#: Steps per radius of :data:`BARRIER_REACH_RADII`.
#:
#: The march only ever decides WHICH SIDE of a cut edge the axis lies on,
#: so what matters is that its chords do not cut a corner across the axis.
#: Sixteen steps per radius put the chord of the worst case a curve can
#: be — one bending through a full radius — at r/2048 from the true
#: curve, three orders below the distances being separated, which are of
#: order r. A straight cut edge, which is what a chamfer or a tangent
#: fillet edge leaves, is followed EXACTLY at any step count.
BARRIER_MARCH_STEPS_PER_RADIUS: int = 16

#: How many EVENLY SPACED points of a face's share of the mouth are tried
#: as the target of the ray that asks whether that face's skin reaches
#: the AXIS.
#:
#: One would do for a face whose share of the footprint is convex, which
#: is every face of every ordinary part. The extras are what keep a face
#: whose share is PINCHED — a rim interrupted twice, at a part corner —
#: from reading as not reaching the axis at all because the one ray tried
#: happened to clip an obstruction. They cost one curve evaluation each.
#:
#: This ladder covers the MIDDLE of a mouth edge and nothing else: its
#: coarsest gap is at the two ENDS, where the first and last samples sit
#: ``1/(n+1)`` of the edge in from the vertices. That is exactly where
#: the surviving slivers are — see :func:`_barrier_chord_tolerance` — so the
#: count here is deliberately NOT what makes the test robust, and raising
#: it would not make it robust either.
MOUTH_RAY_SAMPLES: int = 5

#: Why the ends of a mouth edge need refining at all: every barrier is
#: the stub of a part edge the bore CUT, and :func:`_barrier_track`
#: starts each one AT the rim vertex it was cut at and marches away
#: across the footprint. So a barrier always emanates from an END of a
#: mouth edge, and the piece of mouth it leaves unobstructed is always a
#: run anchored at the OTHER end. Such a run can be arbitrarily short —
#: it is bounded by wherever the cut edge happens to sweep — while the
#: evenly spaced ladder cannot see anything shorter than ``1/(n+1)`` of
#: the edge, whatever ``n`` is.
#:
#: A Ø8 bore beside a conical boss overhanging the plate's edge leaves a
#: free run over the first 15% of the cone's mouth. Five samples start at
#: 16.7% and miss it by 1.7 points; six start at 14.3% and clear it by
#: 0.7 — so six is not a fix, it is the same coincidence with a luckier
#: number, and a boss one degree steeper would take it back (round 7,
#: blocker 2). Halving the gap at each end finds that run on the FIRST
#: halving, and would find one a hundred times narrower.
#:
#: Where the halving must STOP is :func:`_barrier_chord_tolerance`, and
#: that bound is load-bearing rather than a cost control. Arbitrarily
#: close to a rim vertex EVERY ray reads unobstructed, because the
#: barrier's own track begins at that vertex and a target a hair to one
#: side of it passes the segment test on a technicality. That is not a
#: route across the footprint, it is the polyline running out of
#: resolution — and a randomised boss sweep turns up such slivers at
#: ~1.6e-7 of a mouth edge on parts whose true answer is "this face does
#: NOT cover the axis". Refining past the barrier's own accuracy would
#: read all seven of them as covered.


def _barrier_chord_tolerance(radius: float) -> float:
    """How well :func:`_barrier_track`'s polyline locates its own edge.

    The march lays down chords of ``stride`` along a curve whose tightest
    useful bend is the bore's own radius (:data:`BARRIER_REACH_RADII`
    follows an edge only across the footprint), so each chord departs the
    true curve by at most ``stride² / 8r`` — the r/2048 that
    :data:`BARRIER_MARCH_STEPS_PER_RADIUS` is chosen to give.

    A ray target nearer a barrier than this is inside the barrier's own
    uncertainty: whether the segment test says it crosses is a fact about
    the polyline, not about the part. So this is where
    :func:`_mouth_ray_fractions` stops closing on a vertex.

    On a Ø8 bore that is 1.95e-3 mm. The conical-boss sliver that blocker
    2 is about sits 0.52 mm from its vertex — 270x outside it — and the
    degenerate slivers that the same sweep produces sit at 9.4e-7 mm,
    2000x inside. ``test_r7_the_mouth_ray_floor_is_not_a_tuned_epsilon``
    walks the decades between and pins that the answers do not move.
    """
    steps = max(1, int(BARRIER_MARCH_STEPS_PER_RADIUS * BARRIER_REACH_RADII))
    stride = BARRIER_REACH_RADII * max(radius, 0.0) / steps
    return stride * stride / (8.0 * radius) if radius > 0.0 else 0.0


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


def _mouth_inward_bound(edge, origin, direction, sign) -> Optional[float]:
    """How far INWARD along the axis this piece of the bore's MOUTH reaches.

    ``sign`` is -1 at the low end and +1 at the high end, so the way OUT
    of the bore is ``sign`` and "inward" — back up the bore, towards its
    other end — is ``-sign``. This returns the sampled point of greatest
    ``-sign * t``: the last of the mouth, going in.

    The mouth is where the bore's WALL stops, so past the whole of it the
    axis is running through the hollow the bore itself cut and nothing
    can bound the solid there. That is the bound's only job; see
    :func:`_root_for_end`.

    Sampled off the curve like :func:`_edge_axial_mean`, and unlike that
    one this is compared against a measurement rather than used to sort
    an edge into a half — so the sample is dense and the comparison
    carries a slack. Density is a formality on the mouths that matter: a
    mouth lying in one plane, which is every mouth that can put a
    LEGITIMATE crossing anywhere near the bound, gives the same answer at
    any sample count because every point of it sits at one ``t``.
    """
    try:
        curve = BRepAdaptor_Curve(edge)
        u0, u1 = float(curve.FirstParameter()), float(curve.LastParameter())
    except Exception:  # noqa: BLE001 — an unreadable edge bounds nothing
        return None
    samples = 65
    inner = None
    for k in range(samples):
        u = u0 + (u1 - u0) * k / (samples - 1)
        p = curve.Value(u)
        t = _dot(
            (float(p.X()) - origin[0], float(p.Y()) - origin[1], float(p.Z()) - origin[2]),
            direction,
        )
        if inner is None or -sign * t > -sign * inner:
            inner = t
    return inner


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


def _plane_normal(plane) -> Tuple[float, float, float]:
    """A ``gp_Pln``'s PARAMETRIC unit normal — the one the face's
    FORWARD/REVERSED flag is defined against.

    ``Axis().Direction()`` is the plane's Z direction, and for the
    right-handed coordinate systems OCCT builds almost everywhere it IS
    the parametric normal ``dU x dV``. It is not when the ``gp_Ax3`` is
    LEFT-handed: there ``dU x dV`` is its NEGATION, while the face's
    orientation flag still refers to the parametric one. Read the Z
    direction alone and an indirect plane's OUTWARD normal comes back
    inverted — the face reads as facing into the material when it faces
    out of it (ADR-0112 adversarial round 5).

    Not a hypothetical: chamfering two ADJACENT top edges of a plain
    block gives the second chamfer an indirect carrier, and that one
    face then answered every openness question backwards — a genuine
    through-hole at the corner read BLIND with a flat bottom. OCCT's own
    ``BRepClass3d_SolidClassifier`` puts the material on the side this
    function now reports, and the curved branch of
    :func:`_outward_normal` never had the bug: ``GeomLProp_SLProps``
    returns the parametric normal already.

    Inert on every part whose planes are direct, and the correction
    cancels in :func:`_plane_axis_intersection`'s ratio, so no cap
    POSITION moves by a bit either way.
    """
    n = plane.Axis().Direction()
    normal = _unit((float(n.X()), float(n.Y()), float(n.Z())))
    if plane.Position().Direct():
        return normal
    return (-normal[0], -normal[1], -normal[2])


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
    normal = _plane_normal(plane)
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
            normal = _plane_normal(adaptor.Plane())
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


def _mouth_loose_ends(edges) -> Optional[List[Tuple[float, float, float]]]:
    """The free ends of the bore's MOUTH in one cap face.

    ``edges`` is every edge the face shares with the bore's own
    cylinders — the opening the bore cut in it. Returns an EMPTY list
    when those edges close up into a loop (the face owns the whole
    mouth), the two end POINTS when they form a single open chain, and
    ``None`` when they are neither: several disjoint arcs, which this
    cannot read and will not guess at.

    Counted by vertex PARITY rather than by walking the chain in order.
    An interior joint is shared by exactly two edges and a loose end by
    one, so the odd-count vertices ARE the loose ends — whatever order
    OCCT hands the edges back in, and however many pieces a seam split
    the mouth into. A closed edge reports its single vertex twice, which
    is even, so a mouth that is one full circle answers "no loose ends"
    without being special-cased.

    Keyed on ``TopTools_IndexedMapOfShape``'s index for the same reason
    :class:`_EdgeFaces` is: that map hashes with ``IsSame`` semantics, so
    one vertex is ONE key however many orientations reach it. Python's
    own hashing of a ``TopoDS_Shape`` would split every joint in two and
    report a closed loop as all loose ends.
    """
    index = TopTools_IndexedMapOfShape()
    seen: dict = {}
    for edge in edges:
        explorer = TopExp_Explorer(edge, TopAbs_VERTEX)
        while explorer.More():
            vertex = TopoDS.Vertex_s(explorer.Current())
            explorer.Next()
            key = index.Add(vertex)
            count, point = seen.get(key, (0, None))
            if point is None:
                p = BRep_Tool.Pnt_s(vertex)
                point = (float(p.X()), float(p.Y()), float(p.Z()))
            seen[key] = (count + 1, point)
    if not seen:
        return None
    loose = [point for _key, (count, point) in sorted(seen.items()) if count % 2]
    if not loose:
        return []
    return loose if len(loose) == 2 else None


def _mouth_rims(mouths) -> List[Tuple[List[int], List]]:
    """The bore's mouth at ONE end, split into complete RIMS.

    ``mouths`` maps a face key to the edges that face shares with the
    bore's own cylinders at this end. Pooled across every face, those
    edges form one or more CLOSED loops — the rims where the bore's wall
    stops. Returns ``(face keys, edges)`` per rim that closes, in a
    deterministic order; ranking them is the caller's.

    Why rims and not faces (ADR-0112 adversarial round 5). Round 4 asked
    each face ALONE whether its share of the mouth surrounded the axis,
    and gave the end to the outermost face that said yes. That is exact
    only while ONE face owns more than half the mouth, and at a
    doubly-chamfered part CORNER none does. Chamfer two adjacent top
    edges of a plate — an ordinary shape, not a contrived one — and a
    bore beside the corner splits its mouth three ways: 150 deg of flat
    top and 105 deg of each chamfer on the equal-leg part, 168.59 /
    115.18 / 76.23 when the legs differ. Every face answers "not mine",
    and the fall-back handed the end to the OUTERMOST carrier surface,
    which is a chamfer plane 2-3 mm above a part that stops at z=20.

    A rim has no such blind spot, because it IS the whole mouth by
    construction: its arcs sum to the full turn however the faces divide
    it, so no threshold on any one face's share can be missed.

    It also separates the two things a level-based test conflates.
    Faces that merely TIE at one axial level are not thereby one cap —
    the corner's two equal chamfers both cross the axis at z=22 and are
    distinct faces, and pooling their sectors made a 210 deg "open
    chain" that outvoted the real top. Faces that genuinely SHARE a rim
    are one cap however far apart their carrier surfaces cross, which is
    the seam-split bore the pooling was written for. Sharing a rim is a
    topological fact about vertices; tying at a level is an arithmetic
    coincidence, and only the first is evidence.

    Connectivity and closure both come from vertex PARITY, for the
    reason :func:`_mouth_loose_ends` gives at length: an interior joint
    is shared by exactly two edges and a loose end by one, and
    ``TopTools_IndexedMapOfShape`` hashes with ``IsSame`` semantics so
    one vertex is ONE key however many edges and orientations reach it.
    A rim arriving as one closed edge reports its single vertex twice,
    which is even, and needs no special case.
    """
    index = TopTools_IndexedMapOfShape()
    entries: List[Tuple[int, object, List[int]]] = []
    for face_key, edges in sorted(mouths.items()):
        for edge in edges:
            vertices: List[int] = []
            explorer = TopExp_Explorer(edge, TopAbs_VERTEX)
            while explorer.More():
                vertex = TopoDS.Vertex_s(explorer.Current())
                explorer.Next()
                vertices.append(index.Add(vertex))
            entries.append((face_key, edge, vertices))
    if not entries:
        return []

    # Union-find over edges that share a vertex. Roots are kept at the
    # LOWEST member index so the component order is a function of the
    # sorted face keys and not of OCCT's walk order (S3).
    parent = list(range(len(entries)))

    def find(i: int) -> int:
        while parent[i] != i:
            parent[i] = parent[parent[i]]
            i = parent[i]
        return i

    seen_at: dict = {}
    for i, (_key, _edge, vertices) in enumerate(entries):
        for vertex in vertices:
            j = seen_at.setdefault(vertex, i)
            root_i, root_j = find(i), find(j)
            if root_i != root_j:
                parent[max(root_i, root_j)] = min(root_i, root_j)

    components: dict = {}
    for i in range(len(entries)):
        components.setdefault(find(i), []).append(i)

    rims: List[Tuple[List[int], List]] = []
    for root in sorted(components):
        members = components[root]
        parity: dict = {}
        for i in members:
            for vertex in entries[i][2]:
                parity[vertex] = parity.get(vertex, 0) + 1
        if any(count % 2 for count in parity.values()):
            # An open chain: this end's mouth is not all here. No rim.
            continue
        keys: List[int] = []
        rim_edges: List = []
        for i in members:
            key, edge, _vertices = entries[i]
            if key not in keys:
                keys.append(key)
            rim_edges.append(edge)
        rims.append((sorted(keys), rim_edges))
    return rims


def _mouth_owns_axis(edges, origin, direction) -> bool:
    """Does this face's share of the bore's MOUTH surround the axis?

    The round-4 ownership test, and the whole of the foreign-root fix —
    see the module docstring. A face caps a bore because the bore's mouth
    is cut in it; a face that merely stands NEXT TO the mouth owns a
    sector of it and no more, and its unbounded carrier surface has no
    business deciding where the bore ends.

    "Surrounds" is asked of the mouth rather than of the crossing point,
    and that is what makes the test exact instead of a classification
    problem. The obvious reading of ownership — project the axis crossing
    onto the face and ask ``BRepClass`` whether it lands ON the face —
    answers NO for every genuine cap the miner has, because the crossing
    lands in the middle of the hole the bore itself cut. Measured on the
    committed fixtures: the crown of a Ø40 ball, of a NURBS dome and of a
    torus wall all classify OUT, and the two domes are outside the face's
    UV bounds as well (their trim stops at v = ±1.3694 rad and the axis
    leaves at ±π/2). A UV-in-bounds or on-face gate would therefore have
    re-opened round 3's blocker 2 at both ends of all three. The mouth
    has no such trouble: it is a real boundary of the real face, and it
    either goes round the axis or it does not.

    Two cases, both exact:

    - The mouth CLOSES on itself — no loose ends — so it encircles the
      axis and the face owns the end. Every cap on every committed
      fixture is this case, including the ones the mouth arrives in
      pieces on: a seam-split bore hands its top face four quarter-arcs
      and a torus wall four edges, and parity closes both. This costs a
      vertex walk and no geometry at all.

    - The mouth is one open CHAIN, so the face shares it with another
      face and owns an arc between two loose ends. Every point of that
      arc lies on the bore's own cylinder, so seen down the axis it is an
      arc of a circle with the crossing at its CENTRE, and the centre of
      a circle lies inside an arc's own segment exactly when that arc
      exceeds half a turn. Which is one sidedness comparison against the
      chord: the axis is owned when it falls on the same side of the
      chord as the mouth does. No angles are summed and nothing is
      sampled — a chord and a single point on the arc settle it.

    The 45° chamfer of the round-4 repro owns 120° of its neighbour's
    mouth and the concave R6 fillet 129°, so both fail; the true cap owns
    the remaining 240° and 231° and passes. A face reaching neither case
    — a mouth in several disjoint pieces — is not owned, which leaves the
    pre-round-4 contest standing for that end rather than guessing.
    """
    try:
        loose = _mouth_loose_ends(edges)
    except Exception:  # noqa: BLE001 — an unreadable mouth proves no ownership
        return False
    if loose is None:
        return False
    if not loose:
        return True

    e1, e2 = _perp_basis(direction)

    def lateral(point: Sequence[float]) -> Tuple[float, float]:
        """``point`` seen down the bore's axis, from the axis."""
        offset = (
            point[0] - origin[0],
            point[1] - origin[1],
            point[2] - origin[2],
        )
        return _dot(offset, e1), _dot(offset, e2)

    try:
        curve = BRepAdaptor_Curve(edges[0])
        u0, u1 = float(curve.FirstParameter()), float(curve.LastParameter())
        p = curve.Value(0.5 * (u0 + u1))
        on_mouth = lateral((float(p.X()), float(p.Y()), float(p.Z())))
    except Exception:  # noqa: BLE001 — as above
        return False

    a, b = lateral(loose[0]), lateral(loose[1])

    def side(point: Sequence[float]) -> float:
        """Twice the signed area of (chord start, chord end, ``point``)."""
        return (b[0] - a[0]) * (point[1] - a[1]) - (b[1] - a[1]) * (point[0] - a[0])

    reference = side(on_mouth)
    if abs(reference) <= MOUTH_CHORD_MIN_AREA_MM2:
        return False
    return side((0.0, 0.0)) * reference > 0.0


def _mouth_reach(mouths, origin, direction, sign) -> Optional[float]:
    """How far OUT this end's mouth itself gets, over every edge of it.

    The mouth is on the part's boundary — that is what makes it the
    mouth — so nothing beyond the outermost of it is on the part at this
    end. It is the bound :meth:`_EndEvidence.resolve` puts on its
    fall-back, and it is measured on the mouth's own EDGES, real
    boundary of the real solid, for the same reason :meth:`_rim_winner`
    ranks rims on theirs.

    ``None`` when no edge of the mouth can be measured, which bounds
    nothing.
    """
    reach = [
        sign * mean
        for mean in (
            _edge_axial_mean(edge, origin, direction)
            for edges in mouths.values()
            for edge in edges
        )
        if mean is not None
    ]
    return max(reach) if reach else None


def _edge_vertices(edge) -> List:
    """Both ends of an edge, as vertices."""
    verts: List = []
    explorer = TopExp_Explorer(edge, TopAbs_VERTEX)
    while explorer.More():
        verts.append(TopoDS.Vertex_s(explorer.Current()))
        explorer.Next()
    return verts


def _across(point, origin, e1, e2) -> Tuple[float, float]:
    """A 3-D point seen ACROSS the bore: its offset from the axis in the
    plane the mouth lives in, with the axis itself at the origin.

    The basis comes from :func:`_perp_basis`, which derives it from the
    bore's direction alone, so every face of one mouth is measured
    against the same reference.
    """
    v = (point[0] - origin[0], point[1] - origin[1], point[2] - origin[2])
    return _dot(v, e1), _dot(v, e2)


def _crosses(a, b, c, d) -> bool:
    """Do the 2-D segments ``ab`` and ``cd`` PROPERLY cross?

    Properly: each segment must have the other's ends STRICTLY to either
    side. Merely touching is not a crossing, and both ways of touching
    matter here.

    A ray aimed at the very vertex where a cut edge meets the mouth
    grazes it, and at that vertex the two faces meet, so neither is shut
    out by it.

    And a cut edge running through the AXIS ITSELF divides the footprint
    without dividing the axis from anything: the axis is ON the division,
    where the two skins meet, and there they agree — a bore centred
    exactly on a part's chamfer boundary gets the same crossing from the
    flat top and from the chamfer, because that is what meeting means.
    The case is not exotic. A sphere's parametric SEAM is such an edge,
    it is not a crease in the skin at all, and OCCT puts it on the
    meridian a bore beside a dome is very often centred on.
    """

    def turn(o, p, q) -> float:
        return (p[0] - o[0]) * (q[1] - o[1]) - (p[1] - o[1]) * (q[0] - o[0])

    d1, d2 = turn(a, b, c), turn(a, b, d)
    d3, d4 = turn(c, d, a), turn(c, d, b)
    return ((d1 > 0.0 > d2) or (d2 > 0.0 > d1)) and (
        (d3 > 0.0 > d4) or (d4 > 0.0 > d3)
    )


def _barrier_track(edge, vertex, origin, e1, e2, radius) -> List[Tuple[float, float]]:
    """The 2-D track, ACROSS the bore's mouth, of a part edge the bore CUT.

    ``edge`` is a stub the bore left outside the mouth, and ``vertex`` is
    the end of it the bore cut — a rim vertex, on the mouth itself. The
    piece of that edge which used to run on across the mouth's footprint
    is gone from the topology, and this walks the edge's own UNTRIMMED
    curve out past the vertex to get it back.

    UNTRIMMED for exactly the reason :func:`_cap_axis_intersections`
    gives for surfaces: the bore removed the very piece being asked
    about, and what still runs there is the curve underneath it.

    The walk steps by ARC LENGTH — the curve's own first derivative
    converts each step, so a spline's uneven parametrisation does not
    stretch or bunch it — and stops the moment the track leaves the
    footprint, which is where the cut edge re-emerges as its other stub
    and has finished dividing anything.
    """
    try:
        curve = BRepAdaptor_Curve(edge)
        u_lo, u_hi = float(curve.FirstParameter()), float(curve.LastParameter())
        u = float(BRep_Tool.Parameter_s(vertex, edge))
        start = curve.Value(u)
    except Exception:  # noqa: BLE001 — an unreadable edge divides nothing
        return []
    # Out past the cut end, which is to say away from the stub that
    # survived: the vertex is one end of the trimmed range and the
    # footprint is on the far side of it.
    march = -1.0 if abs(u - u_lo) <= abs(u - u_hi) else 1.0
    steps = max(1, int(BARRIER_MARCH_STEPS_PER_RADIUS * BARRIER_REACH_RADII))
    stride = BARRIER_REACH_RADII * radius / steps
    track = [
        _across(
            (float(start.X()), float(start.Y()), float(start.Z())), origin, e1, e2
        )
    ]
    point, tangent = gp_Pnt(), gp_Vec()
    for _ in range(steps):
        try:
            curve.D1(u, point, tangent)
            speed = float(tangent.Magnitude())
            if speed <= 0.0:
                break
            u += march * stride / speed
            here = curve.Value(u)
        except Exception:  # noqa: BLE001 — a curve that stops answering stops here
            break
        step = _across(
            (float(here.X()), float(here.Y()), float(here.Z())), origin, e1, e2
        )
        track.append(step)
        if math.hypot(step[0], step[1]) > radius:
            break
    return track


def _rim_barriers(faces, mouth, origin, e1, e2, radius) -> List[List[Tuple[float, float]]]:
    """Every part edge the bore CUT at this rim, as a track across the
    mouth's footprint.

    A rim face's own edges that TOUCH the mouth without being part of it
    are exactly the edges the bore interrupted. Each one ran across the
    footprint until the bore took the middle out of it, and each one
    divides the footprint in two: the flat top and the chamfer beside it
    meet along such an edge, and so do the flat top and a tangent fillet.

    Empty when the bore's mouth landed in the middle of its faces and cut
    no edge at all, which is the ordinary hole in the ordinary plate —
    and an empty result is what leaves that hole's answer exactly where
    round 5 left it.
    """
    ends = [end for edge in mouth for end in _edge_vertices(edge)]
    tracks: List[List[Tuple[float, float]]] = []
    seen: List = []
    for face in faces:
        explorer = TopExp_Explorer(face, TopAbs_EDGE)
        while explorer.More():
            edge = TopoDS.Edge_s(explorer.Current())
            explorer.Next()
            if any(edge.IsSame(other) for other in mouth):
                continue
            if any(edge.IsSame(other) for other in seen):
                continue
            cut = next(
                (
                    vertex
                    for vertex in _edge_vertices(edge)
                    if any(vertex.IsSame(end) for end in ends)
                ),
                None,
            )
            if cut is None:
                continue
            seen.append(edge)
            track = _barrier_track(edge, cut, origin, e1, e2, radius)
            if len(track) > 1:
                tracks.append(track)
    return tracks


def _mouth_ray_fractions(radius: float):
    """Where along a mouth edge to aim the rays of :func:`_skin_reaches_axis`.

    Two ladders, in this order:

    - the EVENLY SPACED one, ``k/(n+1)`` for ``k`` in ``1..n``
      (:data:`MOUTH_RAY_SAMPLES`), which is what round 6 tried and all
      that an unpinched mouth ever needs;
    - then, from each end inward, the gap the first ladder left there
      HALVED again and again, down to :func:`_barrier_chord_tolerance`.

    Coarse first, so the ordinary part answers on its first ray and pays
    nothing for the rest: the refinement is only ever reached by a face
    whose whole evenly spaced ladder was blocked, which is the pinched
    rim the refinement exists for.

    Depth comes from the geometry rather than from a chosen count, at
    both ends of it. A mouth edge lies inside the bore's own footprint,
    so it is no longer than that footprint's perimeter ``2πr``, which
    turns the fractions into millimetres; and the halving stops at
    :func:`_barrier_chord_tolerance`, the accuracy of the very polylines
    the rays are tested against. Eleven halvings on a bore of any size,
    because both bounds scale with the radius.

    Adding samples can only ever turn a "no route" into a "route", never
    the other way about, so the old ladder being a PREFIX of this one is
    what keeps every answer round 6 already got bit-identical.
    """
    for k in range(1, MOUTH_RAY_SAMPLES + 1):
        # round 6's expression, to the BIT — see the prefix argument above
        yield k / (MOUTH_RAY_SAMPLES + 1)
    step = 1.0 / (MOUTH_RAY_SAMPLES + 1)
    floor = _barrier_chord_tolerance(radius)
    reach = 2.0 * math.pi * max(radius, 0.0) * step
    fraction = step
    while reach * 0.5 > floor:
        reach *= 0.5
        fraction *= 0.5
        yield fraction
        yield 1.0 - fraction


def _skin_reaches_axis(mouth_edges, barriers, origin, e1, e2, radius) -> bool:
    """Is THIS face's skin what the part presents over the bore's axis?

    The face holds a share of the mouth. The question is whether that
    share and the axis lie on the same side of every edge the bore cut
    through the mouth's footprint, and it is answered by aiming a ray
    from the axis at the face's own mouth: an unobstructed ray is a path
    across the footprint from the axis to skin this face demonstrably
    owns, with no cut edge in between, and therefore skin this face owns
    the whole way.

    Several points of the mouth are tried and ANY of them succeeding is
    enough — see :func:`_mouth_ray_fractions`, which covers the middle of
    each mouth edge evenly and then closes on its two ENDS, because a
    pinched face's surviving sliver of mouth is always anchored at one of
    them. Failing all of them means
    every route from the axis to this face's mouth crosses an edge where
    the part's skin changes face, which is precisely what a bore
    straddling a rounded or chamfered edge does to the face on the far
    side of it.
    """
    for edge in mouth_edges:
        try:
            curve = BRepAdaptor_Curve(edge)
            u0, u1 = float(curve.FirstParameter()), float(curve.LastParameter())
        except Exception:  # noqa: BLE001 — an unreadable mouth edge is no route
            continue
        for fraction in _mouth_ray_fractions(radius):
            try:
                p = curve.Value(u0 + (u1 - u0) * fraction)
            except Exception:  # noqa: BLE001
                continue
            tip = _across(
                (float(p.X()), float(p.Y()), float(p.Z())), origin, e1, e2
            )
            if math.hypot(tip[0], tip[1]) <= 0.0:
                continue
            if not any(
                _crosses((0.0, 0.0), tip, track[i], track[i + 1])
                for track in barriers
                for i in range(len(track) - 1)
            ):
                return True
    return False


class _EndEvidence:
    """What the cap walk found at ONE end of a bore.

    ``caps`` are (axial parameter, face, 3-D point, surface normal, face
    key, mouth edge) sextuples from neighbours whose surface the axis
    actually CROSSES in range of this end. The EDGE is the one that led
    to the face and picked this root out of its several; it travels with
    the cap because :meth:`_skin_over_axis` has to ask its question of
    the edge rather than of the face — one face can reach an end along
    two different pieces of the mouth and offer a different root at
    each. ``touching`` is every neighbour at this
    end paired with a point on the shared edge, including the ones that
    produced no usable crossing — a drill point's cone caps the bore
    without its axis ever crossing it, and it still gets a vote on
    whether the end is open.

    ``mouths`` maps a face key to every edge that face shares with the
    bore's own cylinders at this end — the bore's MOUTH in it, pooled
    across however many edges and however many of the bore's own faces
    lead there. That pooling is the point: a seam-split bore reaches one
    cap face along four quarter-arcs, and the mouth is only a closed loop
    when all four are held together. :meth:`resolve` reads it to decide
    which candidate OWNS the end (ADR-0112 adversarial round 4).

    The key is ``TopTools_IndexedMapOfShape``'s index rather than the
    face itself, for the reason :class:`_EdgeFaces` gives at length: that
    map hashes with ``IsSame`` semantics, and Python's own hashing of a
    ``TopoDS_Shape`` would file one face under two keys and split its
    mouth in half.
    """

    __slots__ = ("caps", "touching", "mouths", "_face_keys")

    def __init__(self):
        self.caps: List[
            Tuple[
                float,
                object,
                Tuple[float, float, float],
                Tuple[float, float, float],
                int,
            ]
        ] = []
        self.touching: List[Tuple[object, Tuple[float, float, float]]] = []
        self.mouths: dict = {}
        self._face_keys = TopTools_IndexedMapOfShape()

    def note_mouth(self, face, edge) -> int:
        """Record ``edge`` as part of the bore's mouth in ``face``.

        Returns the face's key, which the caller stores on the cap so
        :meth:`resolve` can find the mouth again. Recorded for EVERY
        neighbour, including the ones that yield no root — another edge
        of the same face may yield one, and the mouth it is judged on has
        to be the whole mouth by then.
        """
        key = self._face_keys.Add(face)
        edges = self.mouths.setdefault(key, [])
        if not any(edge.IsSame(other) for other in edges):
            edges.append(edge)
        return key

    def _at(self, level: float, sign: float) -> List:
        """Every cap sitting at ``level``, ties included.

        TIED caps all get a vote, and every one of them must say "out".
        Two faces meeting exactly at the bore's exit — a chamfer landing
        on its own edge, a bore breaking out on the seam between two skin
        patches — are equally the cap there, and picking whichever the
        face walk happened to reach first would make the answer depend on
        OCCT's explorer order. That is the S3 defect, in the one place the
        round-2 rewrite could have reintroduced it: the position was
        always tie-proof (the two ties agree on t by definition), but the
        openness verdict reads the FACE, which they need not agree on.
        """
        return [cap for cap in self.caps if abs(sign * cap[0] - level) <= 1e-9]

    def _owns(self, winners: Sequence, origin, direction) -> bool:
        """Do the tied caps at one level, TOGETHER, own the bore's mouth?

        Pooled across the tie rather than asked of each face alone,
        because a tie is precisely the case where one mouth is shared: a
        bore breaking out on the seam between two skin patches gives each
        patch half the loop, and only the pair of them closes it. Faces
        at DIFFERENT levels are not pooled — that is the hijack, and
        keeping them apart is what exposes it.
        """
        mouth: List = []
        for cap in winners:
            for edge in self.mouths.get(cap[4], ()):
                if not any(edge.IsSame(other) for other in mouth):
                    mouth.append(edge)
        return _mouth_owns_axis(mouth, origin, direction)

    def _skin_over_axis(self, keys, edges, origin, direction, radius) -> List:
        """The caps of a rim that stand where the part's skin crosses the
        AXIS.

        The mouth's footprint is divided by the edges the bore CUT on its
        way through — :func:`_rim_barriers` — and each piece of the mouth
        holds the part of the footprint on its own side of them. A cap
        stands over the axis when a ray from the axis reaches the piece
        of mouth that FOUND it without crossing one of those edges
        (:func:`_skin_reaches_axis`): that ray is a path from the axis to
        skin this face demonstrably owns, over skin that never changes
        face on the way.

        Asked of the cap's own mouth EDGE, not of its face, because the
        two are not the same question. A ball fused onto a block's corner
        presents ONE spherical face as the skin both above the block and
        outboard of it, and a bore beside it reaches that face twice: on
        the dome, where the sphere really is the skin over the axis, and
        on the underside of the ball's outboard bulge, where it is not.
        The face covers the axis; the second piece of mouth does not, and
        the root it picks is the sphere's other crossing, 11 mm inside
        the material.

        Empty when the question does not arise or cannot be answered:
        when the bore cut no edge of the part, when no route survives, or
        when OCCT declines somewhere in the walk. Empty means the caller
        keeps every cap, which is round 5's rule exactly — this narrows
        the field where the geometry says which skin owns the axis, and
        says nothing where it does not.
        """
        e1, e2 = _perp_basis(direction)
        try:
            faces = [TopoDS.Face_s(self._face_keys.FindKey(key)) for key in keys]
            barriers = _rim_barriers(faces, edges, origin, e1, e2, radius)
        except Exception:  # noqa: BLE001 — an unreadable rim narrows nothing
            return []
        if not barriers:
            return []
        return [
            cap
            for cap in self.caps
            if cap[4] in keys
            and any(cap[5].IsSame(edge) for edge in edges)
            and _skin_reaches_axis([cap[5]], barriers, origin, e1, e2, radius)
        ]

    def _rim_winner(
        self, origin, direction, sign, radius
    ) -> Optional[Tuple[float, List]]:
        """The level this end leaves through, and the caps that say so.

        The round-5 rule, in three steps and no thresholds:

        - Split this end's pooled mouth into complete RIMS
          (:func:`_mouth_rims`). One bore end usually has exactly one;
          a bore CROSSED by a slot has two, because the slot's own
          opening is a second full loop in the same evidence pool.
        - Rank rims by their OUTERMOST edge and take the winner. This is
          what carries a bore past an interruption out to the part's
          real face, and it is ranked on the rim's own edges — real
          boundary of the real solid — rather than on where a carrier
          surface happens to cross, so no unbounded plane can win a rim
          it has no edge in. That was round 4's hijack.
        - Within that rim, the bore ends at the INNERMOST crossing among
          the rim's faces whose SKIN REACHES THE AXIS. Every face of a
          rim bounds the solid at the mouth, so going outward the axis
          leaves the material at the FIRST of them it reaches; a face
          crossing further out has had its material cut away before the
          axis gets there. On the corner part that is min(20, 22, 23) =
          20, the flat top, which is where the plate actually ends.

          Reaching the axis is what round 5 left out. A rim face bounds
          the solid SOMEWHERE on the mouth; that does not make its
          surface the part's skin over the middle of the mouth, where
          the axis crosses. Where the bore STRADDLES an edge of the part
          the two are different faces, and the crossing taken from the
          one that does not cover the axis is not on the part at all:
          it comes from the UNTRIMMED carrier
          :func:`_cap_axis_intersections` deliberately asks, which
          carries on under the neighbouring face and under the material.
          A Ø10 bore straddling a convex R6 rounded top edge crosses
          that fillet's cylinder 1.5 mm BELOW the flat top the plate
          really ends at, and innermost-of-all-faces took it: 18.472136
          against a true 20, on a blind bore an entry point 1.5 mm
          inside solid metal, and on a domed shoulder 45.9% short at
          BOTH ends (ADR-0112 adversarial round 6, blocker 1).

          Which faces reach the axis is :meth:`_skin_over_axis`. It
          answers nothing at all when the bore cut no edge of the part —
          the ordinary hole in the ordinary plate — and there the rule
          is round 5's, unchanged and to the bit.

        The innermost reading is also the safe one. It can only report a
        cap at or inside a crossing that some face of the rim genuinely
        has, so it cannot put an exit in mid-air ABOVE the part — which
        is the entire failure class rounds 4 and 5 are about (23.0 and
        22.0 on a part whose Zmax is 20.0). Round 4's own concave
        hijacker agrees with it: the R6 corner fillet crosses at
        20.8038 and the true top at 20, and 20 is the answer. Round 6
        narrows WHICH crossings are in that reckoning without touching
        the reckoning itself, so it stays safe in the same direction.

        ``None`` when no rim closes, which leaves the round-4 contest
        standing rather than guessing — see :meth:`resolve`.
        """
        rims = _mouth_rims(self.mouths)
        if not rims:
            return None
        ranked = []
        for keys, edges in rims:
            means = [_edge_axial_mean(edge, origin, direction) for edge in edges]
            outer = [sign * mean for mean in means if mean is not None]
            levels = [sign * cap[0] for cap in self.caps if cap[4] in keys]
            if outer and levels:
                ranked.append((max(outer), min(levels), len(edges), keys, edges))
        if not ranked:
            return None
        # Ranked outermost-rim first. Every tiebreak after that is a
        # GEOMETRIC quantity — the rim's own crossing level, then how many
        # edges it arrived in — because the face keys are handed out in
        # OCCT's walk order, and S3 does not allow that order to reach the
        # answer. Keys are the last resort and only separate two rims that
        # agree on all three, which is a degenerate part rather than an
        # ordinary one.
        ranked.sort(key=lambda entry: (-entry[0], entry[1], -entry[2], entry[3]))
        _outer, _level, _count, keys, edges = ranked[0]
        # Ranked on the rim's own EDGES, so narrowing the faces afterwards
        # cannot move which rim won — only where, within it, the bore
        # ends.
        standing = self._skin_over_axis(keys, edges, origin, direction, radius)
        level = min(
            sign * cap[0]
            for cap in (standing or [cap for cap in self.caps if cap[4] in keys])
        )
        # The narrowing decides the LEVEL and stops there. Which faces then
        # get a vote on whether the end is OPEN is the round-2 tie rule,
        # unchanged: every cap of the rim that reaches the winning level is
        # equally the cap there, and all of them must say "out". A face
        # whose skin does not cover the axis can still meet the winner
        # exactly — at a part corner the axis can sit ON the edge where the
        # flat top and a chamfer agree — and disenfranchising it there
        # would throw away a vote the geometry genuinely casts.
        winners = [
            cap
            for cap in self.caps
            if cap[4] in keys and abs(sign * cap[0] - level) <= 1e-9
        ]
        return level, winners

    def resolve(
        self,
        fallback_t: float,
        origin,
        direction: Sequence[float],
        sign: float,
        radius: float,
    ) -> Tuple[float, bool]:
        """This end's true axial parameter, and whether it opens to air.

        ``sign`` is -1 at the low end and +1 at the high end, so the way
        OUT of the bore at this end is ``sign * direction`` and
        "outermost" is simply the largest ``sign * t``.

        The outermost cap that OWNS the bore's mouth wins the position.
        Outermost is what carries a bore interrupted by a slot or a
        counterbore floor out to the part's real face instead of stopping
        at the interruption — and, because the same cap then answers the
        openness question, it is also what stops the slot's own wall from
        voting "blind" on a through-hole. Both of those interruptions cut
        a full circle out of the face they cross, so they own their
        mouths and the rule reaches them unchanged.

        Ownership is what round 4 added, and it decides only which
        candidate the contest is between — see :func:`_mouth_owns_axis`
        and the module docstring. Without it the outermost carrier
        surface wins whether or not the bore ever reached it, and a
        chamfer or a fillet standing beside the mouth carries the end off
        the part. With it, a face has to have the mouth cut in it before
        its surface is allowed to say where the bore stops.

        Where NOTHING at this end owns the mouth the outermost cap wins
        as it did before. That is deliberate: ownership is a fact the
        topology can prove, not one it can disprove, and an end whose
        mouth reads as several disjoint arcs is no reason to throw away
        the only evidence there is.

        With no cap at all the parametric bound stands and every
        neighbour that merely TOUCHES this end votes; all of them must
        say "out" for the end to be open. That is the drill-point case,
        and it is the conservative reading of a genuinely ambiguous one.
        """
        outward = (sign * direction[0], sign * direction[1], sign * direction[2])
        if self.caps:
            decided = self._rim_winner(origin, direction, sign, radius)
            if decided is not None:
                winner_level, winners = decided
            else:
                levels = sorted({sign * cap[0] for cap in self.caps}, reverse=True)
                # Round 4's contest, BOUNDED by the mouth. Round 5 proved
                # this arm insufficient at a chamfered corner — outermost
                # owned put the exit at z=22 on a plate 20 thick — and no
                # committed part reaches it, which is the danger: a real
                # imported part whose mouth is a shade unsewn would drop
                # to a rule known to be wrong and say nothing. It cannot
                # any more. The mouth is boundary of the solid, so a cap
                # further OUT than every edge of it is off the part, and
                # the corner's z=22 goes out on that alone (ADR-0112
                # adversarial round 6, should-fix).
                #
                # The bound narrows and never empties: where it would
                # leave nothing it is not applied, so this arm still
                # carries every answer it carried before rather than
                # trading a wrong number for no number.
                reach = _mouth_reach(self.mouths, origin, direction, sign)
                if reach is not None:
                    levels = [
                        level for level in levels if level <= reach + 1e-9
                    ] or levels
                winner_level = next(
                    (
                        level
                        for level in levels
                        if self._owns(self._at(level, sign), origin, direction)
                    ),
                    levels[0],
                )
                winners = self._at(winner_level, sign)
            return sign * winner_level, all(
                _cap_says_open(face, point, outward, normal)
                for _t_cap, face, point, normal, _key, _edge in winners
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


def _tangency_band(radius: float) -> float:
    """How far a cap's axis crossing can move when its SURFACE is uncertain.

    Two surfaces OCCT cannot tell apart still cross the axis in different
    places, and near a tangency they cross it in wildly different places:
    take a spherical cap of radius ``R`` on a bore of radius ``r``, whose
    crossings sit at ``t_c ± R`` and whose mouth sits at
    ``t_c ± sqrt(R² - r²)``. The two crossings straddle that mouth, one
    either side, at a distance of ``sqrt(R² - r²)`` from it. Writing
    ``R = r + e`` for the amount by which the cap FAILS to be tangent
    gives ``sqrt(2·r·e + e²)``, and the surfaces are indistinguishable —
    the same surface, as far as the kernel is concerned — once ``e``
    drops to :data:`SURFACE_CONFUSION_MM`. So

        band(r) = 2·sqrt(2·r·SURFACE_CONFUSION_MM)

    is exactly "the two surfaces agree to within OCCT's own confusion
    over the whole mouth", carried into the units the caller compares in,
    and counted for BOTH sides of the mouth. Half of it is the one-sided
    figure: how far from the mouth a crossing that ought to be ON the
    mouth may be computed to lie. That is what :func:`_root_for_end`
    wants, and it takes the half.

    Note the SQUARE ROOT, which is the whole reason a slack is needed at
    all rather than an equality: near a tangency the axial displacement
    is the square root of the surface mismatch, so it is enormously more
    sensitive than the mismatch is. On a Ø8 bore the band is 1.8e-3 mm —
    twelve orders above the ~4e-15 mm of float noise that separates a
    machined ball-nose's two roots, and three orders BELOW the smallest
    margin any committed fixture's true cap keeps from its own mouth. The
    answer is flat across that whole band;
    ``test_r7_the_tangency_band_is_not_a_tuned_epsilon`` walks it decade
    by decade and pins that.
    """
    return 2.0 * math.sqrt(2.0 * max(radius, 0.0) * SURFACE_CONFUSION_MM)


class _AxisMaterial:
    """Whether a crossing bounds this bore, asked of the part itself.

    :func:`_root_for_end` has to choose, among the crossings of one cap
    face, the one that is the real END of the bore. Round 6 chose by
    tangency, round 7 by a band, D-19 round 1 by POSITION — inward of the
    mouth is the bore's own hollow, so no crossing there can bound the
    solid. Position is the one that reaches furthest and it is still not
    far enough, because it is a PRESUMPTION about geometry and there is a
    common part on which the presumption is false.

    A pocket with a DOUBLY-CURVED CONVEX floor — a domed seat, a form
    tool's crown, a ball-end plunge left proud — bulges its floor UP into
    the bore. The crown is inward of the mouth by however proud it
    stands, and the crown IS the floor: the metal starts there. Position
    says void; the part says metal. And the two cases cannot be told
    apart by looking at the crossing, which is the point — the crown of a
    convex floor and the upper pole of an UNDERCUT spherical seat sit in
    the same place relative to the mouth and carry the same surface
    normal, ``(0, 0, +1)`` on a Z-up bore, and one of them is the floor
    and the other is a point in mid-air.

    So the presumption is checked against the solid rather than trusted:
    a crossing is the bore's end when the axis is AIR just inward of it
    and METAL just outward of it. That is the definition of the material
    boundary the drill would stop at, and it separates the two cases
    exactly — the crown has metal under it, the undercut pole has the
    seat's own void under it.

    TWO QUESTIONS, TWO COSTS. :meth:`beyond_the_face` costs one bounding
    box of ONE face and can only ever REJECT — a crossing nowhere near
    the face whose carrier produced it is not that face's cap, whatever
    else it is. :meth:`is_exit` costs a point-in-solid query and can only
    ever ACCEPT. Neither is asked unless the cheap rules are in doubt;
    see :func:`_root_for_end`, which owns when.

    WHY A POINT CLASSIFIER IS BACK, having been removed in N2. It is not
    the same use and it is not the same cost. N2 removed a ring of
    THIRTY-SIX classifier queries PER BORE that answered "is this end
    open?" — a question the cap face's own outward normal answers for
    free, so the queries bought nothing and cost most of the subprocess
    budget. This asks a different question, one no normal can answer
    (a normal is local; "is there metal beyond this along the axis" is
    not), and it asks it only of a crossing the position rule and the
    face's own extent have left genuinely undecided. An ordinary plate, a
    through hole, a flat-bottomed pocket, a countersink — none of them
    reach it and none of them construct a classifier at all. The instance
    is built lazily and at most once per bore, and
    ``test_n2_the_point_classifier_is_rationed_not_readmitted`` pins both
    halves of that.

    WHOSE EXTENT — and D-19 round 3 is the whole of that word. Round 2
    asked the question of the PART: is this crossing beyond the extent of
    the metal, taken from the shape's own ``Bnd_Box``? A box is a
    SUPERSET of the metal, so it rejects only what is certainly off the
    part and never argues about anything near the surface — which is the
    right posture, taken of the wrong object. A bounding box of the whole
    part is a property of the WHOLE PART, and the crossing being judged
    is a property of ONE FACE of ONE BORE. Anything else on the part that
    reaches further than the crossing does enlarges the box past it and
    the refusal silently stops firing.

    Not a contrived part and not an assembly: a single body cut from one
    block. A 4 mm leg under the plate — at the far corner, 30 mm from the
    bore, touching nothing of it — restores ``far_opening_through_bore``'s
    24 mm of hole in a 20 mm plate, because the leg's foot reaches z=-4
    and so does the breakout sphere's far pole. A 2.6 mm boss standing on
    top restores ``spherical_mouth_undercut_bore``'s 13.188 and UNKNOWN.
    A 5 mm step and a 10 mm rib do it too. See
    ``test_d19r3_an_unrelated_feature_may_not_rescue_a_cap_in_mid_air``.

    So the box is taken of the FACE THAT PRODUCED THE CROSSING instead.
    The crossing is a root of the axis against that face's UNBOUNDED
    CARRIER, and this whole family of defects is the carrier's root
    landing somewhere the face is not; the face is the thing that made
    the claim and it is the thing asked to stand behind it. Nothing
    elsewhere on the part can move it.

    Still a BOX and still one-sided, deliberately, and for round 3's
    reason as much as round 2's: a doubly-curved convex face's real
    material boundary lies OUTSIDE its own trim curve — a dome's crown
    sits 0.4 mm past where the bore trims it (ADR-0112 round 3, blocker
    2) — so an on-face test would re-break the domes and is exactly what
    ``test_r4_an_on_face_trim_test_would_have_re_broken_the_domes``
    forbids. The crown is 0.4 mm outside the trim and INSIDE the face's
    box; a spherical breakout's far pole is 4 mm outside the face's box
    and goes.
    """

    __slots__ = ("_shape", "_origin", "_direction", "_classifier", "_queries")

    def __init__(self, shape, origin, direction) -> None:
        self._shape = shape
        self._origin = origin
        self._direction = direction
        self._classifier = None
        self._queries = 0

    def beyond_the_face(self, face, t: float) -> bool:
        """Is this crossing outside the extent of the face that made it?

        One-sided: True means the crossing is certainly not this face's
        cap, False means nothing at all. Reached only where the void
        bound discarded a crossing AND no crossing of the face bounds
        metal — ten times across the whole committed corpus — so the box
        is built where it is asked for rather than cached.
        """
        span = _box_axial_span(face, self._origin, self._direction)
        if span is None:  # pragma: no cover — a face with no extent
            return False
        return t < span[0] or t > span[1]

    def _inside(self, t: float) -> bool:
        point = _point_on_axis(self._origin, self._direction, t)
        if self._classifier is None:
            self._classifier = BRepClass3d_SolidClassifier(self._shape)
        self._queries += 1
        self._classifier.Perform(
            gp_Pnt(point[0], point[1], point[2]), MATERIAL_PROBE_TOL_MM
        )
        return self._classifier.State() == TopAbs_IN

    def is_exit(self, t: float, sign: float, step: float) -> bool:
        """Does the metal at this end of the bore begin at ``t``?

        Air one ``step`` INWARD (back up the bore, ``-sign``), metal one
        ``step`` OUTWARD (``+sign``). The step is a
        :func:`_tangency_band`, the module's own figure for how far a
        computed crossing may sit from where the surface really is: any
        shorter and the probes are inside the surface's own uncertainty
        and answer noise; any longer and a thin floor could be stepped
        clean over. Nothing is tuned here —
        ``test_d19_the_material_probe_is_not_a_tuned_epsilon`` walks the
        step over four decades without moving an answer.
        """
        return not self._inside(t - sign * step) and self._inside(t + sign * step)


def _box_axial_span(shape, origin, direction) -> Optional[Tuple[float, float]]:
    """One shape's bounding box, projected onto one axis.

    The box is world-axis-aligned, so for a bore that is not it this is a
    superset of a superset. That is fine and it is deliberate: the only
    claim made of it is that a crossing OUTSIDE it is off the shape, and
    a looser box makes that claim less often, never wrongly.
    """
    box = Bnd_Box()
    BRepBndLib.Add_s(shape, box)
    if box.IsVoid():  # pragma: no cover — a shape with no geometry
        return None
    x_lo, y_lo, z_lo, x_hi, y_hi, z_hi = box.Get()
    ts = [
        _dot((x - origin[0], y - origin[1], z - origin[2]), direction)
        for x in (x_lo, x_hi)
        for y in (y_lo, y_hi)
        for z in (z_lo, z_hi)
    ]
    return min(ts), max(ts)


def _void_slack(radius: float) -> float:
    """How far the wrong side of its own mouth a cap may land and still count.

    Half a :func:`_tangency_band`, and INERT — nothing in the corpus, in
    the ball-nose sweep, in the undercut-seat family or in the dome-floor
    family moves when it is set to zero, and
    ``test_d19r2_the_void_slack_is_inert_and_is_pinned_as_inert`` is what
    says so rather than this sentence.

    Kept and pinned rather than deleted, because "this knob does nothing"
    is a claim about the fix and not a tidy-up — the same posture round 7
    took with the tangency band it had just stopped depending on. The
    history is worth the four lines: round 6 broke a tangency tie with
    it, round 7 widened the tie into a band because the tie was not
    bit-stable, D-19 round 1 promoted the reason to a rule and left the
    band as this slack, and round 2 gave the one case the slack was still
    arguably for — a floor landing a hair inward of its mouth — to the
    material question instead, where it is decided rather than tolerated.

    Split out of :func:`_root_for_end` so that it can be zeroed on its
    own. It used to be spelled inline as ``0.5 * _tangency_band(radius)``,
    and the band is now ALSO the material probe's step: zeroing the band
    would break the probe and a revert-proof could not tell which of the
    two roles it had removed.
    """
    return 0.5 * _tangency_band(radius)


def _root_for_end(
    roots: Sequence[Tuple[float, Tuple[float, float, float]]],
    t_edge: float,
    at_low: bool,
    radius: float,
    t_inner: Optional[float],
    material: Optional["_AxisMaterial"] = None,
    face=None,
) -> Optional[Tuple[float, Tuple[float, float, float]]]:
    """Which crossing of ONE cap face belongs to THIS end of the bore.

    Three rules, in order — and the third one is checked against the
    SOLID rather than reasoned from position, which is what D-19 round 2
    had to add and why :class:`_AxisMaterial` exists.

    **A crossing inside the bore's own hollow is not a candidate.** The
    mouth is where the bore's wall stops; inward of the whole of it the
    axis is running through the void the bore itself cut, and no surface
    bounds the solid there. :func:`_mouth_inward_bound` is how far in the
    mouth reaches, and the comparison carries half a
    :func:`_tangency_band` of slack because a cap that meets the bore
    exactly AT its mouth — a flat floor, the plane of an angled breakout
    — is entitled to land a hair the wrong side of it.

    **Then the crossing NEAREST the edge that led to the face**, which is
    what settles a shaft's two crossings: a line meets a cylinder twice
    and the far meeting is the other end of the bore, not this one.

    The void rule is what a SPHERICAL BOTTOM needs, and machined parts
    are full of them. A ball-nose cutter leaves a sphere of its own
    radius, so the sphere is tangent to the bore, the mouth is the
    sphere's own EQUATOR, and the two poles sit exactly one radius from
    it on either side. Nearest has nothing to say, and whichever of the
    two ``GeomAPI_IntCS`` happened to list first won the end: on a Ø8
    ball-nose pocket 16 mm deep that is a reported depth of 8 and a
    verdict of THROUGH — half the depth, the wrong end condition, and an
    answer that FLIPS with the intersector's list order (ADR-0112
    adversarial round 6, blocker 2, and an S3 defect as well as a wrong
    number). The inward pole is one radius INSIDE the bore's hollow, so
    the void rule discards it without ever having to look at the other.

    HOW THIS GOT HERE, because the reasoning matters more than the rule.
    Round 6 saw the ball-nose, reasoned exactly as the paragraph above
    does — "inward of the mouth, at this end, is inside the bore's own
    void, which the bore itself hollowed out; no surface can bound the
    solid there" — and then attached that reason to the wrong TRIGGER. It
    only looked for an inward twin when the two crossings were TIED, on
    the grounds that a looser test would re-break the cases nearest
    exists for. Round 7 found the tie was not bit-stable — a tangency
    does not survive being computed, the two distances land about a ULP
    apart, and 74 of 240 randomly sized pockets went back to reading
    short and THROUGH — and widened the tie into the band above. Both
    rounds kept the trigger.

    The trigger is what D-19 broke, with the part the ball nose is one
    limiting case of: an UNDERCUT spherical seat, where the sphere's
    radius EXCEEDS the bore's by ``e`` and the cavity is wider than the
    hole that reaches it. Now the two crossings are not tied at all — the
    inward pole is the NEARER of the two, by ``2·sqrt(2·r·e + e²)`` — so
    no tie fires, nearest takes the pole in mid-void, and the miner ends
    a plainly blind pocket at a point ABOVE its floor and calls it
    THROUGH. On a 20 mm plate with the seat centred at z=12 and 0.1 mm of
    undercut: 1.9 deep against a true 14.1 on an r=6 seat, 3.9 against
    12.1 at r=4, 5.9 against 10.1 at r=2 — 42 % to 87 % of the hole gone,
    every one of them an UNDER-quote, and every one of them reported as a
    hole that goes somewhere.

    Worse at r=8, where the pole in the void lands ON the plate's top
    face: the span came out NEGATIVE and the bore was dropped, so a part
    with one hole in it mined ZERO. A silent under-count is the worst of
    the three outcomes, and it appeared and disappeared with ``e``
    crossing OCCT's confusion figure — below it the tie still fired and
    the answer was right, above it the hole vanished, and further above
    it the far-bound filter happened to catch the pole and the answer
    came back. Three regimes of one part, none of them a property of the
    part.

    So the reason is promoted to the rule and the trigger is deleted: an
    inward crossing is out because of WHERE IT IS, not because something
    else happens to be tied with it. The tangency is then the ``e = 0``
    member of the undercut family rather than a case of its own, and the
    band survives as the slack rather than as the tie.

    The bound NARROWS AND NEVER EMPTIES — where it would leave no
    candidate at all it is not applied — which is the same posture as the
    mouth-reach bound in :meth:`_EndEvidence.resolve`, and it is load
    bearing on three committed fixtures. A bore STRADDLING a convex
    rounded edge reaches the fillet along a mouth that lies entirely
    above where the fillet's carrier crosses the axis, so BOTH of that
    carrier's crossings read as void. Emptying there would silently
    change what the round-5 straddle defect looks like; the fall-back
    keeps the fillet's evidence exactly as it was, and
    :meth:`_EndEvidence._skin_over_axis` goes on being what rejects it —
    a second, independent mechanism reaching the same verdict, which is
    where it belongs.

    ── D-19 ROUND 2: THE MIRROR ────────────────────────────────────────

    Everything above reasons about a crossing from WHERE IT IS. Round 2
    is the finding that position is not enough, and it comes in a matched
    pair — the two faults compound, and either alone would have been
    caught by the numbers the other one produced.

    **The void bound discards a floor that is really there.** A pocket
    whose floor is a doubly-curved CONVEX dome crowns ABOVE the mouth: a
    form tool's crown, a domed seat, a ball-end plunge left proud. The
    crown is inward of the mouth by however proud it stands, and the
    crown IS the floor — the metal starts there. Position cannot tell it
    from an undercut seat's upper pole, which stands in the same place
    and is mid-air, and neither can the normal: both read ``(0, 0, +1)``
    on a Z-up bore. So the presumption is put to the solid. See
    :class:`_AxisMaterial`.

    **And then the OTHER crossing wins, unchallenged.** Discarding the
    crown does not end the face's turn — the nearest pick simply takes
    what is left, and what is left is the dome's far pole, which the far
    bound does not constrain and which no rule had ever asked to be
    anywhere near the part. On a Ø12 pocket in a 60 x 60 x 20 plate with
    a crown standing 1.2e-3 mm proud, that is a bore 30012 mm deep,
    reported THROUGH, entering 29992 mm below a 20 mm plate. It flips at
    exactly the slack — 1.1e-3 mm at r=6 — so the same pocket a micron
    flatter answers perfectly, which is the signature of an answer that
    is not a property of the part. Six of 260 random parts, every one of
    the dome-floored sub-family, and all of them right on the round
    before.

    The same pair, without a dome: a Ø8 bore fused with a sphere that
    breaks out through the plate's bottom face reported 24 mm of depth in
    a 20 mm plate, and a Ø4 bore with a spherical undercut at its MOUTH
    put its far end 2.6 mm above the plate's own top face and called the
    part UNKNOWN.

    So: where the bound discards, what is left has to earn the end.
    A crossing the axis really does leave the metal at takes it outright,
    whichever side of the mouth it fell on. Failing that, a survivor
    OUTSIDE THE EXTENT OF THE FACE THAT PRODUCED IT is refused and the
    face contributes no cap at all — the one path here that returns
    ``None`` — and the end falls back to the bore's own parametric bound,
    a number the bore's wall stands behind. Where the bound discards
    NOTHING not one of these rules runs, no classifier is built, and
    every answer is the round-1 answer to the bit.

    ── D-19 ROUND 3: WHOSE EXTENT ──────────────────────────────────────

    Round 2 refused that survivor for being outside the PART'S extent,
    read off the shape's world bounding box. That is a property of the
    whole part, and the crossing is a property of one face of one bore:
    any unrelated feature that reaches further than the crossing does — a
    2.6 mm boss, a 4 mm leg, a 5 mm step, a 10 mm rib, none of them
    touching the bore — enlarges the box past it, turns the refusal off,
    and brings both of round 2's over-quotes straight back. 24 mm of hole
    in a 20 mm plate, and a bore ending 2.556 mm above the plate's own
    top face and reading UNKNOWN, on single bodies cut from one block.

    The extent asked for is the FACE'S, which is the thing that made the
    claim: the crossing is a root of the axis against that face's
    unbounded carrier, and the defect is always the carrier running on
    past the face. Nothing else on the part can move it. See
    :meth:`_AxisMaterial.beyond_the_face`.

    NOT closed by any of this, and recorded rather than half-fixed: a
    TOROIDAL undercut — an O-ring or snap-ring gland at the bore's end —
    still reads short and THROUGH. A ring torus never crosses its own
    axis, so that face offers no crossing at all and nothing here is
    asked to choose. See ``docs/BACKLOG-designed-to-live.md``, D-19.
    """
    sign = -1.0 if at_low else 1.0

    if t_inner is None:
        # An edge whose curve OCCT declines to read has no bound to
        # give. The same edge would already have failed
        # `_edge_axial_mean` and been skipped upstream, so this is
        # defence rather than a live path — and it is also the seam the
        # mouth-ray-floor revert-proof reaches through to take the bound
        # away deliberately.
        live = list(roots)
    else:
        slack = _void_slack(radius)
        live = [root for root in roots if sign * (root[0] - t_inner) >= -slack]
        if material is not None and len(live) != len(roots):
            # THE BOUND HAS JUST DISCARDED SOMETHING, which is the only
            # situation in which either of the next two rules can bite,
            # and the reason they are inside this arm rather than above
            # it. A crossing the bound KEEPS was reached from the bore's
            # own mouth: it sits on the mouth or outside it, and the
            # mouth is boundary of the solid, so the part vouches for
            # it. A crossing that is left over only BECAUSE its nearer
            # sibling was discarded has no such standing — nothing
            # connects it to the mouth, and the far bound does not reach
            # it either. So when the bound discards, what is left has to
            # earn the end rather than inherit it.
            step = _tangency_band(radius)
            exits = [
                root for root in roots if material.is_exit(root[0], sign, step)
            ]
            if exits:
                # A crossing the axis really does leave the metal at IS
                # the end of the bore, whichever side of the mouth it
                # fell on — see :class:`_AxisMaterial`. This is what
                # rescues a CONVEX floor, whose crown stands proud of
                # the mouth and is still the floor, without rescuing an
                # UNDERCUT seat's upper pole, which stands in the same
                # place and is still mid-air.
                #
                # The INNERMOST of them: walking out of the bore, the
                # first metal is the floor, and anything further out is
                # behind it where the drill never went.
                return min(exits, key=lambda root: sign * root[0])
            if live:
                # No crossing of this face bounds metal, and the bound
                # has taken the ones with any claim on the mouth. What
                # survives was promoted by the discard alone, so a
                # crossing NOWHERE NEAR THE FACE that produced it is
                # refused outright rather than capping the bore with a
                # point in mid-air — a spherical breakout's far pole 4 mm
                # under a plate, a flat dome's far pole 30 metres under
                # it.
                #
                # The FACE's extent, not the part's. Round 2 asked the
                # part's bounding box and a 4 mm leg at the other corner
                # of the plate turned the refusal off; see
                # :class:`_AxisMaterial`.
                #
                # Refused, and not replaced: the face keeps its vote on
                # openness as a TOUCHING neighbour, and the end falls
                # back to the bore's own parametric bound, which is a
                # number the bore's own wall stands behind.
                if face is not None:
                    live = [
                        root
                        for root in live
                        if not material.beyond_the_face(face, root[0])
                    ]
                if not live:
                    return None
    return min(live or roots, key=lambda root: abs(root[0] - t_edge))


def _walk_caps(
    group, ancestors, p_lo, p_hi, material=None
) -> Tuple[_EndEvidence, _EndEvidence]:
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

    Three filters on every candidate root:

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
    - It must not lie inward of the bore's own MOUTH in this face. Past
      the mouth the axis is in the hollow the bore itself cut, where no
      surface bounds the solid — see :func:`_root_for_end`, which owns
      this one because the round-6 ball nose and the D-19 undercut seat
      are the same rule seen twice.
    - Of the roots that survive, only the one NEAREST THE EDGE that led
      to this face is kept. A line crosses a shaft's OD twice, and the
      far crossing is the other end of the bore, not this one.

      This settles the several roots of ONE face and nothing more, which
      is worth saying because round 4 found the defect it does NOT reach:
      a contest between two different faces at the same end. Each of them
      keeps its own nearest root perfectly correctly, and the wrong one
      still won the end. That is :meth:`_EndEvidence.resolve`'s to fix,
      and it fixes it with the mouth this walk records here.
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
                # How far back up the bore this piece of mouth reaches.
                # Everything past it is the bore's own hollow.
                t_inner = _mouth_inward_bound(
                    edge, origin, direction, -1.0 if at_low else 1.0
                )
                # The far bound for THIS end: a low-end cap may lie as far
                # below the bore as its own curvature carries it, but not
                # above the high end.
                far = p_hi + pad if at_low else p_lo - pad
                for face in neighbours:
                    if any(face.IsSame(own) for own in group.faces):
                        continue
                    end.touching.append((face, edge_point))
                    # This edge is part of the bore's MOUTH in that face.
                    # Recorded before the root is looked for, because the
                    # ownership test needs the whole mouth even when this
                    # particular edge contributes no crossing.
                    key = end.note_mouth(face, edge)
                    roots = [
                        root
                        for root in _cap_axis_intersections(face, origin, direction)
                        if (root[0] <= far if at_low else root[0] >= far)
                    ]
                    if not roots:
                        continue
                    picked = _root_for_end(
                        roots, t_edge, at_low, group.radius, t_inner, material, face
                    )
                    if picked is None:
                        # Every crossing this face offered is nowhere
                        # near it. The face still stands as a TOUCHING
                        # neighbour and still votes on openness — it is
                        # a real wall of the bore — but it has nothing
                        # to say about WHERE the bore ends, so the end
                        # falls back to the bore's own parametric bound
                        # rather than to a point in mid-air.
                        continue
                    t_cap, normal = picked
                    end.caps.append(
                        (
                            t_cap,
                            face,
                            _point_on_axis(origin, direction, t_cap),
                            normal,
                            key,
                            edge,
                        )
                    )
            except Exception:  # noqa: BLE001 — one odd edge must not kill the bore
                continue

    return low, high


def _resolve_span(
    group, ancestors, p_lo, p_hi, material=None
) -> Tuple[float, float, bool, bool]:
    """The bore's real axial span AND the openness of both its ends.

    One cap walk, both answers — see :func:`_walk_caps` and the module
    docstring's "Why UVBounds is not the answer" and "Open or capped".
    """
    low, high = _walk_caps(group, ancestors, p_lo, p_hi, material)
    origin, direction = group.origin, group.direction
    t_lo, lo_open = low.resolve(p_lo, origin, direction, -1.0, group.radius)
    t_hi, hi_open = high.resolve(p_hi, origin, direction, +1.0, group.radius)
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
                group,
                ancestors,
                group.lo,
                group.hi,
                _AxisMaterial(shape, origin, direction),
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
