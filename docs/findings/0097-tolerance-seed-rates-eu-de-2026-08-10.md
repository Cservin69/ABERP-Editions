# ADR-0097 last mile — cost-rate seed: researched default EU/DE machine-shop values

**Date:** 2026-08-10 · **Scope:** `quoting_tolerance_cost_rates` seed +
the `Grinder` row in `quoting_machine_rates` · **Companion to:**
`docs/quote-tolerance-cost-driver-plan.md` (T4), ADR-0097 Q6/R4, and
`docs/findings/0097-t7-tolerance-validation-2026-06-29.md`.

## Why this change exists

T4 shipped an **all-zero** cost-rate seed (ADR-0097 Q6): the CRUD had rows to
edit, but tolerance-driven pricing produced exactly `0.00 EUR` for every band on
every quote until an operator hand-entered numbers nobody had researched. The
engine (T3), the taxonomy (T2), the CRUD + routes + SPA (T4/T5), the pipeline
precedence + audit stamp (T4) and the storefront token mapping (T6) were all
live. **The zero seed was the only thing standing between a wired feature and a
working one.**

These are **seed defaults, not measurements.** They are rows in an
operator-editable table, stamped in their `notes` column with:

> `SEED — default EU/DE machine-shop rates, NOT your shop's measured values. Tune to your shop.`

The seed is insert-if-absent per band, so an operator edit is never re-clobbered.

## The conservative fork: `loose` and `standard` stay at exactly zero

`standard` is the ISO 2768-**m** title-block default that nearly every
un-toleranced quote resolves to. Seeding it non-zero would silently raise
**every** quote, including in-flight ones — precisely risk **R4** (seed
inflation) that Q6 chose the all-zero seed to avoid.

So `loose`/`standard` keep the engine's exact no-op and the R4 guarantee holds
where it matters: **a part with no tolerance signal still prices
byte-identically.** Money moves if and only if a genuinely tighter class or a
critical-feature callout is supplied — which is the cost driver ADR-0097 exists
to price. The inert path is pinned by
`seeded_rates_price_a_tolerance_quote_at_a_sane_order_of_magnitude`.

## Machine-hour rates — cross-check, and the one addition

The six `quoting_machine_rates` seeds were **re-verified, not re-cut**:

| family | seed €/min | seed €/h | German published range |
|---|---:|---:|---|
| 3-axis mill | 1.6667 | **100** | 70–100 €/h [1] · 70–120 €/h [3] |
| 5-axis mill | 2.50 | **150** | 100–150 €/h [1] · 60–120 €/h [2] · 150–250 €/h [3] |
| lathe | 1.50 | **90** | 60–90 €/h [1] · 75–125 €/h turning [3] |
| 4-axis mill | 1.90 | 114 | (interpolated between 3- and 5-axis) |
| swiss-turn-mill | 1.50 | 90 | 100–180 €/h for turn-mill [3], attended base |
| turn-mill | 1.60 | 96 | as above |

All three directly-published families already sit inside their German bands, at
the upper end a DACH shop with certification and documentation overhead earns
[3]. Re-cutting them would move every seeded shop's prices for no accuracy gain
and would break ADR-0094's pin of 3-axis to the global
`machining_rate_eur_per_minute`. **Left unchanged.**

### `Grinder` — the missing row (new)

ADR-0097's tightest-band grinding escalation prices its adder at the `Grinder`
family rate and **falls back to the routed effective rate** when no such row
exists. No `Grinder` seed existed. For a lights-out Swiss part that fallback is
as low as 0.5250 €/min — badly under-costing a grind. Because the
`ultra_precision` seed below switches that escalation **on**, the row must exist
for the adder to be priced honestly.

Seeded at **2.50 €/min = 150 €/h**, held level with 5-axis. *No German trade
source publishes a grinding `Maschinenstundensatz`* — this is the one figure
here with no direct citation. Precision/coordinate grinding is described as
climate-controlled CNC work holding form-and-position accuracies below what
milling reaches [6], i.e. at least 5-axis-class overhead; pegging it to 5-axis
rather than guessing higher is the conservative read, and it is in any case far
above the fallback it replaces. **Flagged for Ervin to set from a real quote.**

No part ever *routes* to `Grinder` (capacity routing is unchanged), so the row is
inert for base pricing and touches only the escalation.

## Tolerance cost-rate seed — per-value provenance

| band | finish_passes_add | inproc_min | cmm_min | rework_scrap_pct | feed_slowdown | grinding |
|---|---:|---:|---:|---:|---:|:--:|
| loose | 0.0 | 0.0 | 0.0 | 0.00 | 1.0 | no |
| standard | 0.0 | 0.0 | 0.0 | 0.00 | 1.0 | no |
| tight | 0.0 | 0.5 | 1.0 | **0.02** | 1.0 | no |
| precision | 0.5 | 1.0 | 2.0 | **0.05** | 1.25 | no |
| ultra_precision | 1.0 | 2.0 | 4.0 | **0.12** | 1.5 | **yes** |

**Scrap / rework — the one directly-anchored column.** A published case relaxing
twenty ±0.01 mm dimensions to ±0.03 mm "reduced machining time by ~40% and
lowered scrap from 12% to 2%" [4]. Read straight off: ≈**2 %** at the `tight`
(±0.03-class) band and ≈**12 %** at `ultra_precision` (±0.01-class), with
`precision` interpolated at 5 %.

**Slower feeds + extra finishing passes.** Tight tolerances raise machining time
"by 30–200%" [4]; going from ±0.005″ to ±0.0005″ raises machining cost 30–50 %
through slower feeds and additional finishing passes [5]. Hence **half** an extra
whole-part pass at a 1.25 feed factor at `precision` and a full one at 1.5 at
`ultra_precision`.

`finish_passes_add` multiplies the **whole-part** geometry finishing minutes, so
it is the dominant term on any part with real surface area. A full 1.0 at
`precision` measured out at **~50 % of the machining line** on the test bracket —
the very top of the cited 30–50 % range, and stacked on top of the 1.9×
`quoting_tolerance_multipliers` row that already fires at that band (risk R1).
Half a pass — physically, re-finishing the toleranced regions rather than every
surface on the part — lands it mid-range at ~30 %. Both values were cut in half
from the first draft for exactly this reason.

**Inspection.** A simple part's CMM run is put at 15–30 minutes, complex
components "an hour or more" [7]; tight tolerances "double inspection effort"
[4]. The split to a *per-feature* minute figure is ours: ≈2 min/feature at
`precision` on a programmed repeat run, doubled to 4 at the tightest band, with
in-process gauging at half the CMM minutes.

**Sanity ceiling.** Independently, IT7 is reported at 3–5× the IT8–IT9 baseline
and IT5–IT6 at 8–15× [8]. Those are whole-part *price* multipliers spanning
material, machining and setup, so they are **not** directly comparable to this
additive line — they are recorded only as an upper sanity bound. Note the
existing `quoting_tolerance_multipliers` (1.4 / 1.9 / 2.8) already carries part of
that escalation; the ADR-0097 line is additive on top and measures a different
quantity (risk R1).

Every value is deliberately at the **low end** of its researched range: a seed
that under-states is corrected at the operator's first quote review, whereas one
that over-states silently loses work.

## Sources

1. [CNCRechner.de — Stundensatz CNC Fräsen Rechner](https://www.cncrechner.de/rechner/cnc-kosten/) — 3-axis 70–100 €/h, 5-axis 100–150 €/h, CNC lathe 60–90 €/h; rate covers depreciation, financing, facility, energy, maintenance, software, service. Accessed 2026-08-10.
2. [CNC Magazin — CNC Fräsen Preisliste 2025](https://cnc-and-more.blog/cnc-fraesen-preisliste-2025-kostenfaktoren-und-tipps/) — "Moderne 5-Achs-Zentren liegen oft zwischen 60–120 Euro pro Stunde." Published 2025-07-28.
3. [uneed — CNC Machining Cost: Pricing & Savings Guide 2026](https://www.uneedpm.com/cnc-machining-cost-pricing-savings-guide-2026/) — European job-shop rates: 3-axis milling €70–120/hr, turning €75–125/hr, 5-axis €150–250/hr; DACH shops at the upper end. Accessed 2026-08-10.
4. [Tirapid — Tight Tolerance Machining](https://tirapid.com/tight-tolerance-machining/) — "relaxing twenty ±0.01mm dimensions to ±0.03mm reduced machining time by ~40% and lowered scrap from 12% to 2%"; "Machining time rises by 30–200%"; "Inspection effort doubles". Published 2026-04-30.
5. [Metalworks Plus — Precision Machined Parts: Tolerance Standards & Cost](https://metalworksplus.com/news/precision-machined-parts-tolerance-standards-cost/) — ±0.005″ → ±0.0005″ raises machining cost 30–50 % via slower feeds, additional finishing passes, more frequent tool changes. Accessed 2026-08-10.
6. [oberflaechen-bearbeitung.de — Koordinatenschleifen](https://oberflaechen-bearbeitung.de/fertigungsverfahren/koordinatenschleifen/) — coordinate grinding described as climate-controlled CNC work at nanometre-range accuracy for demanding form-and-position tolerances; **no hourly rate published**. Accessed 2026-08-10.
7. [BOYI — CMM for Precision Part Inspection](https://www.boyiprototyping.com/cnc-machining-guide/cmm-for-precision-part-inspection/) — "Simple parts usually take around 15-30 minutes, while more complex components may require up to an hour or more." Published 2024-08-20.
8. [Alibaba Seller Blog — IT Tolerance Grades Explained](https://seller.alibaba.com/blogs/2026/southeast-asia/precision-machining/it-tolerance-grades-guide-alibaba-b2b) — "IT7 tolerance costs 3-5x more than IT8-IT9 baseline, while IT5-IT6 can cost 8-15x base pricing". No publication date shown.

## Caveats

* Published shop rates are **billing** rates; internal machine cost bases run
  20–40 % lower [3]. The `quoting_machine_rates` seeds are used by the engine as
  cost inputs that overhead and margin are then applied to, so a shop taking
  these literally will quote above market until it tunes them. **Tune to your
  shop** — this is what the `notes` stamp says on every row.
* The grinder rate (§ above) has no direct source.
* The `precision` scrap value (5 %) is an interpolation, not a published figure.
* None of these are Defense-specific; a defense/aerospace shop's documentation
  burden pushes inspection minutes materially higher.

## Status of the other two "last-mile" items

The backlog framed this last mile as *seed the rates + finish the auto-mapping +
finish the CAD-hint wiring*. Verified against the code as it stands:

**Auto-mapping — already done, nothing open.** `storefront_tolerance`
(`quote_pricing_pipeline.rs`) maps the customer's storefront token onto a
`PerJobTolerance`: `general` → ISO 2768-m ↔ Standard, `precision` → ISO 2768-f ↔
Tight, `per_drawing` → `PerDrawing` + manual review; `tolerance_critical` and a
non-empty `tolerance_note` each raise manual review; an absent/out-of-vocab token
is inert and never silently tightened. It is wired into the intake path, unit
tested, and stamped into the pricing audit with its provenance. No work was
needed.

**CAD hint — genuinely open, but far larger than a last mile.** The precedence
chain's `"extractor"` arm is live and unit-tested, but **no CAD producer can
populate it**: `python/aberp-cad-extract` pins `SCHEMA_VERSION = 2` with a
pydantic `FeatureGraph` that is `extra="forbid"` and carries no tolerance field,
`aberp-cad-extract-wrapper` pins `EXPECTED_SCHEMA_VERSION = 2` against it, and
the STEP extractor is OCCT bbox/volume/surface-area only — it never reads AP242
PMI/GD&T. Closing it requires semantic PMI extraction (only partially supported
in OCCT) plus a lockstep Python 2→3 and wrapper schema bump — ADR-0097 **R5**.
That is a project, not a wiring gap, and was deliberately not attempted here.
The seam is now documented at `stamp_tolerance` so the dormant arm does not read
as working. **Until it lands, the operator's per-job editor and the storefront
selection are the only tolerance signals — both of which now price real money.**
