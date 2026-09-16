# Measurement — ADR-0112 R3: the boss-side `_barrier_track` mechanism

**Date:** 2026-09-16. **Status:** measurement only — no fix built, per Ervin's
instruction to report the mechanism before touching it.

**Verdict: a clean bug, not a convention question.** It is a cap-SELECTION
failure driven by a barrier the code fabricates out of an edge that does not
exist, and it is decidable from the part alone. No depth convention enters it.

## The family

The pin `test_r8_the_boss_family_OUTSIDE_the_band_is_improved_not_closed`
sweeps a seeded LCG over `_boss_part` — a bore beside a conical boss whose axis
stands on the plate's own edge (x = 40). 400 draws, 108 qualify.

Measured on `4c6133a` (i.e. with the R9 half-span fix in): **98 correct, 10
wrong** — the count the pin already records, unchanged by R9.

**Every one of the 10 returns exactly `20.0` mm — the bare plate thickness.**
The boss contributes nothing to the answer.

## Discriminators tested and REJECTED

Each is true of all 10 failures and therefore looks like the cause; each is
also true of many correct parts, so none of them is.

| candidate | failures | also true of |
|---|---|---|
| bore crosses the plate edge (`bx+br > 40`) | 10/10 | 65 correct |
| bore swallows the cone's axis (`off < br`) | 10/10 | 46 correct |
| wide swallow margin (`br − off`) | — | not separating: 3.408 correct, 3.048 wrong |
| barrier march exhausts its 48-step budget | 10/10 | 26 correct |

## The exact discriminator — 10/10 failures, 0/98 correct

At the top rim, `_EndEvidence::_skin_over_axis` **drops the Cone caps and keeps
the Plane cap.** Instrumented, on the failing part
`cz=5.2490 cr=10.5986 ch=20.4439`, bore `(38.7991, 19.6701) r=4.6180`:

```
candidates = [Cone 23.2906, Cone 23.2906, Plane 20.0, Cone 23.2906]
kept       = [Plane 20.0]                       -> answer 20.0   (want 23.2907)
```

Its near-twin that passes — `cz=7.8310 cr=12.8092 ch=21.0530`, bore
`(38.8147, 21.0891) r=4.5179`, a bore of almost identical size and position —
presents the same candidate mix and keeps the Cone.

**The cone caps carry the correct value already.** Nothing is mismeasured;
the right answer is computed and then thrown away.

## The fabricated barrier

Every barrier edge at this rim lies in the plane **x = 40** — the plate's own
side face, which is also the cone's **axial** plane, because the fixture stands
the cone's axis on the plate edge. A plane through a cone's axis cuts it in two
straight **generators**, and those two generators are the runaways: measured
direction `(0, ∓0.460, −0.888)`, matching the cone's half-angle exactly
(`cr/ch = 10.5986/20.4439 → 0.4602 / 0.8878`).

Each is marched from the rim vertex where the bore cut it, away from its
surviving stub — which is **upward, toward the apex**:

```
apex (40.000, 20.000, 25.693); plate top z = 20
runaway A: from (40.000, 15.211, 16.455) -> (40.000, 21.587, 28.755)
runaway B: from (40.000, 24.129, 17.728) -> (40.000, 17.753, 30.027)
```

Both **end above the apex, in empty space** — 3.1 mm and 4.3 mm past the tip of
the cone. Above z = 20 the plate's side face does not exist, so the generator
stopped being an edge of the solid there; `_barrier_track` walks the untrimmed
line on regardless. That is the round-8 note's own mechanism, confirmed: the
march assumes the BORE is what cut the edge, and here the edge was terminated
by the plate's top face instead.

Because the walk is heading toward the axis rather than away from it, it never
leaves the mouth's footprint and burns all 48 steps — the `n=49`,
`max_r == start_r == radius` signature. In 2-D the two tracks are collinear and
adjacent:

```
runaway A: (4.459, 1.201) -> (-1.917, 1.201)
runaway B: (-4.459, 1.201) -> ( 1.917, 1.201)
```

Together they tile the **whole chord** x ∈ [−4.459, 4.459] at a constant
1.201 mm from the axis — that second coordinate is the plane x = 40 itself. The
result is a continuous wall laid straight across the mouth, between the axis and
the cone's share of it. The ray test then finds every route from the axis to the
cone's mouth blocked, and the route to the plate's top face open.

## Why the kept cap is wrong under ANY convention

In all 10 failures `off < r20`, measured 10/10, where
`r20 = cr·(1 − (20 − cz)/ch)` is the cone's radius where the boss meets the
plate top. `off < r20` puts the bore's axis **inside the boss's footprint**, so
the plate's top face has a hole there and is not the part's skin over the axis
at all. The cap that wins is a crossing of that face's carrier **plane** in the
middle of the hole in the face — precisely the artefact `_skin_reaches_axis`
exists to reject.

## Not a convention question — measured, not argued

For all 98 correct parts the W value (full-diameter wall extent) differs from
the P value (axis-first-material, Ervin's convention B). The code answers:

```
P : 98/98        W : 0/98
```

The code implements P consistently. In the 10 failures the returned `20.0` does
coincide with the W value, but only because W collapses to the plate top
whenever the bore's wall leaves the boss below z = 20 — and the cap evidence
above shows the boss was never weighed, not that a second convention was
applied. A part cannot be P for 98 members of a family and W for 10.

## ⚠️ CORRECTION — the fix does NOT belong in `_barrier_track`

The section this replaces concluded "stop the march where the edge stops being
an edge". **That was wrong, and building it proved it wrong.** Two candidate
fixes were built and measured; both are falsified. Recorded here because the
falsifications are what locate the real defect.

### Attempt 1 — drop un-re-emerged marches. REJECTED

The walk's own premise is re-emergence: the bore took the MIDDLE out of an edge,
so the curve leads back to the other stub, and leaving the footprint is what
proves it. A march that burns its whole budget without leaving never re-emerged,
so it is not a barrier. Implemented; measured on the 108:

```
before: 10 wrong        after: 11 wrong   (the same 10, plus a new regression)
```

It fixed nothing and broke a part that was right — so an un-re-emerged march is
doing necessary work elsewhere, and the runaway is a real artefact that is **not
the blocker**. Patch kept at `scratchpad/attempt1-reemergence.patch`.

**Why it could not have worked**, found by logging which barrier blocks each
ray: the rays to the cone are blocked by the plate's **real** top edge at
x = 40 — 27 rays each — not by the runaways. The fabricated barrier is genuinely
fabricated and genuinely harmless.

### Attempt 2 — reconstruct the division the bore consumed. REJECTED

Better diagnosis: in 8 of the 10 failures the bore eats the **entire junction**
between boss and plate top (`off + r20 <= br`, 8/8 with **zero** false positives
across all 108). The edge that should divide the plate's top face from the boss
is not merely mis-marched — it does not exist, so the plate's top face has a
clear ray to the axis it does not own.

That edge is recoverable from the faces' untrimmed carriers, exactly as
`_cap_axis_intersections` recovers surfaces the bore removed. Implemented with
`GeomAPI_IntSS` over each rim face pair (18 reconstructed divisions on the
exemplar). Measured: **still 10 wrong**, and the cap log says why:

```
cand=[Cone 23.2906, Cone 23.2906, Plane 20.0, Cone 23.2906]   kept=[]
```

The reconstructed boss-base circle **encircles the axis**, so it blocks every
ray — to the cone as well as to the plate. `standing` goes empty, the caller
falls back to "keep every cap", and `min(20.0, 23.2906) = 20.0` returns the same
wrong answer.

## The actual root cause — a limit of the ownership MODEL

`_skin_reaches_axis` decides ownership by **reachability**: the owner of the
axis must have a piece of mouth the axis can see. When the bore consumes the
owner's entire footprint around the axis, the owner has no such mouth — its only
surviving faces are outboard, behind a real edge. No barrier scheme can repair
that, because the fault is not a missing or spurious barrier: **the evidence the
model needs is not in the topology at all.**

Note too that the filter is not merely mis-ranking. `_rim_winner` takes the
INNERMOST crossing among reaching faces, so keeping every cap also yields 20.0
(`min(20.0, 23.2906)`). Only actively *dropping* the plate's top face reaches
23.2907 — which is exactly what the filter does correctly on the passing twin.

The correct criterion is skin **continuation**, not zero crossings: a ray from
the axis to the cone's outboard mouth crosses the boss-base circle
(cone → plate-top) and then the plate's top edge at x = 40 (plate-top → cone
overhang), ending on the same skin it started on. Two changes, same owner. The
present test counts any crossing as disqualifying because the tracks carry no
side information — they are polylines, with no record of which face lies on
each side.

## What this needs — and why it is not being patched in here

A fix means either carrying face-side information on every barrier and replacing
reachability with a parity/continuation test, or replacing the ray test with a
direct "is this cap buried under another rim face's material at the axis"
predicate. Both change the **core reckoning** of `_rim_winner` — the same core
that rounds 4, 5 and 6 each broke in turn, each time on a money path.

That is an ADR-level design change with its own adversarial round, not a patch
smuggled into a residual cleanup. **R3 is therefore measured and characterised,
not fixed.** The existing pin already bounds it (`wrong <= 10`) and records it as
open; nothing regressed.

This is not a depth-convention question. Under P the exemplar's answer is
unambiguous: the bore's axis meets cone material at z = 23.2907 and the
extractor reports 20.0, 3.29 mm shallow. The convention is settled; the
extractor cannot yet reach it.
