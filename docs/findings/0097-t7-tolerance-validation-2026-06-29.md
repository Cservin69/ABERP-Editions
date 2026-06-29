# ADR-0097 T7 — tolerance-cost validation golden: seed provenance + per-line decomposition

- **Date:** 2026-06-29
- **Scope:** ABERP-Editions tree only (Defense + Portable). Frozen prod never touched.
- **Companion:** ADR-0097, `docs/quote-tolerance-cost-driver-plan.md` (T7),
  `crates/aberp-quote-engine/tests/tolerance_validation.rs`.

## What this validates

The end-to-end proof that the machining-tolerance cost driver (ADR-0097 T1–T3)
prices a tighter tolerance / critical-feature callout **correctly, itemised, and
inert-by-default** on a *real* part — the Ø100 compound planetary gearbox already
pinned in `planetary_box_validation.rs` (ADR-0094 S7). Two components carry a
callout, exactly as the plan's T7 names them:

- the **sun** with a Ø-bore **H6** critical fit, and
- the **ring** with an **UltraPrecision ground face**.

The same parts at the default class with no callouts (and the production
zero-contribution seed) price **byte-identical** to the ADR-0094 result.

## Seed cost-rate table — provenance (ILLUSTRATIVE, not production gospel)

The boot seed shipped by T4 is **zero-contribution** for every band (ADR-0097
Q6): the CRUD has rows to edit, but nothing moves until the operator enters
shop-measured values. The table below is the **illustrative** table used by the
validation golden, chosen for clean hand-arithmetic and to exercise each of the
five cost drivers; it is **not** auto-applied anywhere. Before quoting a real
tight part, recheck every value against the shop's measured in-process gauging /
CMM minutes, scrap rate, finishing-feed penalty, and grinder €/min.

| band            | finish_passes_add | inproc_min | cmm_min | rework_scrap_pct | feed_slowdown | grinding |
|-----------------|------------------:|-----------:|--------:|-----------------:|--------------:|:--------:|
| loose           | 0.0               | 0.0        | 0.0     | 0.00             | 1.0           | no       |
| standard        | 0.0               | 0.0        | 0.0     | 0.00             | 1.0           | no       |
| tight           | 0.0               | 0.0        | 0.0     | 0.10             | 1.0           | no       |
| precision       | 1.0               | 2.0        | 3.0     | 0.05             | 1.5           | no       |
| ultra_precision | 2.0               | 3.0        | 6.0     | 0.10             | 2.0           | **yes**  |

Routing rates (from the ADR-0094 planetary catalogue): the sun routes
Swiss-turn-mill lights-out (1.50 × 0.35 = **0.5250 €/min**); the ring routes
turn-mill lights-out (1.60 × 0.45 = **0.7200 €/min**); the `Grinder` family rate
is **3.0000 €/min** (attended). Tolerance terms are costed at the routed
effective €/min — the *same* rate the machining line used — and the grinding
adder at the grinder rate (ADR-0097 Part 2).

## Per-line decomposition (hand-derived, cross-checked to 4 dp)

### Sun — Ø-bore H6 (IT6 → Precision band), 1 critical feature, rate 0.5250

```
inspection = (2.0 in-proc + 3.0 CMM) min/feat × 1 feat = 5.0000 min × 0.5250 =   2.6250
finishing  = 1.0 passes × base_finish 2.1824 min × feed 1.5 = 3.2736 min × 0.5250 = 1.7186
grinding   = 0            (Precision is not the tightest band)
scrap      = 0.05 × (material 0.141637 + machining 1.286595) =                     0.0714
----------------------------------------------------------------------------------------
tolerance_cost                                                                  =  4.4151
```
Baseline total (ADR-0094 pin) **10.6314** → with tolerance **17.7838**
( = 10.6314 + 4.4151 × 1.62, the overhead×margin propagation factor ).

### Ring — UltraPrecision ground face (IT5 → tightest band), 1 critical feature, rate 0.7200

```
inspection = (3.0 in-proc + 6.0 CMM) min/feat × 1 feat = 9.0000 min × 0.7200 =    6.4800
finishing  = 2.0 passes × base_finish 22.4000 min × feed 2.0 = 89.6000 min × 0.7200 = 64.5120
grinding   = 12.0 min/feat × 1 feat × grinder 3.0000 €/min =                      36.0000
scrap      = 0.10 × (material 2.926394 + machining 26.850742) =                    2.9777
----------------------------------------------------------------------------------------
tolerance_cost                                                                  = 109.9697
```
Baseline total (ADR-0094 pin) **227.3354** → with tolerance **405.4863**
( = 227.3354 + 109.9697 × 1.62 ).

## The five structural wins (all asserted in the golden)

1. **Reconstructs from the log** — every `[tolerance] … = X EUR` sub-term is
   parsed back out and summed to the `total tolerance_cost` line and to
   `breakdown.tolerance_cost`.
2. **Inert proof** — Standard / no-callout = byte-identical to `PIN_SUN`
   (10.6314) / `PIN_RING` (227.3354); `tolerance_cost == 0.0`; no `[tolerance]`
   log line; inert even with the full seeded table when no callout is supplied.
3. **Only-when-supplied** — no callout, a not-tighter (IT11 → Standard) callout,
   and an empty rate table all price 0.0; only a genuinely tighter callout
   against a seeded row moves the number.
4. **Grinding only at the tightest band** — present on the ring (UltraPrecision);
   proven absent when the same flag sits on a Precision row.
5. **Totals** — finite, > 0, and above the no-tolerance baseline by exactly the
   itemised sum propagated through overhead and margin (× 1.62).

## Non-negotiables preserved

Pure engine (serde + thiserror only); same inputs ⇒ byte-identical breakdown +
reasoning_log; the additive line is `#[serde(default, skip_serializing_if)]` so
a no-tolerance-cost `breakdown_json` is byte-identical on the wire. Every
pre-existing golden / determinism / branch / property number is unchanged — this
session adds a test file and this note only; no engine source is touched.
