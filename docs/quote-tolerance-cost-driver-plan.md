# Implementation plan — quote-engine machining-tolerance cost driver (ADR-0097)

Sequenced, single-focus, clean-git-between. Companion to **ADR-0097**. All work is in the **ABERP-Editions** tree (Defense + Portable); **frozen prod is never touched**. Toolchain reality (unchanged from ADR-0094): the design/CI sandbox cannot run a full `cargo build/test` (DuckDB's C++ amalgamation ≈ 8 min, Tauri libs absent), so each session's **local** gate is logic-only and the **build-proof** is the editions CI, which builds *both* arms on every pushed branch.

This is a **design-pass companion**: no code lands in this session. It defines the session boundaries Ervin (or auto-mode) executes next, in order.

## Principles for every session

- **Branch from `main`**, one session = one focused branch (`t<NN>-<slug>`), merge to `main` only when green, then the next session branches from the new `main`. Clean git between sessions; commit WIP early/often.
- **Inert-by-default:** the change must not move any existing golden/determinism/branch/property number until its new input is supplied (`#[serde(default)]` + delegation + empty-slice + zero-contribution seed = today). The existing `tests/golden.rs` 4-dp lock (`tolerance = Standard`) is the tripwire.
- **Local gate (in-sandbox logic-verify):**
  1. `cargo fmt --all -- --check`.
  2. **`rustc --test` extract** of the pure engine for engine sessions: `aberp-quote-engine` depends only on `serde` + `thiserror`, so its modules + tests compile/run standalone (no DuckDB, no app).
  3. `bash tools/cut_gate_db_isolation.sh` + `bash tools/cut_gate_negative_probes.sh` (toolchain-free CHECK 1–7 — no new prod launch surface, no `default_store_dir`, no cross-edition root).
- **CI build-proof (the real gate):** push the branch → `.github/workflows/ci.yml` runs the `portable` and `defense (--features production)` matrix (fmt → build `--workspace --locked --all-targets` → `cargo test` → named integration tests → clippy `-D warnings`; Portable arm also `cargo deny` + `cargo audit`), and `cut-gate.yml` runs the required `"ADR-0093 DB-isolation cut-gate"` check. Merge only when all required checks pass.
- **Prod-untouched re-proof** at the end of T7 (and after any session touching launchers/roots): prod tag `PROD_v2.27.76^{tree}` still `2d612811`, prod working-tree fingerprint unchanged.

## Dependency order (why this sequence)

The new `ToleranceCostRate` slice is the input that crosses the engine's 11-arg ceiling, so the **`CatalogueSnapshot` refactor (T1, resolves ADR-0094 Q2) goes first** — every later engine entry point is then a snapshot field, not a positional arg. Taxonomy types (T2) are the contract the cost model (T3) prices and the wiring (T4) fills, so: **refactor → taxonomy → cost model → wiring → SPA → storefront → validation.** Within each, engine-first (pure, golden-guarded) then I/O. T6 (storefront) is a **separate repo** and can run in parallel with T5 once the T4 intake contract is merged.

**Locally-verifiable (pure engine crate):** T1, T2, T3, and the engine-level half of T7. **CI-gated (DuckDB/Tauri/SvelteKit):** T4, T5, T6, and the app-level half of T7.

---

## T1 — Engine refactor: `CatalogueSnapshot` + `quote_with_catalogue` (resolves ADR-0094 Q2)

- **Goal:** collapse the 11 positional engine inputs into one `CatalogueSnapshot<'a>` and add the superset entry `quote_with_catalogue(&FeatureGraph, &CatalogueSnapshot, &QuotingParameters, qty, target_tolerance, &CalibrationTable)`; existing entries (`quote`, `quote_with_calibration`, `quote_with_shop_model`) delegate to it. **No behaviour change, no new field** — pure structural refactor so the next sessions add a snapshot field, not a 12th arg.
- **Files:** `crates/aberp-quote-engine/src/engine.rs` (define `CatalogueSnapshot`; new entry; rewrite the three existing entries as thin delegators), `lib.rs` (re-export `CatalogueSnapshot`); `tests/common/mod.rs` (snapshot constructor); all engine tests compile-only churn (call sites unchanged because the old entries still exist).
- **Tests/assertions:** **every existing golden/determinism/branch/property number byte-identical** (this is a no-op refactor); the new `quote_with_catalogue` reproduces `quote_with_shop_model` exactly (add one equivalence test).
- **Gates:** local (fmt + `rustc --test` engine + cut-gate) → push → CI both arms green → merge.

## T2 — Engine: tolerance taxonomy → tightness scale

- **Goal:** the four drawing dialects as pure types + a deterministic, reasoning-logged normaliser onto `ToleranceRange`. Inert: `Unspecified` default ⇒ engine uses the resolved `target_tolerance` arg = today.
- **Files:** `crates/aberp-quote-engine/src/feature_graph.rs` (`ToleranceSpec` tagged enum + `GeneralClass`; `#[serde(default)] tolerance: ToleranceSpec` on `FeatureGraph`; `FeatureTolerance` + `#[serde(default)] tolerance: Option<FeatureTolerance>` on `Feature`; `SCHEMA_VERSION` 4→5, guard `≤5`), `engine.rs` (pure `tightness(spec, nominal_mm) -> ToleranceRange`; IT/2768/± band edges as pinned constants; reasoning-log lines), `lib.rs` (re-exports); `tests/common/mod.rs` (constructors set `Unspecified`/`None`), `tests/golden.rs` (numbers unchanged; assert log byte-identical), new `tests/tolerance_taxonomy.rs` (each dialect→band, size-aware ± derivation, per-drawing flag), `tests/feature_graph_compat.rs` (v4 graph without `tolerance` still loads → `Unspecified`).
- **Tests/assertions:** existing goldens unchanged; 2768-m→Standard; IT7→Precision; ±0.01@Ø10→IT6→Precision vs ±0.01@Ø250→looser; per-drawing sets the manual-review flag and does **not** silently tighten.
- **Gates:** local (fmt + `rustc --test` engine + cut-gate) → push → CI both arms → merge.

## T3 — Engine: `ToleranceCostRate` cost model + `tolerance_cost` line

- **Goal:** the additive, itemised tolerance cost in the pure scorer; empty rate slice + default spec ⇒ `tolerance_cost = 0.0`, no log line, today's totals.
- **Files:** `crates/aberp-quote-engine/src/catalogue.rs` (`ToleranceCostRate`; add `tolerance_cost_rates: &[ToleranceCostRate]` to `CatalogueSnapshot`), `breakdown.rs` (`#[serde(default, skip_serializing_if = "is_zero_eur")] tolerance_cost: f64`; fold into subtotal — branch the subtotal log exactly as `gear_cost` does so a zero line keeps the bytes), `engine.rs` (compute the decomposition — Σ critical-feature in-process+CMM min, finish-pass × feed-slowdown min, scrap/rework pct on material+machining, grinding escalation via the `Grinder` `MachineRate`; cost at the routed effective €/min from ADR-0094 Gap-2; per-term reasoning-log; add to subtotal), `lib.rs` (re-exports), pinned grinding/escalation constants; new `tests/tolerance_cost.rs` (Ø12 H7 critical bore; UltraPrecision ground feature with grinding adder; scrap-uplift case; per-drawing flag with `tolerance_cost == 0` when no rates), `tests/golden.rs` (assert `tolerance_cost == 0.0`; totals unchanged), `tests/property.rs` (extend sweep with specs/critical features; finite & ≥0).
- **Tests/assertions:** existing totals byte-identical; a seeded tight bore adds exactly the hand-derived gauging+CMM+rework EUR; grinding escalation only fires at the tightest band; reference-impl cross-check to 4 dp on every new line (S7/ADR-0094 discipline).
- **Gates:** local (fmt + `rustc --test` engine + cut-gate) → push → CI both arms → merge.

## T4 — Wiring: schema, catalogue table + seed, pipeline precedence, store-per-job

- **Goal:** operator/extractor/customer supply tolerance; the pipeline resolves precedence and stamps it; the cost-rate catalogue is DB-backed; tolerance is persisted per-job and re-pricing stops hardcoding `Standard`.
- **Files:** `apps/aberp/src/quote_pricing_jobs.rs` (`ALTER TABLE quote_pricing_jobs ADD COLUMN IF NOT EXISTS tolerance_class VARCHAR / tolerance_spec_json VARCHAR / tolerance_manual_review BOOLEAN`; extend `JobDetail` + the SELECT; `set_tolerance(...)` mirroring `set_stock_form` at `:1510`), `apps/aberp/src/quoting_tunables.rs` (`CREATE TABLE quoting_tolerance_cost_rates` mirroring `quoting_tolerance_multipliers` at `:467`; **zero-contribution** seed rows for all five bands; list/update fns + `EventKind::ToleranceCostRatesChanged`), `apps/aberp/src/quote_pricing_pipeline.rs` (`convert_tolerance_cost_rates`; build `ToleranceSpec` + per-feature callouts on the graph; precedence operator-override > extractor/drawing hint > daemon default; pass via `CatalogueSnapshot`; **fix re-price at `:3039-3040,3152` to read the stored per-job tolerance**; audit-stamp the resolved band + manual-review flag), `apps/aberp/src/serve.rs` (intake mapping: accept tolerance fields from the storefront JSON; routes for the new catalogue CRUD).
- **Tests:** pipeline unit (each precedence path; manual-review flag set on per-drawing); migration test (pre-T4 DB ALTERs idempotently; old rows price as Standard); an app-level quote test (a tight critical bore prices with a non-zero `tolerance_cost` end-to-end); re-price test (stored tolerance honoured, not Standard).
- **Gates:** local fmt + cut-gate (full build needs DuckDB ⇒ CI) → push → CI both arms → merge.

## T5 — SPA: catalogue CRUD + per-job tolerance editor

- **Goal:** operator UI for the cost-rate catalogue and per-job tolerance (overall + per-critical-feature).
- **Files:** `apps/aberp-ui/ui/src/routes/QuotingToleranceCostRatesList.svelte` (new — mirror `QuotingToleranceMultipliersList.svelte`: edit-in-place over the seeded band rows), `apps/aberp-ui/ui/src/routes/MaintenanceDashboard.svelte` (+ route), `apps/aberp-ui/ui/src/routes/PricingJobDetail.svelte` (tolerance editor mirroring the stock-form editor at `:1048`; overall class + per-feature callouts as class/IT/± dropdowns; surface the manual-review banner), `apps/aberp-ui/ui/src/lib/api.ts` (+ `listToleranceCostRates`/`updateToleranceCostRate`/`setQuoteTolerance` bindings + types), `apps/aberp-ui/ui/src/lib/quoting-tunables-format.ts` (labels), `apps/aberp-ui/src/commands.rs` (Tauri command bindings).
- **Tests:** `*.test.ts` for the new format/labels + api shapes; svelte-check; a demo-mode guard (read-only) consistent with existing routes.
- **Gates:** local fmt (SPA build/`svelte-check` ⇒ CI) → push → CI both arms (incl. SPA build) → merge.

## T6 — Storefront `/quote` tolerance field (separate Lightsail repo — coordinated, out of this tree)

- **Goal:** the customer can state a general-tolerance class + flag critical features; the intake JSON carries it; ABERP stores it (T4 already maps it).
- **Files (storefront repo, NOT ABERP-Editions):** the `/quote` SvelteKit form (guided class dropdown + "tighter on critical features?" note), the submission payload schema, the `GET /api/quotes` projection ABERP polls. **ABERP side already done in T4** (`serve.rs` intake mapping + per-job columns).
- **Tests:** storefront form validation; a contract test that an ABERP intake round-trips the new fields (can live in `apps/aberp/tests` against a sample payload).
- **Gates:** storefront repo CI; ABERP contract test on the editions CI. **Flagged:** cross-repo coordination — sequence after T4 merges so the contract is fixed. Out of this bundle's tree; tracked, not built here.

## T7 — Validation + integration: tolerance golden + prod-untouched re-proof

- **Goal:** prove direction + decomposition on a real scenario; re-confirm inert-by-default end-to-end.
- **Files:** new engine-level golden `crates/aberp-quote-engine/tests/tolerance_validation.rs` (re-price an ADR-0094 planetary component — e.g. the sun with a Ø-bore H6 critical fit + the ring with an UltraPrecision ground face — through `quote_with_catalogue` with a seeded `ToleranceCostRate` catalogue; assert each `tolerance_cost` term reconstructs from the log, a Standard/no-callout variant is **byte-identical** to the ADR-0094 result, and a 'per drawing' variant raises the flag with zero silent cost); a `docs/findings/` note recording the chosen seed cost-rates' provenance + the per-line decomposition.
- **Tests/assertions:** the five structural wins — (1) every tolerance term present in the log and reconstructing the line; (2) the Standard variant equals the ADR-0094 per-box number to 4 dp (inert proof); (3) `tolerance_cost > 0` only where a tighter spec/critical callout is supplied; (4) grinding escalation fires only at the tightest band; (5) totals finite, > 0, above the no-tolerance baseline by exactly the itemised sum.
- **Gates:** local (fmt + `rustc --test` engine + cut-gate) → push → CI both arms → merge → **prod-untouched re-proof** (`PROD_v2.27.76^{tree}` == `2d612811`).
- **Defense cut:** after T7 merges, tag the editions head (e.g. `PROD_Defense_v0.3.0`) per ADR-0056 release-branch versioning; the cut is a tag on the editions tree only — prod untouched.

---

## Risks

- **R1 — double-count perception (legacy multiplier + new line).** *Mitigation:* the two are itemised separately in the reasoning log and measure different quantities (ADR-0097 adversarial review #1); Q2 offers the single-mechanism alternative if Ervin prefers.
- **R2 — golden drift.** Any change that moves the default (`Standard`) path breaks `tests/golden.rs`. *Mitigation:* `Unspecified` default + empty rate slice + **zero-contribution seed** is the *same code path* as today; CI golden is the per-branch backstop.
- **R3 — band-edge / ±-derivation disputes.** The IT/2768/± mappings are judgment. *Mitigation:* pinned golden-guarded constants, reasoning-logged, operator-overridable per job/feature; Q4 surfaces the edges for Ervin before T2 builds.
- **R4 — seed inflation.** A positive seed would silently raise in-flight tight quotes. *Mitigation:* zero-contribution seed (Q6); illustrative values documented, not auto-applied; S429 self-corrects once real values are entered.
- **R5 — schema lockstep.** `SCHEMA_VERSION` 5 must match the S269 extractor. *Mitigation:* fields default-inert until S269; Q8 tracks the lockstep bump (extends ADR-0094 Q3).
- **R6 — cross-repo storefront drift (T6).** *Mitigation:* fix the intake contract in T4 first; ABERP contract test guards the shape; T6 sequenced after T4.
- **R7 — sandbox cannot compile ⇒ wiring bugs surface only at CI.** *Mitigation:* `rustc --test` engine extracts catch the math locally (T1–T3, T7-engine); wiring/SPA/storefront caught at CI both-arm build. Slower loop accepted (SAW-OFF posture).

## Validation target (success criterion)

Not a single € number. The criterion is **direction + decomposition + inert-by-default**: with the cost-rate catalogue seeded, a part carrying a tighter overall class or a per-critical-feature callout shows a **non-zero, fully itemised** `tolerance_cost` (in-process gauging, CMM, scrap/rework, slower-feed finishing, and — at the tightest band — a grinding adder), every term inspectable in the reasoning log and reconstructing the line; **and** the same part at the default class with no callouts and no seeded rows prices **byte-identical** to the ADR-0094 result. Tolerance becomes a readable, defensible, operator- and customer-driven cost line, while every existing quote is provably unchanged until a new input is supplied.
