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

## What a fix has to do

Stop the march where the edge stops being an edge. The failure is not the ray
test and not the cap walk; it is `_barrier_track` reconstructing edge past a
vertex the bore did not create. Any fix must keep the 26 correct parts that also
exhaust the march budget, and must leave the 43 committed fixtures bit-identical.

Not built. Held for Ervin.
