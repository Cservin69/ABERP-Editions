# ADR-0094 — Quote-engine cost-model gap closures: turned/round stock, machine-family rates, gear-generation ops

- **Status:** Proposed (design pass; one decision flagged for Ervin's confirmation — see *Open questions* Q1)
- **Date:** 2026-06-25
- **Deciders:** Ervin
- **Grounds:** ADR-0066 (auto-quoting engine architecture; explicitly anticipates the per-machine-rate split — `crates/aberp-quote-engine/src/lib.rs:79-84`), the S418 geometry-model overhaul (`engine.rs` §5 / `catalogue.rs:109-161`), ADR-0093 (product-line saw-off; editions tree vs frozen prod; edition gating = compile-time `production` feature in `apps/aberp/src/build_profile.rs`), ADR-0056 (release-branch versioning), FOUNDATION.md §2/§5 (purity, path-derived-not-user-supplied), CLAUDE.md rules 2/3/7/9/12/13.
- **Scope guard:** This ADR is authored in the **ABERP-Editions** tree (`Cservin69/ABERP-Editions`, the Defense + Portable line). Frozen prod (`Cservin69/ABERP`, `PROD_v2.27.76`, tree `2d612811`) is **never** touched — per ADR-0093 and Ervin's permanent freeze (prod is invoicing-only, forever).

## Context

The auto-quoting engine (`crates/aberp-quote-engine`, the pure deterministic scorer from S268/S418) prices every part with one geometry model: a **rectangular** stock block and **one** flat machining rate. Three modelling gaps make it over-quote an entire class of real work — turned/round parts and geared assemblies — which is exactly the work the Defense edition's target customer (a CNC machine shop, post-EU-grant) will quote daily. The motivating case is a **Ø100 planetary gearset**: ring + 5 planets + sun + carrier + 5 pins + hub, qty 100.

The three gaps, grounded in the current code:

1. **Round/turned stock is billed as a square block.** `engine.rs:201-203` computes `bbox_volume = bx·by·bz` then `stock_volume = bbox_volume·(1 + scrap_factor)`. Material is billed on that block (`engine.rs:211-212`) and roughing removal is `(stock_volume − part_volume)` (`engine.rs:317-323`). A round bar of diameter `D` occupies `π/4·D²·L = 0.7854×` the bounding-box block, so a turned part is **over-billed ~21.5 %** on material and its roughing minutes are overstated by the same square-vs-round excess (a near-net bar barely needs roughing; the block model "removes" the four corners that were never bought).

2. **One flat €100/h rate for every machine.** `QuotingParameters.machining_rate_eur_per_minute` (default `1.6667`, `quoting_tunables.rs:383`) is a single global knob; `engine.rs:404-405` multiplies all billable minutes by it. Real shops route to families with very different effective €/part: a bar-fed Swiss / fixed-head twin-spindle turn-mill (sub-spindle pickoff, **lights-out / unattended**) prices a small turned part far below an attended €100/h; a 5-axis mill prices above it. The engine already has a `MachineFamily` enum (`capacity.rs:60-77`) and already keys the S429 calibration coefficient by family (`engine.rs:373`, `calibration.rs`), but **no rate is attached to the family** — and the enum lacks Swiss / turn-mill / 4-axis variants. `quoting_machines` rows (`quoting_machines.rs:54-70`) carry capacity and lead-time only, **no €/h field**. ADR-0066 and `lib.rs:79-84` already name this split as deferred future work.

3. **Gear teeth are not modelled at all.** `FeatureType` (`feature_graph.rs:20-44`) is {pocket, hole, slot, thread, undercut_5axis, thin_wall, surface, engraving} — no gear. A geared part is priced purely by its bounding-box envelope; the cost of generating teeth (module, count, face width, AGMA quality) is invisible. In the motivating case this was patched **outside this tree** with a manual €95 adder.

**Verified-absent finding (load-bearing for the validation plan).** The planetary fixture, the €444/box result, the €95 gear adder and the €1.93 pins **do not exist anywhere in the ABERP-Editions tree** (four independent exhaustive sweeps: no `planet|gear(box|set)|skiv|hob|broach|involute|spur|pinion`, no `€95`/`444`/`1.93` price constants; the only `gear` token is the SPA topbar icon). The €444 is therefore an **external/manual baseline** Ervin ran, not a re-runnable test in this repo. The validation target below is consequently a **new golden fixture** this work introduces, not a re-quote of an existing one.

**Non-negotiable engine invariants** that constrain every option (`lib.rs:15-31`): the scorer is **pure** (no I/O, clock, RNG, async, global state); same inputs ⇒ **byte-identical** `QuoteBreakdown` *and* `reasoning_log`; the reasoning log is the trust signal (`[[trust-code-not-operator]]`); the model is **catalogue-driven** (the wiring layer reads DB tables and hands the engine owned snapshots); and a **golden test** (`tests/golden.rs:18-75`) plus a determinism test and a never-panic property test (`tests/property.rs:39`) lock the numbers and the log to 4 dp.

## Decision

Close all three gaps **inside the shared, edition-agnostic `aberp-quote-engine` crate**, as **catalogue-driven, reasoning-logged, golden-guarded** extensions that are **byte-for-byte inert until their new inputs are supplied** (so every existing golden/determinism/property test stays green). Each gap is split into an **engine** change (pure math + tests) and a **wiring** change (`apps/aberp` DB tables, seeds, SPA, pipeline plumbing).

### Cross-cutting back-compat mechanism (applies to all three)

Three techniques keep the existing golden numbers and logs identical:

1. **`#[serde(default)]` on every new `FeatureGraph` / `QuoteBreakdown` field**, exactly as S418 did for `surface_area_mm2` (`feature_graph.rs:201`) and S429 did for `calibration_coefficient` (`breakdown.rs:75`). New geometry inputs default to "behaves like today"; new output fields default to `0.0` so persisted `breakdown_json` blobs still deserialize.
2. **A delegation chain for the engine entry points**, following the existing `quote → quote_with_calibration` precedent (`engine.rs:103-145`). The new catalogue slices enter through one new superset function; the older entry points delegate to it with **empty** slices, so callers and tests that use `quote(...)` are unchanged.
3. **Empty catalogue slice ⇒ today's behaviour.** No machine-rate rows ⇒ the global `machining_rate_eur_per_minute` is used (identical to today). No gear ops on the part ⇒ no gear cost. The default stock form is `RectangularBlock` ⇒ today's block math.

### Gap 1 — Stock-form material + roughing model

Introduce a `StockForm` to the geometry contract that carries its own dimensions, so the **pure** engine evaluates a volume formula rather than guessing a spin axis (the engine "does not second-guess" the extractor — `feature_graph.rs:170-173`):

```rust
// crates/aberp-quote-engine/src/feature_graph.rs
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StockForm {
    #[default]
    RectangularBlock,                                   // uses bounding_box_mm — today's math
    RoundBar { diameter_mm: f64, length_mm: f64 },      // π/4·d²·L
    Tube { od_mm: f64, id_mm: f64, length_mm: f64 },    // π/4·(od²−id²)·L
}
// new field on FeatureGraph, defaulted for back-compat:
//   #[serde(default)] pub stock_form: StockForm,
```

The stock-volume step (`engine.rs:201-203`) branches on `stock_form`:

- `RectangularBlock` → `bbox_volume = bx·by·bz` (unchanged → golden stays byte-identical).
- `RoundBar` → `form_volume = π/4 · diameter² · length`.
- `Tube` → `form_volume = π/4 · (od² − id²) · length` (the bore is *never bought*, so it is neither billed nor "roughed away" — the correct model for a ring-gear blank).

Then `stock_volume = form_volume · (1 + scrap_factor)` feeds **both** the material billing (`mass = stock_volume · density / 1e6`) **and** the roughing removal `(stock_volume − volume_mm3)` unchanged (`engine.rs:317-323`). One stock definition drives both, exactly as the S418 comment intends (`engine.rs:195-200`). Two new reasoning-log lines name the form and the formula; the `RectangularBlock` branch emits today's exact line.

**Source of the stock form (wiring layer, Gap 1 part B), precedence highest→lowest:** (a) an explicit per-quote/part field set by the operator or storefront; (b) a CAD-extract hint — the future S269 extractor classifies a rotationally-symmetric part and emits `stock_form` + `od/id/length`; (c) fallback `RectangularBlock`. Heuristics live in the extractor/wiring, never in the pure core.

### Gap 2 — Machine-family rate + lights-out factor + routing

**Extend the existing `MachineFamily` enum** (`capacity.rs:60-77`) with the missing families, reusing the one enum that already keys capacity *and* calibration:

- add `SwissTurnMill` (`"swiss-turn-mill"`) — bar-fed, sub-spindle, lights-out-capable,
- add `TurnMill` (`"turn-mill"`) — fixed-head twin-spindle,
- add `FourAxisMill` (`"4-axis-mill"`).

This is additive but touches a fixed set of sites (enumerated in the plan): the variants, `ALL` (length `8 → 11`), `as_db_str`, and the SPA/CRUD closed-vocab validator that reads `ALL`. `from_db_str` auto-covers them (it iterates `ALL`), and `CalibrationTable` auto-tolerates them (absent family ⇒ coefficient `1.0`, `calibration.rs:108-114`).

**Attach the rate as a new catalogue table, not to `quoting_machines`** (which stays capacity-only per ADR-0093's clean separation) and not as a map on the singleton `QuotingParameters` (which is flat scalars mirrored to DB columns). A new family-keyed catalogue type mirrors the existing `ComplexityRule` / `ToleranceMultiplier` / `StockAdjustment` snapshot pattern:

```rust
// crates/aberp-quote-engine/src/catalogue.rs
pub struct MachineRate {
    pub family: String,                  // MachineFamily::as_db_str round-trip
    pub attended_rate_eur_per_min: f64,  // the family's true €/min when an operator is dedicated
    pub lights_out_factor: f64,          // ∈ (0,1]; effective €/min = attended × factor when unattended-eligible
    pub unattended_capable: bool,        // bar-fed Swiss/turn-mill = true; manual mill = false
}
```

The machining-cost step (`engine.rs:402-405`) selects the routed family's rate from the `&[MachineRate]` snapshot; **absent ⇒ the global `machining_rate_eur_per_minute` (today's value)**. When the routed family is `unattended_capable` and the job qualifies (turned part on bar stock and `qty ≥ setup_amortization_threshold`), the **effective rate = attended_rate × lights_out_factor** — the physical cut-minutes are unchanged, but cost-per-minute drops because one operator tends several spindles overnight. The selection and the factor are reasoning-logged; the no-rows path logs today's exact line.

**Routing rule** (pure, generalising `MachineFamily::for_route`, `capacity.rs:118-124`) — a `route_family(stock_form, requires_5_axis, od_mm, params)` that decides: `requires_5_axis` ⇒ `FiveAxisMill`; `RoundBar`/`Tube` with `od ≤ bar_capacity_mm` ⇒ `SwissTurnMill` (lights-out); larger round ⇒ `Lathe` or `TurnMill`; otherwise prismatic ⇒ `ThreeAxisMill`. `bar_capacity_mm` is a new operator tunable on `QuotingParameters` (proposed default 32 mm). The operator can override the routed family per quote (wiring). This is the elegant link to Gap 1: **stock form drives the route**, and the route drives the rate *and* (Gap 3) whether skiving is in-cycle.

Proposed seed rates (to be calibrated, not gospel — the S429 closed loop already learns actual/estimated per family): Swiss turn-mill attended ≈ €1.50/min with `lights_out_factor ≈ 0.35`; turn-mill ≈ €1.60/min, `0.45`; 3-axis ≈ €1.6667/min (today's rate, `unattended_capable = false`); 4-axis ≈ €1.90/min; 5-axis ≈ €2.50/min; lathe ≈ €1.50/min.

### Gap 3 — Gear-generation op model

Model gear cutting as **costed operations**, since the volume model cannot see teeth. New optional input vector on the part, defaulted empty:

```rust
// crates/aberp-quote-engine/src/feature_graph.rs
pub enum GearKind { ExternalSpurHelical, InternalRing }
pub enum GearProcess { Hob, PowerSkive, Shape, Broach, WireEdm, Auto }
pub struct GearOp {
    pub kind: GearKind,
    pub module_mm: f64,      // m
    pub teeth: u32,          // z
    pub face_width_mm: f64,  // b
    pub quality_agma: u8,    // AGMA class (higher = tighter)
    pub process: GearProcess,
}
//   #[serde(default)] pub gears: Vec<GearOp>,   on FeatureGraph
```

Process coefficients live in a **catalogue** table (mirroring the others), so the per-process numbers are operator-tunable while the *math* stays in the pure engine:

```rust
pub struct GearProcessRate {
    pub process: String,              // GearProcess db-string
    pub setup_min: f64,               // indexing / tool-load per gear
    pub min_per_tooth: f64,           // base time per generated tooth
    pub module_exponent: f64,         // time scales with module^exp (bigger teeth = slower)
    pub agma_quality_factor_base: f64,// multiplier growth per AGMA class above a datum
    pub in_cycle_factor: f64,         // <1 when run in-cycle (power-skiving on a turn-mill)
}
```

Per gear, the engine computes
`gear_min = setup_min + z · min_per_tooth · (module_mm^module_exponent) · facewidth_factor(b) · quality_factor(agma)`,
applies `in_cycle_factor` when the process is in-cycle (power-skiving on a routed `SwissTurnMill`/`TurnMill` — the part is already on the spindle, no second op, no refixture ⇒ cheap), costs it at the routed family's effective rate, and **sums into a new `gear_cost` breakdown field** (`#[serde(default)] f64`, folds into subtotal). External teeth → hob or skive; internal ring teeth → shape / broach / wire-EDM (expensive). `Auto` selects deterministically by `kind` + routed family + quality (external + turn-mill ⇒ skive; external + mill ⇒ hob; internal ⇒ shape, escalating to wire-EDM above an AGMA threshold) — every choice reasoning-logged. AGMA-band thresholds and `facewidth_factor`/`quality_factor` shapes are pinned engine constants (golden-guarded), like `THIN_WALL_TIGHT_TOL_BUMP`.

### Shared engine vs Defense-gated — **Decision: ship shared (in the editions tree), not feature-gated**

Interpret "ABERP-Defense only" as a **TREE boundary, not a Cargo feature-gate**: the work lands in the ABERP-Editions tree (where Defense lives) and **never** in frozen prod — which is the whole of Ervin's constraint and is satisfied by construction (ADR-0093). Within the editions tree, ship the three closures in the **shared** `aberp-quote-engine`, which both arms link identically. Rationale, with Ervin's steer (Defense is the real product; Portable is only a demo):

- The engine crate is **deliberately edition-agnostic**: it has **no `[features]` section**, both arms depend on the same path crate, and *all* edition divergence lives in `apps/aberp` (`build_profile.rs`, data roots, NAV). Injecting a `#[cfg(feature = "production")]` into the pure scorer would violate the crate's stated purity/independence contract and ADR-0093's clean split — **more** complexity for **no** gain, because Portable inheriting better quoting is harmless (it is a demo).
- Defense (the real install) gets the full benefit; Portable inherits it for free. There is no business reason to *withhold* better cost modelling from the demo.
- If a premium/differentiated *framing* is ever wanted, the cheapest lever is the **catalogue/seed + UI layer**, not the engine: seed the Swiss-family rates and the gear-process table only under Defense's data root, or expose the gear/turning UI only when `EDITION == Edition::Defense` (`build_profile.rs:127-147`). The engine stays universal; the *data and onboarding* can be edition-flavoured later without touching the pure core.

This is **flagged for Ervin's confirmation** (Open question Q1) per the brief — the default is shared, and nothing here forecloses a later UI/seed-level Defense framing.

## Consequences

- **Turned and geared work is finally defensible.** A round/tube blank bills ~21 % less material with proportionally less roughing; a bar-fed Swiss running lights-out prices small turned parts (the pins) far below attended €100/h; gear teeth carry an explicit, itemised cost instead of a hidden manual adder. Every contribution is a line in the reasoning log — the operator can read exactly why.
- **Existing prices do not move.** The default stock form, empty rate table, and empty gear vector reproduce today's math byte-for-byte; `tests/golden.rs`, `tests/determinism.rs`, `tests/branches.rs`, `tests/property.rs` stay green. New numbers appear only when the new inputs are supplied.
- **One enum, three uses.** `MachineFamily` now keys capacity, calibration **and** rate — coherent, and the S429 closed loop already corrects each family's estimate against actuals, so seed-rate error self-heals over time.
- **New maintenance surface.** Three new catalogue tables (machine rates, gear processes) + one new `FeatureGraph`/`QuoteBreakdown` field set + SPA CRUD. Mitigated by following the established `quoting_*` table pattern verbatim.
- **Lock-in.** The `StockForm`/`GearOp` shapes become a wire contract with the S269 extractor; `SCHEMA_VERSION` bumps (2 → 3 for stock-form, → 4 for gears) and the Python extractor + Rust wrapper `EXPECTED_SCHEMA_VERSION` must move in lockstep when S269 lands (Open question Q3).
- **Engine entry-point arg count grows** via the delegation chain. Accepted now (follows the repo's own `quote → quote_with_calibration` precedent); flagged to collapse the catalogue slices into a single `CatalogueSnapshot` struct once the arg count crosses ~11 (Open question Q2) — a dedicated refactor, not a speculative bundling mid-feature (CLAUDE.md #2/#13).
- **Prod stays frozen** — all of this is editions-tree only; prod's tree-hash `2d612811` is re-proved untouched after the work.

## Adversarial review

- *"A hostile auditor: your `lights_out_factor` lets the engine quote below cost — a money-losing race to the bottom."* The min-margin floor (`engine.rs:598-610`, `QuoteError::MarginFloorViolation`) is unchanged and still gates every quote; the lights-out factor lowers the *cost basis* (real: unattended spindle-hours are genuinely cheaper per part), not the margin. The factor is an operator tunable per family with a default ≤ 1.0, reasoning-logged, and bounded; and S429 calibration corrects it against recorded actuals. It cannot silently produce a sub-floor quote.
- *"You're encoding shop-floor judgement (routing, Auto gear-process, bar-capacity) as code — that's exactly CLAUDE.md rule 5's 'don't use the model for deterministic transforms' inverted into hidden heuristics."* The routing and process-selection rules are **deterministic, pure, reasoning-logged, and operator-overridable** — the opposite of hidden. Every decision prints a log line; the operator can override the routed family and force a gear process per quote. Thresholds are pinned constants the golden test guards (rule 9), so they cannot drift silently.
- *"The golden test can pass while the new code returns garbage — all your defaults make the new paths inert, so nothing tests them."* Each session adds **new** golden fixtures that exercise the non-default paths (round-bar blank, tube ring, Swiss lights-out rate, skived external gear, shaped internal ring) with hand-derived 4-dp expected values, plus the property sweep is extended so the new branches stay finite and non-negative across varied inputs (`tests/property.rs`). The inert-by-default property protects *old* prices; the new fixtures protect *new* math.
- *"You can't even compile this in your sandbox — how is any of it verified?"* Honest deferral, matching the SAW-OFF posture. The local gate is rustfmt + `rustc --test` extraction of the pure engine modules (no DuckDB, no Tauri) + the toolchain-free `tools/cut_gate_db_isolation.sh`; the **real** build/test proof is the editions CI (`.github/workflows/ci.yml`), which builds and tests **both** edition arms (`portable` and `defense --features production`) on every pushed branch.
- *"The €444 baseline you're beating doesn't exist in the tree — you could 'validate' against a number you invented."* Acknowledged explicitly: the €444 is an external manual baseline, and the validation is a **new** golden fixture built from first principles. The claim is therefore framed as *direction + decomposition* (the per-box total is materially below €444 **and** every line — bar-stock material, Swiss lights-out machining, skived externals, shaped ring — is present and individually inspectable in the reasoning log), not a spurious exact-match to a number with no provenance in this repo. Q4 asks Ervin to confirm the baseline's inputs.

## Alternatives considered

- **Feature-gate the closures to the Defense build (`#[cfg(feature = "production")]`).** Rejected: pollutes the deliberately edition-agnostic pure crate, contradicts ADR-0093's split, and withholds value from the demo for no benefit. "Simpler for marketing" is not an engineering reason (ADR README: "'Simpler' is not a reason on its own").
- **Attach rates to `quoting_machines` rows (operator enters each machine's €/h).** Rejected for v1: the engine routes to a *family*, not a specific machine; `quoting_machines` is documented capacity-only and can be empty (virtual-shop fallback). A family-keyed catalogue is the right granularity and matches the existing snapshot pattern. (A per-machine rate is a clean future extension once the extractor can pick a specific machine.)
- **A `BTreeMap<MachineFamily, f64>` of rates on `QuotingParameters`.** Rejected: `QuotingParameters` is a flat singleton row mirrored to DB columns (`quoting_tunables.rs:313-330`); a map breaks that 1:1 column mapping. A sibling catalogue table is cleaner and audit-friendly.
- **Model gears through the generic volume/feature model.** Rejected: teeth are not a bounding-box or a `FeatureType`; generating them is a distinct process whose cost is dominated by module/count/quality/process, none of which the volume model sees. A dedicated op model is the only honest representation.
- **Derive the cylinder axis inside the engine from the bounding box.** Rejected: axis inference is a heuristic and belongs in the extractor/wiring, not the pure core (consistent with `representative_size_mm` — the extractor picks dimensions, the engine evaluates). `StockForm` carries explicit dims instead.
- **One big "fix everything" session.** Rejected: violates the sequential single-focus + clean-git-between discipline; the plan sequences six build sessions + one validation, each its own branch and CI proof.

## Open questions

- **Q1 (flagged for confirmation):** Confirm **shared engine** (both arms) over a Defense feature-gate. Default taken: shared, per Ervin's "bias to shared" and the edition-agnostic crate architecture. Any future Defense-only *framing* lands at the seed/UI layer, not the engine. → resolved by Ervin's nod, no further ADR needed; a feature-gate would need a superseding ADR.
- **Q2:** When the engine entry-point arg count crosses ~11, collapse the catalogue slices into a `CatalogueSnapshot` struct (dedicated refactor session). Tracked, not done speculatively.
- **Q3:** `FeatureGraph::SCHEMA_VERSION` bump (→3, →4) must move in lockstep with the S269 Python extractor + S270 wrapper `EXPECTED_SCHEMA_VERSION` when they land. Until then the fields are wiring/operator-populated and default-inert. Owning ADR: ADR-0066 follow-up / S269.
- **Q4:** Confirm the external €444/box baseline's inputs (material grade, tolerances, quantities, which parts were geared) so the new planetary golden fixture is anchored to the same scenario rather than an approximation.
- **Q5:** Whether gear-op estimates should later feed their own S429 calibration family (actual vs estimated gear minutes). Deferred; the existing per-family loop covers the machining portion today.
