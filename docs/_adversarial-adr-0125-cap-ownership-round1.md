# Adversarial review — ADR-0125 round 1 (2026-09-16)

**Verdict: FIX-FIRST, and the fix is measured and in hand.** The decision
survives every surface it named, and one of them — the adjacency gate — was
genuinely too wide. A strictly narrower gate was built and measured to be
behaviourally identical, so the amendment costs nothing and closes the surface
by construction. Two findings the ADR did not have are added: one is a safety
property strong enough to change how this should be reviewed, the other is a
missing pin.

Everything below was measured against the tree at `3fd7644`.

---

## A1 (FIX — amendment required) — the adjacency gate asked the wrong question

**Claim under test.** ADR §5 surface 2: D2 gates the veto on whether the two
faces "still meet on the solid", not near this mouth. Two faces meeting on the
far side of a part would wrongly suppress the veto.

**The objection is correct.** Whether two faces touch somewhere else on the
part says nothing about whether the bore consumed their junction *here*, which
is the only thing D2 is trying to detect.

**Measured fix.** Scope the gate to the mouth: a shared edge counts only when
it has a vertex in common with one of this rim's mouth edges — which is exactly
the `cut`-vertex test `_rim_barriers` already uses to decide that the bore
interrupted an edge. Re-measured:

| | wide gate | mouth-scoped gate |
|---|---|---|
| boss family (baseline 10 wrong) | 0 wrong | **0 wrong** |
| caps dropped | 178 | **178** |
| 50 committed fixtures | bit-identical | **bit-identical** |
| hole tests red | 4 | **4 (the same four)** |

Behaviourally indistinguishable on everything the corpus can see, and strictly
narrower. **Take it** — a gate that is wider than its justification is a
latent defect even when nothing currently exercises it.

**D2 is amended accordingly.**

---

## A2 (NEW — strengthens the decision) — the veto cannot under-quote, by construction

The ADR argues correctness but never states the direction of error, and the
direction is the thing D-19 actually cares about.

`_rim_winner` takes `min(sign * cap[0])` over the surviving caps. The veto only
ever **removes** caps. Removing members of a `min` can only move the level
**outward**, which means the entry moves away from the material and the hole is
reported **deeper**. So:

> The buried veto can make a hole deeper. It can never make one shallower.

Measured over the whole family: **10 deeper, 98 unchanged, 0 shallower.**

This matters for how the change should be reviewed. Its failure mode is
over-quote, never under-quote — the opposite of the defect it fixes, and the
side D-19's convention deliberately errs on ("conservative, never
under-quote"). It does **not** make the change safe to wave through: an
over-quote is still wrong, and pushing an entry into mid-air above the part is
precisely the rounds 4/5 failure class. But it bounds the blast radius to the
visible direction rather than the invisible one.

**What actually bounds mid-air entries** is worth stating in the ADR, because
it is not obvious: every cap carries the mouth edge that found it (`cap[5]`),
so a cap the veto promotes is always a face that **bounds the solid at this
mouth**. The veto cannot invent a level from an unbounded carrier that has no
edge in the rim — which was round 4's hijack exactly.

---

## A3 (clears) — the untrimmed-carrier hazard is bounded, but not to zero

**Claim under test.** §5 surface 1: M3 died of the untrimmed-carrier hazard and
M5 only fences it off. Is there a part where two faces are genuinely severed
*and* the outer one's carrier passes over the axis without its material being
there?

**Pressed three ways.**

1. **The corpus already contains the hazard shapes** — `bore_into_fillet`,
   `bore_straddling_a_concave_fillet`, `blind_bore_under_dome`,
   `undercut_ball_seat`, `bore_through_spherical_dome`, three ball-nose parts,
   three conical bosses. All 50 stay bit-identical.
2. **The sphere attack is self-limiting.** A dome whose carrier dips below the
   plate's bottom face needs a radius large enough that its base circle is
   wider than the plate, so it stops being a dome sitting on a plate. I could
   not construct the part.
3. **Rim scoping bounds it further.** The veto only considers caps of the
   winning rim, and (A2) every such cap's face has a mouth edge there.

**Residual, stated honestly:** this is a bounded argument, not a proof. A face
that genuinely bounds *this* mouth, crosses the axis outside, and has no
material on the axis between the two crossings would still defeat it. Nothing
in the corpus or in the three constructions is such a part. **The ADR should
carry this as a known limit rather than a cleared surface.**

---

## A4 (clears) — D3 step 3, and the multi-rim interaction

**Step 3** (buriedness overruling reachability entirely) is the aggressive move.
The parts that exist to catch exactly that — the four straddles
(`test_r6_keeping_every_rim_face_re_breaks_the_straddles`'s subjects), the
chamfer and corner families of rounds 4 and 5, the ball-nose verdicts and
`undercut_ball_seat` — all return bit-identical answers. The one docstring
case the ADR flagged, the ball fused on a block's corner, is covered by
`undercut_ball_seat` and does not move.

**Multi-rim:** `coaxial_split_faces.step` — one bore severed into two faces by
a slot, the part the two-rim logic exists for — is bit-identical. The veto is
scoped to the rim `_rim_winner` already chose, so it cannot change which rim
wins, only where within it the bore ends.

---

## A5 (FIX — make the required action concrete) — the three counter-pins

The ADR forbids re-blessing them green, which is right, but leaves *what to do*
unstated. Per pin, the required action:

- `test_r8_the_boss_family_..._is_improved_not_closed` — its `assert wrong > 0`
  is a deliberate tripwire. **Rewrite it as closed**, and keep the family sweep
  with `wrong == 0` so a regression reds it.
- `test_r6_keeping_every_rim_face_re_breaks_the_straddles`
- `test_r7_evenly_spaced_rays_alone_re_break_the_boss`
- `test_r8_a_seam_barrier_re_opens_the_whole_band`

For the last three, the pin must be **re-pointed to a part where its guard is
still solely load-bearing** — disable the guard *and* the buried veto together,
and assert the old wrong answer returns. That keeps the pin honest about what
it protects instead of asserting a redundancy that no longer holds. Round 8
already did this once for the r7 pin, so the precedent and the technique both
exist. Only if no such part can be found is the guard genuinely redundant, and
then it and its pin go together, deliberately.

---

## A6 (NEW — missing pin) — nothing in the corpus has a filleted boss base

The corpus has conical bosses and it has fillets, but no boss with a **filleted
base** — and that is precisely the shape where D2's gate stops suppressing,
because the fillet sits between the cone and the plate top so those two faces
never meet at all.

Built six such parts (cone radii 8–12, heights 14–25, fillet radii 1.5–4.0,
bore offsets 0.5–2.0). Baseline: 6/6 correct. Under M5 with either gate:
**6/6 correct, unchanged.** So the gate permits the veto there and it does no
harm — but that was luck until it was measured, and nothing in the suite would
have caught it going the other way.

**Required:** commit the filleted-boss family as a fixture or an in-memory pin
as part of the build.

---

## Confirmed sound — pressed and held

- **Level-awareness is the load-bearing half of D1.** M3 (the same test without
  it) is 75 wrong. The ADR's framing — an untrimmed half-space is true of
  almost every point of a solid — is right and is why.
- **D3 step 3 is necessary, not decorative.** In the exemplar `standing` is
  `[Plane 20.0]` and nothing else, so a veto confined to `standing` leaves it
  empty and changes no answer; measured, the family stays at 10 wrong.
- **Ties are safe.** The veto requires the other cap to be *strictly* outside,
  so two faces agreeing at one level are never vetoed — consistent with
  `_rim_winner`'s existing tie rule that all caps at the winning level vote.
- **No new order dependence.** The veto filters a list and the level is a `min`
  over levels, so OCCT's face-walk order still cannot reach the answer (S3).
- **Cost is not a concern.** 1164 ms baseline vs 1154 ms over all 50 fixtures;
  the veto runs only on rims where the bore cut an edge.

## Verdict

**FIX-FIRST.** Amend D2 to the mouth-scoped gate (A1); add A2's direction-of-
error property and the mouth-edge bound to the ADR; carry A3's residual as a
stated limit rather than a cleared surface; make A5's per-pin action explicit;
add A6's filleted-boss pin to the build. D1, D3, D4 and D5 stand unchanged.
