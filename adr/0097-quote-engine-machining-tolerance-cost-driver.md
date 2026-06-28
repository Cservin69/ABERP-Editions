# ADR-0097 — Quote-engine machining-tolerance cost driver: professional taxonomy, per-feature callouts, and a dedicated tolerance cost line

- **Status:** Proposed (design pass; conservative calls flagged for Ervin's confirmation/veto — see *Open questions* Q1–Q7)
- **Date:** 2026-06-28
- **Deciders:** Ervin
- **Grounds:** ADR-0094 (quote-engine cost-model gap closures — the inert-by-default + catalogue-driven + reasoning-logged pattern this ADR mirrors verbatim; its Q2 `CatalogueSnapshot` deferral is resolved here, and its Q3 `SCHEMA_VERSION` lockstep is extended), ADR-0066 (auto-quoting engine architecture; the pure-scorer contract), the S418 geometry model (`engine.rs` §5; `catalogue.rs` `QuotingParameters`), S429 calibration (`calibration.rs`; per-family closed loop), ADR-0093 (product-line saw-off; editions tree vs frozen prod; edition = compile-time `production` feature in `apps/aberp/src/build_profile.rs`), FOUNDATION.md §2/§3 (modular monolith, pure domain), README/ADR house rules (catalogue-driven, "'Simpler' is not a reason on its own").
- **Scope guard:** Authored in the **ABERP-Editions** tree (`Cservin69/ABERP-Editions`, the Defense + Portable line; bundle head `cc722fac` = `PROD_Defense_v0.2.2`). Frozen prod (`Cservin69/ABERP`, `PROD_v2.27.76`, tree `2d612811`) is **never** touched — per ADR-0093 and Ervin's permanent freeze (prod is invoicing-only, forever). This ADR is a **design pass only**: no engine/wiring code is changed in this session; the sequenced build is ADR-0097's companion plan (`docs/quote-tolerance-cost-driver-plan.md`).

## Context

A machine shop's customer does not buy a "Tight part" — they buy a part with a **drawing**, and that drawing states tolerance in one of four professional dialects: a **general tolerance class** (ISO 2768 fine/medium/coarse/very-coarse, the title-block default), an **ISO 286 IT grade** on a fit (IT6–IT14), an **explicit ± value** on a critical dimension, or **"per drawing"** (GD&T callouts the operator must read). Tolerance is one of the largest single levers on the true cost of a machined part: a Ø20 H7 bore is not 40 % dearer than a Ø20 ±0.1 bore, it can be several times dearer once spring passes, in-process gauging, a CMM report, scrap on the rejects, and — at the tightest bands — a second setup on a grinder are counted. The Defense edition's target customer (a CNC shop, post-EU-grant) lives and dies on quoting this correctly.

**What exists today (verified-present).** Tolerance is *already* a first-class engine input, but a coarse one:

1. A 5-band ordered enum `ToleranceRange` — `Loose < Standard < Tight < Precision < UltraPrecision` (`feature_graph.rs`, derives `PartialOrd`), passed as the top-level `target_tolerance: ToleranceRange` argument to every engine entry point (`engine.rs:136/168/214`).
2. A catalogue table `quoting_tolerance_multipliers` (S267) surfaced to the engine as `ToleranceMultiplier { tolerance_range, multiplier, inspection_minutes_per_feature }` (`catalogue.rs`). DDL at `quoting_tunables.rs:467-471` (PK `tolerance_range`, `multiplier DOUBLE DEFAULT 1.0`, `inspection_minutes_per_feature DOUBLE DEFAULT 0.0`). Seeded at boot (`quoting_tunables.rs:629-635`): `Loose 0.9/0.0`, `Standard 1.0/0.0`, `Tight 1.4/0.5`, `Precision 1.9/1.5`, `UltraPrecision 2.8/3.0`.
3. The engine applies it in three places: inspection minutes `inspection_minutes = inspection_minutes_per_feature × feature_row_count` (`engine.rs:541`); a **single flat multiplier folded into `machining_cost`** — `machining_cost = (machining_minutes + inspection_minutes) × machining_rate × tolerance.multiplier` (`engine.rs:585-586`); and a thin-wall coupling `THIN_WALL_TIGHT_TOL_BUMP = 1.15` applied when `thin_wall_present && target_tolerance >= Tight` (`engine.rs:29, 598-600`). Missing row ⇒ `QuoteError::ToleranceNotInTable` (`error.rs:21-22`).
4. Operator CRUD exists: `QuotingToleranceMultipliersList.svelte` — edit-in-place over the fixed 5-row closed-vocab table (no create/delete).

**The gaps (verified-absent — four independent sweeps across `*.rs`/`*.ts`/`*.svelte`/`*.py`, excluding `node_modules`/`.git`):**

- **No professional taxonomy.** `iso.?2768` → **0 hits**; `per.?drawing` → **0 hits**; no structured IT-grade or ± input anywhere (the only `it_grade`/`IT\d` hits are incidental — "grade not in catalogue", a material grade `IT`-substring). The customer/operator can pick exactly one of five qualitative words; there is no way to say "ISO 2768-m general, Ø12 H7 on the bore."
- **No per-feature / critical-feature tolerance.** `critical.?feature` / `per.?feature.?tol` → **0 hits**. `Feature` (`feature_graph.rs:163`) is `{feature_type, count, representative_size_mm}` — no tolerance field. Tolerance is a single whole-part band; a part that is rough everywhere except one ground journal is over- or under-quoted because the band is global.
- **No dedicated tolerance cost line.** `tolerance_cost` / `ToleranceCost` → **0 hits**. The cost is invisible: it is buried inside `machining_cost` as a bare `× multiplier` and a few inspection minutes. The operator cannot read *why* a tight part costs what it does — which violates the spirit of the reasoning-log trust signal as much as the pre-ADR-0094 hidden gear adder did.
- **No richer cost model.** The flat multiplier cannot represent the five real, separable cost drivers: extra **finishing passes**, **in-process + CMM inspection** time, **scrap/rework** rate, **slower finishing feeds**, and the tightest-band **process escalation** to grinding/special process. (A `MachineFamily::Grinder` *already exists* — `capacity.rs:61` — so escalation has a routing target the moment Gap-2's `MachineRate` table carries a grinder rate.)
- **The customer cannot specify tolerance at all.** The storefront `/quote` form (separate SvelteKit app on Lightsail, `abenerp.com`; `docs/walkthroughs/quote-workflow.md`) collects CAD + specs but **not** tolerance — confirmed in code: `quote_pricing_pipeline.rs:226-230` ("the storefront submission form does not yet collect a tolerance band, so the daemon …"). Tolerance is a **daemon-config default** (`default_tolerance: ToleranceRange::Standard`, `quote_pricing_pipeline.rs:912/1372`), is **not stored per-job**, and re-pricing **hardcodes `Standard`** (`quote_pricing_pipeline.rs:3039-3040,3152`). There is no precedence chain — unlike `StockForm`, which already resolves operator > extractor > default (ADR-0094 Gap 1B).

**Non-negotiable engine invariants** (lib.rs:15-31) constrain every option: the scorer is **pure** (no I/O, clock, RNG, async, global state); same inputs ⇒ **byte-identical** `QuoteBreakdown` *and* `reasoning_log`; the reasoning log is the trust signal (`[[trust-code-not-operator]]`); the model is **catalogue-driven** (wiring reads DB tables, hands the engine owned snapshots); and the golden + determinism + property tests lock the numbers and the log to 4 dp.

## Decision

Upgrade tolerance from a single coarse multiplier into a **professional, catalogue-driven, reasoning-logged cost driver** — a taxonomy that maps every drawing dialect onto one internal tightness scale, an optional per-critical-feature layer, and a **new additive `tolerance_cost` breakdown line** — built **inside the shared, edition-agnostic `aberp-quote-engine` crate**, as extensions that are **byte-for-byte inert until their new inputs are supplied**. The existing `ToleranceMultiplier` flat-multiplier path is **kept unchanged** as the overall-class baseline; the professional model is **purely additive on top of it**, exactly as ADR-0094's `gear_cost` was additive over the existing lines. Each part is split into an **engine** change (pure math + tests, locally verifiable) and a **wiring** change (`apps/aberp` DB tables/seeds/SPA/pipeline + storefront contract, CI-gated).

### Cross-cutting back-compat mechanism (the inert-by-default proof)

Three techniques — identical to ADR-0094's — keep every existing golden number and log line byte-identical:

1. **`#[serde(default)]` on every new field.** `FeatureGraph.tolerance: ToleranceSpec` defaults to `ToleranceSpec::Unspecified`; `Feature.tolerance: Option<FeatureTolerance>` defaults to `None`; the new `QuoteBreakdown.tolerance_cost: f64` defaults to `0.0` with `skip_serializing_if = "is_zero_eur"` so a no-tolerance-cost `breakdown_json` is byte-identical on the wire (verbatim the `gear_cost` precedent — `breakdown.rs`). A persisted v1–v4 graph deserialises unchanged.
2. **A delegation chain (no new positional arg).** The new catalogue slice does **not** become a 12th argument. Instead — resolving **ADR-0094 Q2** — the catalogue slices collapse into a single `CatalogueSnapshot` struct, and a new superset entry `quote_with_catalogue(&FeatureGraph, &CatalogueSnapshot, &QuotingParameters, qty, target_tolerance, &CalibrationTable)` is added; the existing `quote` / `quote_with_calibration` / `quote_with_shop_model` delegate to it with a snapshot whose `tolerance_cost_rates` slice is **empty** (`&[]`). Callers and tests that use the old entry points are unchanged.
3. **Empty slice / default spec ⇒ today's behaviour.** `ToleranceSpec::Unspecified` + all-`None` per-feature + empty `&[ToleranceCostRate]` ⇒ the new `tolerance_cost` path is never entered ⇒ `tolerance_cost = 0.0`, **no** new reasoning-log line, and the subtotal line is today's exact bytes. The existing `target_tolerance` multiplier + inspection adder continue to drive `machining_cost` precisely as now. **The golden (`tolerance = Standard`) stays byte-identical** by construction.

> **Inert-by-default is achievable and provable.** The default-class path (`Standard`/`Loose`) and the all-absent path produce `tolerance_cost = 0.0`; `material_cost`, `machining_cost`, `inspection_minutes`, `setup_cost`, `subtotal`, `overhead`, `margin`, `total_price` are unchanged to the last bit; `tests/golden.rs`, `tests/determinism.rs`, `tests/branches.rs`, `tests/property.rs` stay green. New numbers appear **only** when a tighter spec, a per-feature callout, or a seeded `quoting_tolerance_cost_rates` row is supplied.

### Part 1 — Tolerance taxonomy → one internal tightness scale

Support the four real dialects as a closed serde-tagged enum on the geometry contract (the drawing/extractor side), defaulted inert:

```rust
// crates/aberp-quote-engine/src/feature_graph.rs
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToleranceSpec {
    #[default]
    Unspecified,                              // → engine uses the resolved target_tolerance arg (today)
    GeneralClass { class: GeneralClass },     // ISO 2768 title-block default
    ItGrade { grade: u8 },                    // ISO 286 IT6..IT14 on a fit
    PlusMinus { value_mm: f64 },              // explicit symmetric ± on a nominal
    PerDrawing,                               // GD&T-only → manual-review flag, no silent assumption
}
pub enum GeneralClass { Iso2768Fine, Iso2768Medium, Iso2768Coarse, Iso2768VeryCoarse }
```

A **pure, deterministic, reasoning-logged** normaliser maps every dialect onto the existing 5-band `ToleranceRange` (the internal cost-relevant tightness scale — reused, not replaced, for v1; see Q3):

`tightness(spec, nominal_mm) -> ToleranceRange`

- **General class:** `2768-fine → Tight`, `2768-medium → Standard`, `2768-coarse → Loose`, `2768-very-coarse → Loose`. (Medium is the universal title-block default ⇒ `Standard` ⇒ byte-identical to today — Q1.)
- **IT grade (size-aware-correct):** the band edges are the engineering judgment in Q4. Proposed: `≤ IT5 → UltraPrecision`, `IT6–IT7 → Precision`, `IT8–IT9 → Tight`, `IT10–IT11 → Standard`, `IT12–IT14 → Loose`.
- **Explicit ±:** derive the ISO 286 IT grade from the ± width **and the nominal size** (per-feature, the `nominal_mm` is the feature's `representative_size_mm`; for a whole-part ± with no feature, fall back to a magnitude table and flag the approximation), then map the grade → band as above. This is the professionally-correct path: a ±0.01 means IT6 on a 10 mm bore but IT9 on a 250 mm one.
- **Per drawing:** resolve to the part's overall class (default `Standard`) **and** raise a `tolerance_manual_review` flag — never silently price tight or loose (Q5).

The normaliser lives in the pure engine; the *heuristics that pick which dialect a drawing used* live in the extractor/wiring, never in the core (consistent with `representative_size_mm` — the extractor picks, the engine evaluates).

### Part 2 — Cost model: catalogue-driven `ToleranceCostRate`, additive `tolerance_cost`

The five real cost drivers become a per-band (optionally per-process) catalogue row, mirroring `ToleranceMultiplier`/`MachineRate`/`GearProcessRate`:

```rust
// crates/aberp-quote-engine/src/catalogue.rs
pub struct ToleranceCostRate {
    pub tolerance_class: String,            // ToleranceRange::as_db_str round-trip (band key)
    pub finish_passes_add: f64,             // extra finishing passes contributed at this band
    pub inproc_inspection_min: f64,         // in-process gauging min per critical feature
    pub cmm_min_per_critical_feature: f64,  // final/CMM report min per critical feature
    pub rework_scrap_pct: f64,              // fractional uplift on (material+machining) for expected scrap/rework
    pub feed_slowdown_factor: f64,          // >=1.0; multiplies the finishing-pass minute contribution (slower feeds)
    pub grinding_escalation: bool,          // tightest band: route critical feature to a grinder adder
}
```

Per part the engine computes an additive line, costed at the **routed family's effective €/min** (reusing ADR-0094 Gap-2's `MachineRate`, so a tight turned part still benefits from the lights-out rate; grinding uses the `Grinder` family rate):

```
tolerance_cost =
    Σ_critical_features ( inproc_inspection_min + cmm_min_per_critical_feature ) × effective_rate
  + ( finish_passes_add × base_finish_min × feed_slowdown_factor ) × effective_rate
  + grinding_min × grinder_rate                    // only when grinding_escalation && band is tightest
  + rework_scrap_pct × ( material_cost + machining_cost )
```

It **sums into a new `tolerance_cost` breakdown field** (`#[serde(default, skip_serializing_if)]`, folded into the subtotal beside `material/machining/setup/cad_cam/gear`). Every term is a reasoning-log line; the no-rows path adds no line and contributes `0.0`. **Inert-by-default is exact:** an empty `&[ToleranceCostRate]` slice ⇒ `tolerance_cost = 0.0`. The boot **seed is zero-contribution rows for every band** (so the CRUD has rows to edit but nothing moves until the operator tunes them — Q6's conservative posture; recommended illustrative seed values are listed in the plan, not auto-applied). The existing `ToleranceMultiplier` flat multiplier and the `THIN_WALL_TIGHT_TOL_BUMP` are untouched — the new line is strictly additional, so no existing line moves.

### Part 3 — Engine integration

- **`FeatureGraph.tolerance: ToleranceSpec`** (overall, `#[serde(default)] = Unspecified`) and **`Feature.tolerance: Option<FeatureTolerance>`** (per-critical-feature, `default None`), where `FeatureTolerance { spec: ToleranceSpec }` carries that feature's tighter callout. This is the "`Tolerance` (and optional per-feature) field on `FeatureGraph`" the brief asks for, expressed as the drawing-side spec; the **resolved overall band** still flows through the existing `target_tolerance: ToleranceRange` argument (the wiring computes precedence and passes it), so the existing multiplier path is unchanged.
- **`ToleranceCostRate` catalogue type** (above) + **`tolerance_cost` breakdown line** (above), mirroring StockForm/MachineRate/GearOp.
- **`CatalogueSnapshot` + `quote_with_catalogue`** (resolves ADR-0094 Q2). The `ToleranceCostRate` slice is the input that crosses the 11-arg threshold (`quote_with_shop_model` is exactly 11 positional args today — `engine.rs:206-218`), so rather than add a 12th, the slices are bundled now. This is a dedicated refactor session (T1), not speculative mid-feature bundling.
- **`SCHEMA_VERSION` 4 → 5** (`FeatureGraph::SCHEMA_VERSION`) for the new `tolerance`/per-feature fields; a v2–v4 graph (no `tolerance`) still loads (defaults to `Unspecified` ⇒ today's price), and the version guard accepts `schema_version ≤ 5`. The Python extractor `SCHEMA_VERSION` + the wrapper `EXPECTED_SCHEMA_VERSION` move in lockstep when S269 lands — extends **ADR-0094 Q3** (Q8 here).

### Part 4 — Intake UX (precedence: operator override > extractor/drawing hint > default)

Mirroring `StockForm`'s precedence (ADR-0094 Gap 1B), the wiring layer resolves the final overall band and the per-feature callouts **before** the engine call; the pure core only evaluates:

- **Customer storefront `/quote` form** (separate Lightsail repo — out of this tree; a coordinated cross-repo session, T6). Conservative scope (Q6): a single guided **general-tolerance dropdown** in plain language ("Standard machining (ISO 2768-m)" / "Fine (ISO 2768-f)" / "Coarse" / "Precision — tight fits") plus an optional free-text **"tighter tolerance on any critical features?"** note that routes to operator review (raises `tolerance_manual_review`). The intake JSON contract gains the tolerance fields; ABERP's intake stores them per-job. Customers rarely know IT grades — over-asking loses quotes; the operator refines.
- **Operator SPA** (`apps/aberp-ui`): the full professional surface. A new `QuotingToleranceCostRatesList.svelte` catalogue CRUD (mirror `QuotingToleranceMultipliersList.svelte`) under Maintenance → Quoting; and a **per-job tolerance editor** in `PricingJobDetail.svelte` (mirror the existing stock-form editor) — overall class **and** per-critical-feature callouts as class / IT-grade / ± dropdowns, with an operator override that beats both the extractor hint and the default.
- **Persistence.** New `quote_pricing_jobs` columns via `ALTER TABLE … ADD COLUMN IF NOT EXISTS` (the established pattern — `quote_pricing_jobs.rs:326-343` did this for `stock_form`/`gear_ops_json`): `tolerance_class VARCHAR`, `tolerance_spec_json VARCHAR` (overall spec + per-feature callouts), `tolerance_manual_review BOOLEAN`; plus `set_tolerance(...)` mirroring `set_stock_form()` (`quote_pricing_jobs.rs:1510`). This **fixes** the two standing defects: tolerance is now **stored per-job**, and **re-pricing reads the stored tolerance** instead of hardcoding `Standard` (`quote_pricing_pipeline.rs:3039-3040`).

### Shared engine vs Defense-gated — **Decision: ship shared (in the editions tree), not feature-gated**

Identical to ADR-0094's resolved Q1: "ABERP-Defense only" is a **TREE boundary, not a Cargo feature-gate**. The work lands in `aberp-quote-engine` (no `[features]` section; both arms link it identically); all edition divergence stays in `apps/aberp`. Injecting `#[cfg(feature = "production")]` into the pure scorer would violate the crate's purity/independence contract and ADR-0093's split for no gain — Portable inheriting better tolerance pricing is harmless (it is a demo). Any premium *framing* later is a seed/UI-layer lever (e.g. expose the per-feature tolerance editor only when `EDITION == Edition::Defense`), never an engine fork. Consistent with the resolved precedent; no separate veto needed.

## Consequences

- **Tolerance is finally defensible and readable.** A part's tolerance cost is an itemised line — in-process gauging, CMM minutes, scrap uplift, slower-feed finishing, and (at the tightest band) a grinding op — each a reasoning-log entry the operator can read, instead of a hidden `× 1.9`. A part that is rough except one ground bore is priced on *that* feature, not on a whole-part band.
- **Every drawing dialect maps in.** ISO 2768, IT grades, explicit ±, and "per drawing" all normalise onto one tightness scale deterministically; "per drawing" raises a manual-review flag rather than guessing.
- **Two standing defects close as a side effect.** Tolerance becomes **per-job persisted**; re-pricing stops hardcoding `Standard`; the storefront finally collects it.
- **Existing prices do not move.** Default class + empty cost-rate table + no per-feature callouts reproduce today's math byte-for-byte (the seed is zero-contribution). The new line appears only when inputs are supplied.
- **One scale, reused.** `ToleranceRange` now backs both the legacy flat multiplier and the new additive model; the S429 loop already corrects the machining estimate per family, so seed-rate error in the additive line self-heals once a calibration family is (optionally) added for it (Q deferred).
- **New maintenance surface.** One new catalogue table (`quoting_tolerance_cost_rates`) + its CRUD, three new per-job columns + `set_tolerance`, one `FeatureGraph`/`QuoteBreakdown` field set, a storefront contract field. Mitigated by following the `quoting_*` / `set_stock_form` patterns verbatim.
- **Lock-in.** `ToleranceSpec` + per-feature `FeatureTolerance` become a wire contract with the S269 extractor; `SCHEMA_VERSION` 4 → 5; the Python extractor + wrapper `EXPECTED_SCHEMA_VERSION` must move in lockstep when S269 lands (Q8).
- **`CatalogueSnapshot` arrives.** ADR-0094 Q2 is resolved: the engine entry collapses 11 slices into one struct. Net simplification at the call sites; one focused refactor session (T1) ahead of the feature.
- **Prod stays frozen** — editions-tree only; prod tree-hash `2d612811` re-proved untouched after the work.

## Adversarial review

- *"A hostile auditor: you're now double-counting tolerance — the old `× multiplier` in `machining_cost` AND a new `tolerance_cost` line for the same tightness."* They measure different things and are individually inspectable. The legacy multiplier scales the **base machining** of the whole part by a coarse band factor (it stays as the overall-class baseline); the new line adds the **separable, itemised** costs the flat factor cannot see — per-critical-feature CMM/in-process gauging, scrap/rework, slower-feed finishing passes, grinding escalation. Every term is a distinct reasoning-log line, so an auditor reads exactly what each contributes and can see they are not the same quantity. If Ervin prefers a single mechanism, Q2 offers the alternative (fold into the multiplier) with its golden-re-pin cost named.
- *"Your IT-grade and ±→band mappings are shop judgement encoded as code — the hidden-heuristic trap."* The mappings are **deterministic, pure, reasoning-logged, operator-overridable**, and the band edges are **pinned golden-guarded constants** (like `THIN_WALL_TIGHT_TOL_BUMP`) — they cannot drift silently. The ± path uses the professionally-correct size-aware ISO 286 derivation, not a guess. Q4 explicitly surfaces the band edges for Ervin to set; the operator can override the resolved band per job and per feature.
- *"The golden passes while the new code returns garbage — every default makes the new path inert, so nothing exercises it."* T3 adds **new** goldens that drive the non-default paths (a Ø12 H7 critical bore; an UltraPrecision ground feature with grinding escalation; a 'per drawing' part asserting the manual-review flag and *no* silent tightening; a scrap-uplift case), with hand-derived 4-dp expected values cross-checked by an independent reference implementation (the S7/ADR-0094 discipline); the property sweep is extended so the new branches stay finite and non-negative. Inert-by-default protects *old* prices; the new fixtures protect *new* math.
- *"'Per drawing' is a silent under-quote waiting to happen — the engine will price a GD&T-stacked part as Standard."* It resolves to the overall class **and raises `tolerance_manual_review`**, which the SPA surfaces and (Q5) can gate auto-send on. The engine never invents a tightness it cannot read; it flags for a human. The min-margin floor (`engine.rs` `MarginFloorViolation`) is unchanged and still gates the result.
- *"You can't compile this in your sandbox."* Honest deferral, SAW-OFF posture. The pure `aberp-quote-engine` crate (serde + thiserror only) compiles and tests standalone via `rustc --test`; T1–T3 + the engine-level validation are locally verifiable there. The both-arm build/test proof is editions CI (`.github/workflows/ci.yml`, `portable` + `defense --features production`); the wiring/SPA/storefront sessions (T4–T6) are CI-gated (DuckDB/Tauri/SvelteKit).
- *"Seeding the cost-rate table with positive values silently inflates every tight quote already in flight."* Precisely why the conservative call (Q6) is **zero-contribution seed rows** — nothing moves until the operator opts in by entering shop-measured values; the recommended illustrative numbers live in the plan as guidance, not an auto-migration.

## Alternatives considered

- **Fold the professional cost into the existing `machining_cost` multiplier (no new line).** Rejected for v1: it moves the existing `machining_cost` line for any non-Standard quote, forcing a golden re-pin, and re-buries the cost the operator most needs to see. A dedicated additive line mirrors `gear_cost`, keeps inert-by-default exact, and is inspectable. (Offered as Q2 if Ervin prefers it.)
- **Introduce a finer numeric IT-indexed internal scale now (replace the 5-band enum).** Rejected for v1: the 5-band `ToleranceRange` already backs the multiplier table, the golden, and the SPA; a wholesale rescale is churn with no immediate quoting benefit. The taxonomy maps cleanly onto 5 bands today; a finer scale is a clean later extension (Q3).
- **Add `&[ToleranceCostRate]` as a 12th positional engine argument.** Rejected: ADR-0094 Q2 explicitly flagged collapsing to `CatalogueSnapshot` before the 12th. We resolve it here instead of compounding the smell.
- **Put tolerance entirely on `QuotingParameters` (a per-band map).** Rejected: `QuotingParameters` is a flat singleton row mirrored 1:1 to DB columns (`catalogue.rs`); a map breaks that mapping. A sibling catalogue table is the audit-friendly, CRUD-friendly precedent (verbatim the `quoting_tolerance_multipliers` shape).
- **Expose full IT-grade/±/per-feature controls to the customer on the storefront.** Rejected for v1: customers rarely specify IT grades; over-asking loses quotes and produces garbage input. A guided class dropdown + a "critical features?" note that routes to operator review is the honest customer surface; the operator owns precision (Q6).
- **Derive the drawing dialect inside the engine.** Rejected: which dialect a drawing used is a parsing heuristic and belongs in the extractor/wiring, not the pure core. `ToleranceSpec` carries the explicit, already-classified intent; the engine only normalises and prices.
- **One big "add tolerance" session.** Rejected: violates sequential single-focus + clean-git-between. The plan sequences a refactor + three engine + two wiring/SPA + one storefront + one validation session, each its own branch and CI proof.

## Open questions

- **Q1 (flagged — confirm):** Default overall general-tolerance class = **ISO 2768-medium ↔ internal `Standard`** (multiplier 1.0). This keeps every existing quote byte-identical. *Conservative call taken; Ervin can veto* (→ pick a different default class; would move existing default quotes and re-pin the golden).
- **Q2 (flagged — veto point):** Professional tolerance cost lands in a **new additive `tolerance_cost` line** (mirrors `gear_cost`), not folded into the existing `machining_cost` multiplier. *Conservative call: new line (inspectable + inert-clean). Ervin can veto* (→ fold into the multiplier; moves the existing line, needs golden re-pin).
- **Q3 (flagged — veto point):** Internal tightness scale = **reuse the 5-band `ToleranceRange`** for v1; the taxonomy maps onto it. *Conservative call: reuse. Ervin can veto* (→ introduce a finer numeric IT-indexed scale now).
- **Q4 (flagged — confirm the numbers):** The **IT-grade → band** edges (`≤IT5→Ultra`, `IT6–7→Precision`, `IT8–9→Tight`, `IT10–11→Standard`, `IT12–14→Loose`) and the **ISO 2768 f/m/c/v → band** map. Engineering judgment; pinned golden-guarded constants. *Conservative seed proposed; Ervin can veto/adjust the edges.*
- **Q5 (flagged — veto point):** **"Per drawing"** resolves to the overall class **and raises `tolerance_manual_review`** (never silent tight/loose). *Conservative call: flag, don't guess. Ervin can veto* (→ refuse to auto-price, or assume tightest).
- **Q6 (flagged — veto point):** Customer storefront granularity = **guided overall-class dropdown + "critical features?" note → operator review**; full IT/±/per-feature is operator-only. And the new `quoting_tolerance_cost_rates` boot **seed is zero-contribution** (nothing moves until tuned). *Conservative calls taken; Ervin can veto* (→ expose full controls to the customer; and/or seed positive illustrative values).
- **Q7 (flagged — confirm sequencing):** Resolve **ADR-0094 Q2** by introducing `CatalogueSnapshot` + `quote_with_catalogue` as the **first** session (T1), since `ToleranceCostRate` is the 12th slice. *Conservative call: refactor first. Ervin can veto* (→ ship the 12th positional arg and defer the refactor).
- **Q8 (tracked):** `FeatureGraph::SCHEMA_VERSION` 4 → 5 must move in lockstep with the S269 Python extractor + wrapper `EXPECTED_SCHEMA_VERSION`. Until S269, the fields are wiring/operator-populated and default-inert. Extends ADR-0094 Q3; owning ADR: ADR-0066 follow-up / S269.
- **Q9 (deferred):** Whether tolerance-driven minutes (in-process/CMM/rework) should get their own S429 calibration family (actual vs estimated). Deferred; the existing per-family machining loop covers the routed-rate portion today.
