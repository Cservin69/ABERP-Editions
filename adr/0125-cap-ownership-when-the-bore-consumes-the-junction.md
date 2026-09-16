# ADR-0125 — Cap ownership when the bore consumes the junction (ADR-0112 R3)

- **Status:** **Proposed** (2026-09-16) — **held for Ervin's review before any
  code lands.** The design below is not speculative: it was prototyped and
  measured, and so were four alternatives, three of which are falsified. No
  production code is changed by this ADR.
- **Date:** 2026-09-16
- **Deciders:** pending.
- **Related:** ADR-0112 R3 (the residual), `docs/_measurement-adr-0112-r3-boss-barrier.md`
  (the measurement this builds on), D-19 (drilling cycle-time pricing — the
  money path this feeds).

## 1. Context — a measured under-quote the current model cannot reach

`test_r8_the_boss_family_OUTSIDE_the_band_is_improved_not_closed` sweeps a
randomised 108-part family: a bore beside a conical boss whose axis stands on
the plate's own edge. **Ten of the 108 read the depth wrong, and all ten return
exactly `20.0` — the bare plate thickness.** The boss contributes nothing.

Exemplar: cone `z=5.2490 r=10.5986 h=20.4439`, bore at `(38.7991, 19.6701)`
`r=4.6180`. Depth under convention P (ruled 2026-09-16 — the deepest point on
the bore's **axis**) is **23.2907**; the extractor returns **20.0**, i.e.
**3.29 mm short**. Short is the under-quote direction, and the invisible one.

This is not a convention question. The bore's axis meets cone material at
23.2907 and the code says 20.0; measured across the family, the code answers P
in 98/98 of the parts it gets right and W in 0/98, so it implements P
consistently and simply fails here.

### Why the current model cannot express the answer

`_rim_winner` takes the **innermost** axis crossing among the rim faces whose
skin **reaches the axis**, and `_skin_reaches_axis` decides reaching by
**reachability**: a ray from the axis to a piece of that face's own mouth,
crossing no edge the bore cut.

In these ten parts the bore consumes the **entire junction** between boss and
plate top (`off + r20 <= br`, true in 8 of the 10 and in **0 of the 98**
correct parts). The boss's only surviving faces are outboard, behind the
plate's real top edge at `x = 40`. So:

- the cone caps are computed **at exactly the right value** and then discarded;
- the plate's top face wins, although at the bore's axis it has a hole in it —
  `off < r20` in **10 of 10** failures, so the axis is inside the boss's
  footprint and the top face is not the skin there at all.

The owner of the axis has no mouth the axis can see. **That is a limit of the
ownership model, not a bad barrier** — which is what makes this an ADR rather
than a patch.

## 2. Evidence — five mechanisms, measured

Each was built and run against the 108-part family. This section exists so the
next reader does not re-propose a dead end; three of these look obviously right
until measured.

| # | mechanism | family (baseline 10 wrong) | verdict |
|---|---|---|---|
| M1 | drop marches that never re-emerge from the mouth | **11 wrong** | **falsified** — worse, and regressed a passing part |
| M2 | reconstruct the consumed division from the untrimmed carriers (`GeomAPI_IntSS`) | **10 wrong** | **falsified** — the reconstructed circle *encircles* the axis, blocking every ray including the cone's; `standing` empties and the fallback returns 20.0 again |
| M3 | cap is buried if inside any other rim face's carrier material | **75 wrong** | **falsified** — an untrimmed half-space contains nearly every point of a solid; 423 caps dropped |
| M4 | M3 made **level-aware and pairwise** (see §3) | **0 wrong** | closes the family, but reds 14 real tests |
| M5 | M4 + the **adjacency gate** (§3, D2) | **0 wrong** | **proposed** — reds no real test; see §4 |

M1's autopsy is the useful one: logging which barrier blocks each ray showed the
plate's **real** top edge at `x = 40` doing it — 27 rays each. The runaway march
past the cone's apex is a genuine artefact and is **not** the blocker. The
earlier note concluded the fix belonged in `_barrier_track`; that is wrong and
is corrected in the note.

## 3. Proposed decision

### D1 — A cap the axis cannot leave through is BURIED, and is out of the reckoning

Under P the entry is the first material the axis meets coming in from outside.
So: cap `C` at axial level `L` is **buried** when another cap `D` of the same
rim, on a **different face**, crosses the axis strictly **outside** `L`, and the
midpoint of the axis segment between them lies **inside `D`'s face material**.

The material test is a projection of that midpoint onto `D`'s carrier, with the
sign taken along the surface normal, flipped for a `TopAbs_REVERSED` face.

Level-awareness is the whole point, and is what M3 lacked: an untrimmed
half-space is true of almost every point of a closed solid (a plate's top face
is "inside" its bottom face's half-space), so the question only carries
information when asked about **the segment between two crossings**.

### D2 — The veto applies only where the two faces NO LONGER MEET

If the two faces still share an edge on the solid, the junction survived, the
existing barrier machinery already has that evidence, and the veto must keep its
hands off. The veto exists precisely for what that machinery cannot see: a
junction the bore **consumed**, leaving the two faces not touching at all.

This gate is what separates M5 from M4, and it is worth 10 reds: without it the
chamfer and corner families (rounds 4 and 5) break, because a chamfer's carrier
legitimately contains the axis segment above the flat top while the chamfer is
still adjacent to it.

### D3 — Reachability narrows; buriedness VETOES

Composition, in order:

1. `_skin_over_axis` narrows as today;
2. the buried veto is applied to the **whole rim cap set**;
3. if every cap reachability chose is buried, **its choice was wrong** and
   buriedness stands alone;
4. if buriedness would empty the field, it yields — round 5's "keep every cap".

Step 3 is load-bearing and is where R3 is actually won: in the exemplar
`standing` is `[Plane 20.0]` and nothing else, so a veto that could only filter
*within* `standing` leaves it empty and changes no answer. Measured: applying
the veto inside `standing` only leaves the family at **10 wrong**.

### D4 — Scope: the geometry layer only, no pricing change

Depth values move only where they were wrong. The drilling model consumes
`depth_mm` unchanged; no rate, no formula, no schema. D-19's drilling rates
still ship inert until an operator sets real feeds, so **no quoted price moves
on landing** — this corrects the input before it is ever priced.

### D5 — Tolerance

The inside test uses a fixed `1.0e-6` mm band, and a point within it counts as
**on** the surface, not inside — so a cap that merely touches another face's
carrier is not vetoed. This must be pinned by a test that fails if the band is
removed, or recorded as unreachable in the corpus the way
`_root_is_in_its_own_half`'s `pad` already is.

## 4. What M5 measured — including the part that needs a decision

- **R3 family: 10 wrong → 0 wrong.** The family closes completely.
- **All 50 committed fixtures bit-identical**, at `%.17g` on diameter, depth,
  end condition, entry point and axis.
- **No measurable cost:** 1164 ms baseline vs 1154 ms for all 50 fixtures. The
  veto only runs on rims where the bore cut an edge.
- **122 hole tests: 4 red, and none of them is a wrong answer.**

Those four need an explicit decision, because three of them are guards:

1. `test_r8_the_boss_family_..._is_improved_not_closed` asserts `wrong > 0` —
   a deliberate tripwire so that closing the residual forces the claim to be
   rewritten. It firing is **correct** and the test gets rewritten as closed.
2. `test_r6_keeping_every_rim_face_re_breaks_the_straddles`
3. `test_r7_evenly_spaced_rays_alone_re_break_the_boss`
4. `test_r8_a_seam_barrier_re_opens_the_whole_band`

(2)–(4) are REVERT-PROOF counter-pins: each disables a guard and asserts the old
wrong answer returns. Under M5 it does not, because the veto independently gets
those parts right. **That is subsumption, not regression** — the fixtures are
bit-identical and no real assertion moved.

**It is still a decision, and it must not be settled by re-blessing.** Each of
the three guards must be either (a) shown still solely load-bearing on some
other part, and its counter-pin re-pointed there — which is exactly what round 8
already did once for (3) — or (b) found genuinely redundant and removed together
with its pin. Quietly relaxing a counter-pin to green is the one outcome this
ADR forbids: these guards are the reason rounds 6, 7 and 8 stayed closed.

## 5. Adversarial surfaces — attack these before Accepted

1. **The untrimmed-carrier hazard, again.** M3 died of it and M5 only fences it
   off with the adjacency gate. Is there a part where two faces are genuinely
   severed by the bore *and* the outer one's carrier passes over the axis
   without its material being there? A tangent fillet, or a boss whose carrier
   sweeps back over the plate, are the shapes to hunt.
2. **Does the adjacency gate have the right sense?** It asks whether the faces
   meet *anywhere on the solid*, not near this mouth. Two faces meeting on the
   far side of the part would wrongly suppress the veto.
3. **Is D3 step 3 safe?** It lets buriedness overrule a reachability answer
   entirely. The ball-on-corner case in `_skin_over_axis`'s docstring is the one
   to re-measure.
4. **Multi-rim parts.** The veto is scoped to the winning rim; a bore crossed by
   a slot has two, and the interaction is unexamined.
5. **Was 20.0 ever right?** Under W the ten failures' `20.0` coincides with the
   correct value. P is ruled, but the coincidence should be stated rather than
   discovered later.

## 6. Consequences

- One under-quoting family closes; nothing else moves.
- A second ownership mechanism joins the reckoning, and the module gains a
  geometric predicate that asks about material rather than connectivity.
- Three counter-pins must be re-pointed or retired (§4), deliberately.
- `_rim_winner` gains its first change since round 6 — the reason this is held
  for review rather than landed.

## 7. Alternatives considered

- **Leave it.** The pin bounds it at `wrong <= 10` and records it as open. Costs
  nothing today, because drilling rates ship inert — but it is an under-quote
  wired into the money path, waiting for the first operator to set a feed.
- **Side-aware barriers with a parity test** — carry both faces on every
  division and decide ownership by which region the axis lands in. More
  faithful to the real structure, and strictly larger: it needs the M2
  reconstruction to work, which measurement says is the harder half.
- **M1/M2/M3.** Falsified; see §2.
