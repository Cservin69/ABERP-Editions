# Implementation plan — quote-engine cost-model gap closures (ADR-0094)

Sequenced, single-focus, clean-git-between. Companion to **ADR-0094**. All work is in the **ABERP-Editions** tree (Defense + Portable); **frozen prod is never touched**. Toolchain reality: the design/CI sandbox cannot run a full `cargo build/test` (DuckDB's C++ amalgamation ≈ 8 min, Tauri libs absent), so each session's **local** gate is logic-only and the **build-proof** is the editions CI, which builds *both* arms on every pushed branch.

## Principles for every session

- **Branch from `main`**, one session = one focused branch (`s<NN>-<slug>`), merge to `main` only when green, then the next session branches from the new `main`. Clean git between sessions; commit WIP early/often.
- **Inert-by-default:** the change must not move any existing golden/determinism/branch/property number until its new input is supplied (`#[serde(default)]` + delegation + empty-slice = today). The existing `tests/golden.rs:18-75` 4-dp lock is the tripwire.
- **Local gate (in-sandbox logic-verify):**
  1. `cargo fmt --all -- --check` (rustfmt; toolchain per `rust-toolchain.toml` = stable + rustfmt + clippy).
  2. **`rustc --test` extract** of the pure engine for engine sessions: the `aberp-quote-engine` crate depends only on `serde` + `thiserror`, so its modules + tests compile and run **standalone** (no DuckDB, no app) — run the new unit/golden/property tests here without the full workspace.
  3. `bash tools/cut_gate_db_isolation.sh` + `bash tools/cut_gate_negative_probes.sh` (toolchain-free CHECK 1–7; must stay green — no new prod launch surface, no `default_store_dir`, no cross-edition root).
- **CI build-proof (the real gate):** push the branch → `.github/workflows/ci.yml` runs the `portable` and `defense (--features production)` matrix (fmt → build `--workspace --locked --all-targets` → `cargo test` → named integration tests → clippy `-D warnings`; Portable arm also `cargo deny` + `cargo audit`), and `cut-gate.yml` runs the required `"ADR-0093 DB-isolation cut-gate"` check. Merge only when all required checks pass.
- **Prod-untouched re-proof** at the end of S7 (and recommended as a one-line check after any session that edits launchers/roots): prod tag `PROD_v2.27.76^{tree}` still `2d612811`, prod working tree fingerprint unchanged.

## Dependency order (why this sequence)

`StockForm` (S1) is consumed by the routing rule (S3: round/tube ⇒ Swiss/lathe) **and** by gear in-cycle selection (S5: skiving needs a turn-mill route). So: **Gap 1 → Gap 2 → Gap 3 → validation.** Within each gap, engine-first (pure, golden-guarded) then wiring (I/O + SPA), because the engine change is the contract the wiring fills.

---

## S1 — Gap 1 engine: `StockForm` material + roughing

- **Goal:** round-bar and tube stock volume in the pure scorer; `RectangularBlock` default reproduces today's math byte-for-byte.
- **Files:** `crates/aberp-quote-engine/src/feature_graph.rs` (add `StockForm` enum + `#[serde(default)] stock_form` field, `SCHEMA_VERSION` 2→3), `engine.rs` (branch the stock-volume step ~`:201-203`; two new reasoning-log lines; `RectangularBlock` keeps the exact existing line), `lib.rs` (re-export `StockForm`); `tests/common/mod.rs` (constructors set `stock_form: RectangularBlock`), `tests/golden.rs` (unchanged numbers; assert log still byte-identical), new `tests/stock_form.rs` (round-bar + tube hand-derived 4-dp goldens, incl. a Ø40×30 sun-blank and a Ø100/Ø80×15 ring-blank), `tests/property.rs` (extend the LCG sweep to emit round/tube forms; assert finite & ≥0).
- **Tests/assertions:** existing golden/determinism/branches **unchanged**; new round-bar bills `π/4·d²·L·(1+scrap)`; tube excludes the bore from both material and roughing; property sweep never panics on the new branch.
- **Gates:** local (fmt + `rustc --test` of engine + cut-gate) → push → CI both arms green → merge.

## S2 — Gap 1 wiring: stock-form intake

- **Goal:** operator/extractor supplies the stock form; the pipeline stamps it onto the `FeatureGraph` before the engine call.
- **Files:** `apps/aberp/src/quote_pricing_pipeline.rs` (set `stock_form` on the graph it hands to `engine::quote_with_calibration` ~`:882`; precedence operator field > extractor hint > `RectangularBlock`), the quote/part record + schema (nullable `stock_form` + `od_mm`/`id_mm`/`length_mm`), intake/SPA control, `convert_*` if a new snapshot field is needed. Audit: stamp the chosen form into the existing pricing audit payload.
- **Tests:** pipeline unit (each precedence path), an app-level quote test (a turned part prices on bar stock end-to-end).
- **Gates:** local fmt + cut-gate (full build needs DuckDB ⇒ CI) → push → CI both arms → merge.

## S3 — Gap 2 engine: family rates + routing + lights-out

- **Goal:** per-family effective rate, unattended factor, geometry→family routing; absent rate rows ⇒ today's global rate.
- **Files:** `crates/aberp-quote-engine/src/capacity.rs` (add `SwissTurnMill`, `TurnMill`, `FourAxisMill`; update `ALL` `[_;8]`→`[_;11]` + `as_db_str`; `from_db_str` auto-covers via `ALL`), `catalogue.rs` (`MachineRate` type), `engine.rs` (new superset entry point `quote_with_shop_model(...)` taking `&[MachineRate]`; `quote_with_calibration` delegates with an empty slice; `route_family(...)` pure fn; effective-rate selection at the machining step `:402-405`; lights-out factor; reasoning-log), `lib.rs` (re-exports), add `bar_capacity_mm` to `QuotingParameters` (`catalogue.rs`) — defaulted so golden stays inert; `tests/branches.rs`/new `tests/machine_rates.rs` (family selection, lights-out math, no-rows = global-rate byte-identical), `tests/determinism.rs` (new enum order stable), `tests/property.rs` (extend).
- **Touch-point checklist (enum extension):** `capacity.rs` variants + `ALL` + `as_db_str`; `MachineFamily::for_route` (superseded by `route_family`, keep for callers); confirm no other exhaustive `match MachineFamily` exists (verified: only `for_route`); `calibration.rs` auto-tolerant (absent ⇒ 1.0). App-side closed-vocab validator updated in S4.
- **Gates:** local fmt + `rustc --test` engine + cut-gate → push → CI both arms → merge.

## S4 — Gap 2 wiring: rate catalogue + seed + routing override

- **Goal:** a `quoting_machine_rates` DB table the pipeline snapshots into `&[MachineRate]`; seeded defaults; operator family override.
- **Files:** new `apps/aberp/src/quoting_machine_rates.rs` (mirror `quoting_machines.rs` shape: schema, `get/list`, CRUD, audit events, **seed** the six families per ADR-0094's proposed rates), `quote_pricing_pipeline.rs` (load + `convert_machine_rates` → pass to `quote_with_shop_model`; optional per-quote family override), `quoting_machines.rs` (extend the family closed-vocab validator to accept the three new db-strings; SPA dropdown reads `MachineFamily::ALL`), SPA settings page for rates.
- **Tests:** table CRUD + seed presence; pipeline picks the routed family's rate; override path; empty-table fallback to global rate.
- **Gates:** local fmt + cut-gate → push → CI both arms → merge.

## S5 — Gap 3 engine: gear-op model

- **Goal:** costed gear operations summed into a new `gear_cost`; empty gear vector ⇒ no change.
- **Files:** `crates/aberp-quote-engine/src/feature_graph.rs` (`GearKind`, `GearProcess`, `GearOp`; `#[serde(default)] gears: Vec<GearOp>`; `SCHEMA_VERSION` 3→4), `catalogue.rs` (`GearProcessRate`), `breakdown.rs` (`#[serde(default)] gear_cost: f64`; folds into subtotal), `engine.rs` (gear loop: external hob/skive, internal shape/broach/wire-EDM; `Auto` selection by kind + routed family + AGMA; `in_cycle_factor` when skiving on a turn-mill route; per-gear reasoning-log; add `gear_cost` to subtotal `:571`), `lib.rs` (re-exports), pinned AGMA-band/face-width/quality constants; new `tests/gears.rs` (external skive in-cycle vs hob, internal ring shape vs wire-EDM, AGMA bands, 4-dp goldens), `tests/golden.rs` (add `gear_cost == 0.0` assertion — existing totals unchanged), `tests/property.rs` (extend with gears; finite & ≥0).
- **Gates:** local fmt + `rustc --test` engine + cut-gate → push → CI both arms → merge.

## S6 — Gap 3 wiring: gear intake + process catalogue

- **Goal:** gear features on the quote/part; a `quoting_gear_processes` table snapshotted to `&[GearProcessRate]`.
- **Files:** new `apps/aberp/src/quoting_gear_processes.rs` (schema, CRUD, audit, **seed** hob/skive/shape/broach/wire-EDM coefficients), quote/part schema + SPA (per-gear module/teeth/face/AGMA/process rows), `quote_pricing_pipeline.rs` (stamp `gears` onto the graph + `convert_gear_processes` → `quote_with_shop_model`).
- **Tests:** table CRUD + seed; a geared part prices end-to-end with the gear line visible; internal-vs-external selection.
- **Gates:** local fmt + cut-gate → push → CI both arms → merge.

## S7 — Validation + integration: planetary-box golden  ✅ DONE (2026-06-25)

- **Goal:** the motivating case, as a **new** fixture (the €444 baseline is external — see Risk R1), proving direction + decomposition.
- **Files:** new app-level (or engine-level, if all inputs are operator-set) golden test `apps/aberp/tests/quote_planetary_gearset.rs`: Ø100 planetary set — internal **ring** (shaped/wire-EDM, tube blank), 5 **planets** + **sun** (external, skived in-cycle on Swiss turn-mill, round-bar blanks), **carrier** (prismatic, 3-axis), 5 **pins** (round bar, Swiss lights-out), **hub** — qty 100. A short `docs/findings/` note recording the €444 provenance + the new per-line decomposition.
- **Assertions:** per-box total **materially below €444**; the reasoning log contains, and the test inspects, the *specific lines*: bar-stock material (`stock_form=round_bar …`), Swiss lights-out rate (`effective_rate = attended × lights_out_factor`), skived external gears (`in-cycle`), and the expensive shaped internal ring. Honest note baked into the test comment: for a *trivial* part (a pin) the dominant term is amortized CAD-CAM programming (`cad_cam_base_hours × rate ÷ qty`), not cut cost — so the pin's reduction comes from the machining/setup portion routing to lights-out, while the *box-level* win is dominated by the geared parts (skiving in-cycle replacing a flat manual gear adder) and all turned blanks billing on bar stock.
- **Gates:** local fmt + `rustc --test` engine + cut-gate → push → CI both arms → merge → **prod-untouched re-proof**.

---

## Risks

- **R1 — the €444 baseline is not in this tree.** Verified absent (four sweeps). Validation is a *new* fixture; the target is framed as direction + decomposition, not an exact re-match. *Mitigation:* Q4 in ADR-0094 asks Ervin to confirm the baseline's inputs so the fixture is anchored.
- **R2 — golden drift.** Any engine change that accidentally moves the default path breaks `tests/golden.rs`. *Mitigation:* the inert-by-default mechanism (serde defaults + delegation + empty slices) is designed so the default path is the *same code* as today; CI golden is the backstop on every branch.
- **R3 — `MachineFamily` extension misses a match arm or a db-string round-trip.** *Mitigation:* enumerated touch-points in S3; `from_db_str` fails loud on an unknown string (`capacity.rs:111-113`, CLAUDE.md #12) rather than silently bucketing to `Other`.
- **R4 — entry-point arg explosion** from the delegation chain. *Mitigation:* accepted now (repo precedent); ADR Q2 tracks the `CatalogueSnapshot` collapse when arg count crosses ~11.
- **R5 — routing/gear-process heuristics mis-route an odd part.** *Mitigation:* every decision reasoning-logged + operator-overridable per quote; thresholds are golden-guarded constants; S429 calibration self-corrects family estimates over time.
- **R6 — gear time model is a parametric estimate, not a CAM simulation.** *Mitigation:* coefficients are operator-tunable catalogue rows; flagged as calibratable (ADR Q5). v1 is "defensible and itemised," not "CAM-exact."
- **R7 — sandbox cannot compile ⇒ logic bugs surface only at CI.** *Mitigation:* `rustc --test` extracts of the pure engine catch the math locally; wiring bugs caught at CI both-arm build. Slower loop accepted (SAW-OFF posture).
- **R8 — `SCHEMA_VERSION` bumps must stay in lockstep with the S269 extractor + S270 wrapper.** *Mitigation:* fields are default-inert until the extractor emits them; ADR Q3 tracks the lockstep bump.
- **R9 — schema/SPA churn across three new tables.** *Mitigation:* each mirrors the existing `quoting_*` table pattern verbatim; per-tenant DuckDB singletons/rows with the same audit discipline.

## Validation target (success criterion)

After S1–S7 land, re-quoting the Ø100 planetary box (qty 100) yields a **realistically lower, more-defensible per-box number than the external €444**, in which: the **pins** and turned blanks bill on **bar stock** (~21 % less material + less roughing) and route to a **Swiss lights-out** effective rate (no longer the attended-rate, square-block line that produced ~€1.93/pin); the **sun + planets** are **power-skived in-cycle** on the turn-mill (cheap) instead of a flat manual gear adder; the **internal ring** carries an explicit, justified shaping/wire-EDM line; and **every** contribution is an inspectable line in the `reasoning_log`. Prod tree-hash `2d612811` re-proved untouched.


---

## Status — ADR-0094 chain COMPLETE (S7 validation, 2026-06-25)

S1–S7 have landed. **S7** adds the planetary-box validation golden
`crates/aberp-quote-engine/tests/planetary_box_validation.rs`. It is an
**engine-level** fixture (every input is operator-set, so — per this section's
own "or engine-level, if all inputs are operator-set" — it runs fully
in-sandbox with no DuckDB), pricing the Ø100 compound planetary gearbox (ring +
5 planets + sun + carrier + 5 pins + hub) at **qty 100 boxes** through
`quote_with_shop_model` with the seeded **6-family machine-rate** + **5-process
gear** catalogues. An independent reference implementation of the engine
arithmetic was run alongside and **agreed with the Rust engine to 4 dp on every
line** — the pinned goldens are cross-checked, not just self-consistent.

**Per-box result (upgraded engine, 6061-T6 @ €6/kg, Standard tol):**

| component | per-box × | upgraded €/part | naive €/part |
|---|---|---|---|
| ring (ext Z50 + int ring Z60) | ×1 | 227.3354 | 126.3572 |
| planet (ext Z24) | ×5 | 10.2000 | 10.8131 |
| sun (ext Z18) | ×1 | 10.6314 | 9.8753 |
| carrier (prismatic) | ×1 | 80.2016 | 80.2016 |
| pin | ×5 | 1.0492 | 2.4452 |
| hub | ×1 | 11.4491 | 26.0072 |
| **PER BOX** | | **385.8635** | **308.7331** |

- realistic pre-gap quote = naive €308.73 + legacy €95 gear adder = **€403.73**;
  external manual baseline = **€444**. Upgraded **€385.86 < €403.73 < €444**. ✓

**The five structural wins (the validation — not a € target) — all asserted green:**
1. every cost line is present in each `reasoning_log` and the named lines
   reconstruct the subtotal + total;
2. planets/sun/pins (OD ≤ bar-cap 32) route to **Swiss lights-out €0.5250/min**,
   ring/hub (OD > 32) to **turn-mill lights-out €0.7200/min** — never the flat
   €1.6667 (€100/h);
3. gears are costed (`gear_cost > 0` on geared parts, `== 0` elsewhere) and
   external skiving (€9.36 on the ring) ≪ internal-ring shaping (€99.36);
4. every round/tube part bills exactly **π/4 ≈ 78.5 %** of its bbox-block
   material (Gap 1);
5. the per-box total is finite, > 0, and below both realistic baselines.

**⚠️ Honest baseline flag (conservative call, not rigged).** Against the
*strict* no-gear baseline (€308.73) the upgraded box (€385.86) is **higher** —
correctly so: Gap 3 surfaces ~€133/box of real gear-generation cost that the
pre-Gap-3 engine simply could not see. A no-gear baseline structurally
under-quotes a gearbox, so it is not a fair reference; the fair references
(legacy-adder quote €403.73 and the external €444) are both beaten. The test
documents this relationship explicitly rather than inverting it by fudging
inputs.

**⚠️ Seeds are illustrative, and self-correct.** The seed machine €/min, the
gear-process coefficients, and the tooth counts / modules / face widths /
AGMA classes are Dispatch-supplied **illustrative** values, not shop gospel.
They self-correct through the **S429 calibration loop** (mean actual ÷ estimated
per machine family) once Ervin enters the shop's measured rates and a real gear
drawing. Recheck every gear parameter and seed rate before quoting a real
planetary set (ADR-0094 Q4/Q5).

**Gate (in-sandbox, pure engine crate):** `rustfmt --check` clean; `cargo test
-p aberp-quote-engine` **15/15 binaries green** (the new fixture + every prior
golden/determinism/branch/property test byte-identical — no engine source
changed); `cargo clippy -p aberp-quote-engine --all-targets -- -D warnings`
clean; `tools/cut_gate_db_isolation.sh` PASS. The full both-arm
`--workspace` build-proof is Dispatch's consolidated Gap-2+Gap-3 CI run (this
session deliberately builds only the engine crate — the bundled-DuckDB
workspace build blows the sandbox disk). Prod tree-hash `2d612811` re-proved
untouched.
