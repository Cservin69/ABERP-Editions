# ADR-0112 — Defense-edition CAD line: STEP-only pipeline, located-hole FeatureGraph, drilling cycle-time pricing, toolpath roadmap

- **Status:** Proposed (design pass; no code. Items flagged for Ervin — see *What we need from Ervin* and *Open questions* Q1/Q2/Q7)
- **Date:** 2026-08-16
- **Deciders:** Ervin
- **Grounds:** ADR-0093 (product-line saw-off — the compile-time `Edition` binding and the `storefront_polling_allowed` capability-gate pattern this ADR reuses), ADR-0014 (CAD/CAM artifacts — Proposed stub; explicitly excludes CAM post-processor logic), ADR-0066 (auto-quoting engine architecture), ADR-0083 (CAD encryption at rest), ADR-0094 (cost-model gap closures — the `StockForm`/`GearOp` serde-default + catalogue pattern), ADR-0097 (tolerance cost driver — the zero-contribution-seed pattern), FOUNDATION.md §2/§5 (engine purity; capability derived from the build, never user-supplied).
- **Scope guard:** Authored in the **ABERP-Editions** tree. This tree holds **two** editions and **only Defense is active**. Every behavioural change in this ADR lands in **Defense only**; the **Portable edition is frozen** and must be provably unmoved. The separate frozen prod line (`ABERP.git`, `PROD_v2.27.76`, tree `2d612811`) is never touched and is never a build target.

---

## Context

Ervin's 2026-08-15 operator decisions: **(1)** drop STL entirely — the CAD pipeline becomes STEP-only; **(2)** then build machining **toolpaths**; **(3)** this reaches into the storefront (`ABERP-site`) upload validation. Plus the 2026-08-16 scope clarification: **Defense is the only active line; Portable is frozen alongside prod.**

This section states only what was **verified against the code in this tree**, with file:line.

### C0 — How the Portable/Defense split is actually expressed (audit result)

The split is **one compile-time Cargo feature, read through one module**. There are no separate binaries, no separate crates, no `cfg`-gated modules, no runtime config, and no edition enum in the database.

**The switch.** `apps/aberp/Cargo.toml:32-34` declares `default = []` and `production = []`. That single feature drives everything:

```rust
// apps/aberp/src/build_profile.rs:26-30, :141-147
#[cfg(feature = "production")]     pub const IS_PRODUCTION_BUILD: bool = true;
#[cfg(not(feature = "production"))] pub const IS_PRODUCTION_BUILD: bool = false;

pub enum Edition { Prod, Defense, Portable }
pub const EDITION: Edition = if IS_PRODUCTION_BUILD { Edition::Defense } else { Edition::Portable };

/// Compile-time proof this tree never binds the frozen prod line.
const _: () = assert!(!matches!(EDITION, Edition::Prod));
```

`run_defense.sh:229-233` builds `--features production`; `run_portable.sh:187-191` omits it, and `:165` calls that omission out explicitly as the mechanism.

**What EDITION binds.** Data root (`.aberp-defense` / `.aberp-portable`, never `.aberp` — `build_profile.rs:157-181`); the foreign roots a build physically refuses to open (`foreign_data_dirnames()`, `:213-222`); the snapshot store segment (`:200-206`).

**The capability-gate pattern — this is the reusable part.** ADR-0093 S2 already gated a *capability* (not just a path) to Defense, and it did it in a shape this ADR copies verbatim:

```rust
// build_profile.rs:247-262 — parameterised so BOTH arms are provable in ONE compile
pub const fn storefront_polling_allowed_for(edition: Edition) -> bool {
    matches!(edition, Edition::Defense)
}
pub const fn storefront_polling_allowed() -> bool { storefront_polling_allowed_for(EDITION) }

// :264-289 — runtime backstop behind the compile-time binding
pub fn assert_storefront_reach_allowed(intent: &str) -> anyhow::Result<()> { … }
```

Two layers: a `const fn` predicate that is **total over `Edition`** and parameterised (so a single test compile can assert both arms, even though a binary only ever pins its own), plus a loud runtime refusal naming the intent. Enforcement style at the call sites is **don't spawn, don't serve**: Portable simply never starts the daemon (`serve.rs:3078`, `:3156`) and the handlers refuse (`:14580`, `:23315`, `:23424`, `:24688`).

**Reachability audit — who can reach the CAD and quoting paths today:**

| Path | Production call sites | Edition reach |
|---|---|---|
| `CadExtractor::extract` | **exactly one** — `quote_pricing_pipeline.rs:1078` | Defense only (module is storefront-gated, `:7`) |
| `engine::quote_with_catalogue` | **exactly two** — `quote_pricing_pipeline.rs:1336` (daemon) and `:3497` (`reprice_quote`) | both inside the Defense-gated pipeline module |
| `reprice_quote` via SPA | `serve.rs:13071`, `:13283`, `:13537`, `:13368` | operate on `quote_pricing_jobs` rows |

`quote_pricing_jobs` rows are created **only** by the Defense-only storefront poller, and Portable's DB is a physically separate, compile-time-bound root. **So in a Portable build the table is always empty and the quote engine is unreachable in practice.**

**Three findings that this audit turned up, each load-bearing for the design below:**

1. **The engine crate itself has no edition gate and no Cargo features at all** (`crates/aberp-quote-engine/Cargo.toml` — no `[features]` section). It is edition-*blind* by design, per FOUNDATION §2 purity. Its Defense-only-ness today is **reachability, not construction.**
2. **`build_profile.rs:239-244` explicitly documents the intent that this changes.** Verbatim: *"the pure local quote engine (`aberp-quote-engine`) and the operator/manual quoting paths stay fully available in BOTH editions — a Portable demo can still price a part the operator types in."* That manual path is **not implemented today** (no engine call site outside the pipeline module), but it is written-down intent. **A design that relies on reachability alone is designing against a documented future.** This is the single reason Part C needs a real gate.
3. **Catalogue seeding at serve boot is NOT edition-gated.** `serve.rs:1555-1594` seeds `quoting_machine_rates`, `quoting_gear_processes` and `quoting_tolerance_cost_rates` in a bare block — a Portable build seeds all three today. So "just don't seed it in Portable" is **not** an existing pattern; it would be a new, deliberate gate. (It is still the right one — see the *Edition scoping* section.)

Minor, noted for completeness: `upgrade_portable.sh:326-370` provisions the `.[step]`/OCP Python venv in Portable too, though Portable never invokes the extractor. Harmless waste; not worth a change.

### C1 — What the extractor actually produces

`aberp-cad-extract` emits a **scalar** FeatureGraph: bounding box, volume, surface area, material grade, plus `features[]` of `{feature_type, count, representative_size_mm}` (`feature_graph.py:44-87`). **No coordinates, no axes, no depths** anywhere.

Worse: **both** extractors emit `features: []` unconditionally in production — `features or []` with production callers passing `None` (`extractors/stl.py:114`, `extractors/step.py:201`). The argument exists only for test injection. So `feature_machining_minutes` is **always 0.0**, which the engine says out loud (`engine.rs:762-766`).

Machining time today is entirely:

```
roughing_min  = (stock_volume − part_volume)/1000 · machining_difficulty / mrr_rough_ref   (engine.rs:811-817)
finishing_min = surface_area_cm2 · t_finish_min_per_cm2 · machining_difficulty             (engine.rs:841-842)
machining_minutes_base = roughing_min + finishing_min + 0.0                                (engine.rs:851)
```

A part with forty M6 holes and a part with none price **identically** if volume and area match. Parts B and C close that.

### C2 — The schema version is 5, not 2 (correction to the brief)

The brief said "FeatureGraph v2". That is the **Python** side only. Three different versions are live in three places:

| Site | Value | Reference |
|---|---|---|
| Rust engine `FeatureGraph::SCHEMA_VERSION` | **5** | `crates/aberp-quote-engine/src/feature_graph.rs:546` |
| Python `SCHEMA_VERSION` | **2** | `python/.../feature_graph.py:24` |
| Wrapper `EXPECTED_SCHEMA_VERSION` | **2** | `crates/aberp-cad-extract-wrapper/src/lib.rs:98` |

The located-hole extension is therefore **v6**, not "v3". Every "FeatureGraph v3" in the brief means v6 here.

### C3 — The wrapper's version guard is exact-equality, and its own docs are wrong

`feature_graph.rs:533-545` states four times that "the version guard accepts any `schema_version <= N`". **It does not.** The only guard on the wrapper path is:

```rust
if graph.schema_version != EXPECTED_SCHEMA_VERSION {   // lib.rs:228 — EXACT equality, against 2
    return Err(ExtractError::SchemaVersionMismatch { .. });
}
```

The `<=` guard the docs describe exists **only** in the engine (`engine.rs:583`), downstream of the wrapper and never reached when the wrapper rejects first.

**Consequence, and it is the highest-risk item in this ADR:** bumping the Python `SCHEMA_VERSION` from 2 to 6 without changing `lib.rs:228` **takes down every quote in the Defense pipeline** with a `SchemaVersionMismatch` that `classify_failure` marks **Permanent** (`quote_pricing_pipeline.rs:2893`) — no auto-retry, every in-flight quote parked until an operator clicks Retry on each one, after a rebuild. Silent until deploy. Fixing it is **Part B's first commit**, standalone.

### C4 — v3/v4/v5 fields: the operator path exists, the extractor path does not

`stock_form` (v3), `gears` (v4), `tolerance` + `critical_feature_tolerances` (v5) are `#[serde(default)]` on the Rust struct (`feature_graph.rs:494`, `:501`, `:513`, `:521`) and are **absent from the Python schema entirely** (Pydantic `extra="forbid"`, v2 fields only, `feature_graph.py:68-87`).

**Correction to the working assumption that "nothing populates them".** The *operator* wiring is landed and live:

- `handle_set_quote_stock_form` (`serve.rs:13283`) → `jobs::set_stock_form` (`quote_pricing_jobs.rs:1546`) → the `stock_form` / `stock_od_mm` / `stock_id_mm` / `stock_length_mm` columns (`:326`) → `reprice_quote`.
- `handle_set_quote_gear_ops` (`serve.rs:13537`, with `gear_kind_from_db_str` / `gear_process_from_db_str` at `:13447-13511`) → `GearOp` construction → `reprice_quote`.

So ADR-0094 Gap 1 part B (S2) and Gap 3 (S6) **shipped**. What is missing is item (b) of ADR-0094's own precedence list: the **CAD-extract hint** — automatic detection. The gap is therefore not "unpriced" but **"unpriced unless a human remembers to click"**, on every quote, silently defaulting to `RectangularBlock` and empty `gears` when they don't.

The pricing effect of that silent default, direction-corrected:

- `stock_form` defaults to `RectangularBlock` ⇒ a turned Ø-bar part is billed on `bx·by·bz` instead of `π/4·d²·L` (~27 % over) *and* "roughs away" four corners never bought (`engine.rs:811`). Turned parts **over**-priced.
- A `Tube` blank bills its bore as solid metal — **over**-priced.
- `gears` defaults to empty ⇒ tooth generation costs **zero**. Geared parts **under**-priced.

Only the last one bleeds cash. The workstream that closes this is **B0**, not Part B.

### C5 — Where STL is accepted today (complete enumeration)

| # | Site | Role |
|---|---|---|
| 1 | `python/.../cli.py:29-37` `_route()` | dispatch `.stl` → `extract_stl`; error literal `"Supported: .stl, .step, .stp"` |
| 2 | `python/.../extractors/stl.py` (whole module) | numpy-stl parser |
| 3 | `python/.../extractors/__init__.py:8-10` | exports **only** `extract_stl`; docstring still claims STEP "is a stub" (stale since PR-273) |
| 4 | `python/.../pyproject.toml:15` | hard dependency `numpy-stl>=3.1,<4.0`; description line 8 |
| 5 | `python/.../cli.py:4,43` | usage + `--help` text advertise STL |
| 6 | `apps/aberp/src/quote_pricing_pipeline.rs:624-626` | storefront CAD picker: `.stl \|\| .step \|\| .stp` |
| 7 | `crates/aberp-cad-extract-wrapper/src/lib.rs:250-253` | `ExtractRequest.input_path` doc: "Both `.stl` and `.step`/`.stp` are supported" |
| 8 | `apps/aberp/src/cad_blob.rs:15,338-339` | decrypted temp file preserves extension **because** "the extractor dispatches STL vs STEP by extension" |
| 9 | `crates/.../tests/extract_smoke.rs` | **real** end-to-end STL smoke — dies |
| 10 | `crates/.../tests/common/mod.rs:106-165` `write_cube_stl` | binary-STL synthesiser used by 8 tests |
| 11 | `crates/.../tests/error_paths.rs` ×5, `schema_version.rs` ×1 | `.stl` files as **carriers only** — every one uses a stub module (`with_module`); the file is never parsed |
| 12 | `python/.../tests/test_stl_extractor.py`, `test_cli.py:26,42,111`, `conftest.py` | STL unit + CLI tests, `cube_stl_path` fixture, `_cube_mesh` writer |

### C6 — The storefront allow-list is wider than the extractor, and the mismatch is already handled

`classify_failure` documents it verbatim (`quote_pricing_pipeline.rs:2860-2866`): *"the storefront accepts 11 CAD formats but the Python dispatcher only routes `.stl`/`.step`/`.stp`. Anything else (`.iges`, `.dxf`, `.sldprt`, …) raises `ValueError` with the literal 'Unsupported file extension'"* → **Permanent** (`:2918`).

Two consequences:

1. The in-tree count is **11**, not 13 — and that is what `apps/aberp` *believes*, not what the storefront enforces. `ABERP-site` is **not on this machine**. Treat both numbers as unverified.
2. **The rejection mechanism for STL already exists and is free.** Keep the literal substring `Unsupported file extension` in the new message and it inherits Permanent classification with **no `classify_failure` change**. Part A takes this path.

### C7 — Seven real STL quotes exist in this tree

`quote-artifacts/` holds 7 quote directories, **all STL**, 4 with an issued `priced.pdf`:

```
210308f6…/GearSppinners100.stl      96fffdb2…/pump_adapter.stl   + priced.pdf
22384e8c…/GearSppinners100.stl      9dec4b87…/poolbasket_V1.stl  + priced.pdf
852ba33d…/GearSppinners100.stl      d3d9e1db…/poolbasket_V1.stl  + priced.pdf
                                    e7e09974…/GearSppiners60.stl + priced.pdf
```

Real customer quotes with PDFs that went out. Blast radius in *Part A — What breaks*.

### C8 — OCCT is already wired, already used, already provisioned

`extractors/step.py` is a working OCCT/OCP extractor: unit normalisation to MM (`:104`), `STEPControl_Reader` (`:105-111`), solid counting (`:118-125`), `BRepBndLib.AddOptimal_s` (`:169`), `BRepGProp.VolumeProperties_s` (`:178`) and `SurfaceProperties_s` (`:190`), plus an OS-level fd-1 silencer so OCCT's C++ progress bytes don't corrupt the JSON on stdout (`:59-79`). Gated behind the optional `[step]` extra (`pyproject.toml:25-29`, ~63 MB wheel).

**And the provisioning already installs it.** `run/provision_pipeline_venv.sh:47-50,74` installs `-e "${pkg_dir}[step]"` and verifies `import aberp_cad_extract, OCP`, precisely because "the gate's STEP smoke test needs OCP". Both upgraders do the same. **Part B needs no new dependency, no new process model, and no CI provisioning work.**

---

## Decision

Four workstreams, sequenced, **all scoped to the Defense edition**. **A → B0 → B → C** is the value path; **D** is a separate, much larger programme, scoped here and not designed.

Every schema and catalogue change follows the ADR-0094/0097 **inert-by-default** discipline this codebase has now used three times: `#[serde(default)]` new fields, empty catalogue slice ⇒ zero contribution ⇒ **byte-identical** `QuoteBreakdown` *and* `reasoning_log`, so every existing golden, determinism and property test stays green without edit. That discipline is also, as it happens, half of the edition-isolation mechanism — see below.

---

## Edition scoping — the exact mechanism, per part

The governing principle, taken from ADR-0093: **the gate lives in the wiring layer (`apps/aberp`), never in the pure crates.** `aberp-quote-engine` is edition-blind by design (FOUNDATION §2) and stays that way; teaching it about editions would destroy the property that makes it deterministically testable. Same for `aberp-cad-extract-wrapper` and the Python package: they are mechanism, not policy.

**Part A (STL drop) — Defense-only already; no new gate. Pin it instead.**
`CadExtractor` has exactly one production call site (`quote_pricing_pipeline.rs:1078`) inside a module whose whole storefront reach is already Defense-gated (C0). A Portable build compiles the wrapper and never calls it; a Portable operator has no way to submit a CAD file at all. The shared *source* changes (Python package, wrapper crate, both venvs) but no Portable *behaviour* does.

Adding a redundant `#[cfg]` here would be worse than nothing — it would imply the existing gate is insufficient. Instead: **pin the reachability with a test** so a future manual-upload path in Portable can't silently reopen it.

> `apps/aberp/tests/edition_cad_reach.rs` (new): assert that on the Portable arm (`#[cfg(not(feature = "production"))]`) `storefront_polling_allowed()` is `false`, and that the pricing-pipeline daemon is not spawned. Mirrors the existing `edition_db_isolation.rs` posture.

**Part B (FeatureGraph v6) — no gate needed; inert-by-default does it.**
The v6 schema change lands in the shared engine crate, but `located_holes` is `#[serde(default, skip_serializing_if = "Vec::is_empty")]`. On a Portable build the field is always empty (nothing produces it — no extractor reach), so it serialises to **no JSON key** and contributes **nothing**. Portable output is byte-identical by construction, not by policy.

**Part C (drilling pricing) — the ONLY part that needs a deliberate gate. Two layers.**

This is where C0 finding (2) bites: `build_profile.rs:239-244` says in writing that a Portable demo *should* be able to price a part an operator types in. That path doesn't exist yet — but if it is ever built, an ungated drilling model would immediately start changing Portable prices. Designing on reachability alone here is designing against the documented plan.

*Layer 1 — data (primary).* `quoting_drilling_rates` is **seeded Defense-only**. The boot seed at `serve.rs:1555-1594` gets a new gated arm; a Portable DB never gets a row; the snapshot slice is always empty; the engine never enters the drilling path; `drilling_minutes = 0.0` with **no reasoning-log line**; the breakdown is byte-identical. This reuses the ADR-0094/0097 empty-slice mechanism *as* the edition gate, which is why Part C's gating costs almost nothing.

Note this is a **new** gate, not an existing pattern — C0 finding (3) established that the other three catalogues seed in Portable today. That asymmetry is deliberate and should be called out in the diff: machine rates and tolerance bands are shared cost-model furniture; drilling cycle-time is a Defense-line capability.

*Layer 2 — capability (defence in depth).* A new predicate in `build_profile.rs`, a verbatim structural clone of `storefront_polling_allowed_for`:

```rust
// apps/aberp/src/build_profile.rs — new, immediately after the storefront gate
/// Whether a given `Edition` may run the machining cost model (drilling
/// cycle-time and, later, toolpath-derived costing). ONLY `Edition::Defense`.
/// Parameterised so BOTH arms are provable in one compile.
pub const fn machining_cost_model_allowed_for(edition: Edition) -> bool {
    matches!(edition, Edition::Defense)
}
/// `true` iff THIS build may run the machining cost model.
pub const fn machining_cost_model_allowed() -> bool {
    machining_cost_model_allowed_for(EDITION)
}
/// Runtime backstop, mirroring `assert_storefront_reach_allowed`.
pub fn assert_machining_cost_model_allowed(intent: &str) -> anyhow::Result<()> { … }
```

Gates three surfaces: the boot seed, the `quoting_drilling_rates` CRUD handlers (Portable refuses, same shape as `serve.rs:23315`), and the SPA tab (hidden in Portable, same as the storefront-settings panels).

*Explicitly rejected:* a Cargo feature on `aberp-quote-engine`. The engine takes an empty slice and does nothing — that is already the correct, pure, testable expression of "off". Adding a feature flag would make the crate's behaviour depend on build configuration, which is exactly what its determinism guarantee forbids.

**Part D (toolpaths) — Defense-only from birth, with its own predicate and a runtime backstop that actually matters.**
If D ever proceeds it gets `toolpath_generation_allowed_for(Edition)` at the same site, plus a *hard* runtime assertion at every G-code emission point — not because Portable might drift, but because D.3(5)'s liability means "which build am I?" must be checkable at the moment output is produced, not only at boot. **A Portable/demo build must never emit G-code under any configuration.**

### Proving Portable didn't move

The house already has the shape: the `production` feature is compile-time, so `cargo test --workspace` (feature **off**) *is* the Portable arm and `cargo test --features production` *is* the Defense arm — the `build_profile.rs` test module (`:305-340`) uses exactly this split. The obligation for every part of this ADR:

1. **`cargo test --workspace` (Portable arm) stays green with zero golden edits.** Any golden that needs editing is a Portable behaviour change and must be treated as a defect in the design, not a test to update.
2. **New Portable-arm assertions:** `drilling_minutes == 0.0` and the `reasoning_log` contains no `[drilling]` line on a graph carrying `located_holes`; `machining_cost_model_allowed()` is `false`; the drilling CRUD handlers refuse.
3. **New Defense-arm assertions:** the same graph with seeded rates produces non-zero drilling minutes and a full per-hole log.
4. Both arms of every new `const fn` predicate asserted in one compile via the `_for(edition)` parameterisation — the reason ADR-0093 chose that shape.

---

## PART A — Drop STL; STEP-only contract (Defense line)

### A.1 The decision

`.stl` becomes a **rejected input**, not a degraded one. The extractor accepts `.step` / `.stp` only. STL is removed from the code path rather than deprecated-in-place, because a half-live STL path is exactly the configuration that lets an STL quote slip through after the toolpath work assumes located geometry.

### A.2 The rejection contract

`cli.py::_route` becomes:

```python
_SUPPORTED_SUFFIXES = (".step", ".stp")

def _route(path: Path, material_grade: str):
    suffix = path.suffix.lower()
    if suffix in _SUPPORTED_SUFFIXES:
        return extract_step(path, material_grade)
    if suffix == ".stl":
        raise ValueError(
            "Unsupported file extension '.stl'. STL is a triangle mesh with no "
            "topology: it cannot carry hole axes, depths or diameters, so it "
            "cannot be priced for drilling or programmed for toolpaths. "
            "Re-export the part as STEP (AP203/AP214/AP242). "
            "Supported: .step, .stp"
        )
    raise ValueError(
        f"Unsupported file extension '{suffix}'. Supported: .step, .stp"
    )
```

Three deliberate properties:

1. **The literal `Unsupported file extension` is preserved**, so `classify_failure` (`quote_pricing_pipeline.rs:2918`) marks it **Permanent** with **zero Rust change**. No auto-retry storm; one clear failure.
2. **STL gets its own branch and its own message** — the customer is told *why* and *what to do*.
3. The generic branch stays for `.iges`/`.dxf`/`.sldprt`/…, unchanged.

Structured stderr shape unchanged (`cli.py:25-27`): `{"error": {"stage": "input", "message": "…"}}`, exit **2**. No new error taxonomy, no new wrapper variant, no new classifier rule.

### A.3 Removals and edits (file-level plan)

| Action | Target |
|---|---|
| **Delete** | `python/.../extractors/stl.py` |
| **Delete** | `python/.../tests/test_stl_extractor.py` |
| **Delete** | `crates/aberp-cad-extract-wrapper/tests/extract_smoke.rs` (superseded by `step_extract_smoke.rs`) |
| **Edit** | `extractors/__init__.py` — export `extract_step`; delete the stale "step is a stub" docstring |
| **Edit** | `cli.py:4,29-37,43` — route table, usage line, `--help` description |
| **Edit** | `pyproject.toml:8,15` — drop `numpy-stl`; rewrite description. **Keep `numpy`** (`heuristics.py` uses it) |
| **Edit** | `quote_pricing_pipeline.rs:621-626` — CAD picker drops `.stl`; comment updated |
| **Edit** | `wrapper/src/lib.rs:250-253` — `ExtractRequest` doc: STEP-only |
| **Edit** | `cad_blob.rs:15,338-339` — rationale becomes "the extractor requires a `.step`/`.stp` suffix" |
| **Edit** | `tests/common/mod.rs` — retire `write_cube_stl`; add `copy_step_fixture()` → `unit_cube.step` |
| **Edit** | `tests/error_paths.rs` (×5), `tests/schema_version.rs` (×1) — carrier files `.stl` → `.step`. **Behaviourally inert** (all use stub modules) but required for honesty |
| **Edit** | `python/.../tests/conftest.py` — retire `_cube_mesh`/`cube_stl_path`; add `step_fixture_path` |
| **Edit** | `python/.../tests/test_cli.py:26,42,111` — port to the STEP fixture |
| **Add** | `python/.../tests/test_cli.py` — `.stl` input exits **2**, stderr contains `Unsupported file extension` **and** `STEP` |
| **Add** | `apps/aberp/tests/` — pin `classify_failure("extract", <the STL message>) == Permanent`. Protects the free-classification trick in A.2 |
| **Add** | `apps/aberp/tests/edition_cad_reach.rs` — the Portable-arm reachability pin (see *Edition scoping*) |

### A.4 What breaks

**(a) The seven stored STL quotes (C7) — mostly fine, with one sharp edge.**

Re-pricing does **not** re-extract. `get_job_artifacts` reads the persisted `feature_graph_json` column (`quote_pricing_jobs.rs:1230`), and the engine accepts any `schema_version <= FeatureGraph::SCHEMA_VERSION` (`engine.rs:583`). A stored v2 graph from an STL keeps deserialising and keeps re-pricing forever. Issued PDFs unaffected.

The sharp edge: a job in `Fetched` or `Extracting`, or an operator clicking **Retry** on a Failed STL job, routes through `advance_extract` → `CadExtractor::extract` (`:1078-1083`) and **will now fail Permanent**. Correct — but it must be *communicated*, not discovered.

**Required before merge:** an operator-run inventory of live Defense jobs whose `cad_filename` ends `.stl` and whose `state` is not terminal. Read-only, against the live Defense tenant DB under `~/.aberp-defense/<tenant>/`, and an **operator action** (this ADR is read-only on running systems):

```sql
SELECT quote_id, state, cad_filename, failure_kind
  FROM quote_pricing_jobs
 WHERE lower(cad_filename) LIKE '%.stl'
   AND state NOT IN ('Posted', 'Failed');
```

Non-empty ⇒ drain (price and post) **before** the STEP-only build ships, or the customers re-upload as STEP. See Q1. Portable needs no such check — its `quote_pricing_jobs` is empty by construction (C0).

**(b) `real_part_gearspinner60_validation.rs` survives, loses reproducibility.** Its golden graph is a hard-coded literal described as what "the frozen-prod schema_v2 extractor produced for this STL" (`:206`). The test compiles and passes untouched; what is lost is the ability to **regenerate** it from `GearSppiners60.stl`. Mitigation: convert that STL to STEP once, offline, commit it alongside, and add a comment recording that the golden's provenance is a pre-ADR-0112 STL run. Do **not** re-derive the numbers — that silently moves a validation baseline.

**(c) CI loses its light lane — but the provisioning already exists.** *(Corrected from the first pass.)* Today `extract_smoke` proves the Python↔Rust wire **without** OCCT; afterwards every end-to-end extractor test needs the ~63 MB wheel. **However**, `run/provision_pipeline_venv.sh` already installs `.[step]` and verifies `import OCP` (C8), exactly for the cut gate's isolated worktree, and both upgraders do the same. So the residual work is **not** "build OCP into CI" — it is only: confirm the gate still calls the provisioner, and accept that a venv-less environment now fails *all* extractor tests rather than *one*. Materially smaller than first assessed.

**(d) Nothing in `aberp-quote-engine` changes.** The engine has never known what a file format is; its only STL mentions are comments (`breakdown.rs:76`, `engine.rs:762-763`), which get a wording pass.

**(e) Portable: nothing changes.** No reachable call site (C0). The shared Python package and wrapper crate change in source; Portable behaviour does not.

### A.5 Storefront (ABERP-site) — CONTRACT ONLY

> **⚠ `ABERP-site` is NOT on this machine.** Nothing in A.5 can be implemented from this repo. It is a contract for the storefront maintainer, and the format list below is derived from what `apps/aberp` *believes* (`quote_pricing_pipeline.rs:2860-2866`), **not** from reading the storefront's sniffer. The real list must be read from the repo before any of this is coded. See Q2. The storefront serves the **Defense** line only — Portable has no storefront reach by construction (C0) — so this contract is Defense-scoped by definition.

**S-1 — Accepted formats become exactly two.** `.step`, `.stp`. Extension **and** content sniff must agree; extension alone is insufficient (a `.step` file holding an ASCII STL mesh must be rejected at upload, not at extraction). A STEP file's first non-blank line begins `ISO-10303-21;` — that is the sniff.

**S-2 — Everything else is rejected at upload time,** with a hard distinction:

- **STL** (`.stl`; ASCII `solid ` prefix, or an 80-byte header + `uint32` triangle count matching file length) → its **own** error code and message. STL is the format customers are most likely to have; a generic "unsupported" here is a conversion-killer.
- **All other CAD** (`.iges`, `.igs`, `.dxf`, `.dwg`, `.sldprt`, `.ipt`, `.3mf`, `.obj`, `.x_t`, `.sat`, …) → generic unsupported-format error.

**S-3 — Error shape.** Mirrors the extractor's `{"error": {"stage", "message"}}` envelope plus a machine-readable code so the UI branches without string-matching:

```json
{
  "error": {
    "stage": "upload",
    "code": "cad_format_unsupported_stl",
    "message_en": "STL files can no longer be quoted. STL is a triangle mesh with no geometry data — it cannot describe hole positions, depths or diameters, so we cannot price machining or generate toolpaths from it. Please re-export your part as STEP (.step or .stp) from your CAD system.",
    "message_hu": "STL fájlokat már nem tudunk árazni. Az STL háromszöghálót tartalmaz, geometriai adat nélkül — nem írja le a furatok helyzetét, mélységét vagy átmérőjét, így megmunkálást sem árazni, sem szerszámpályát generálni nem tudunk belőle. Kérjük, exportálja az alkatrészt STEP (.step vagy .stp) formátumban.",
    "accepted_formats": [".step", ".stp"]
  }
}
```

with `code: "cad_format_unsupported"` and no STL-specific wording for the generic class. Bilingual EN/HU matches house convention (`quoting_machine_rates.rs:200-218`).

**S-4 — The storefront must never accept a file ABERP will reject.** The allow-list becomes a one-directional subset: storefront-accepted ⊆ extractor-accepted. The 11-vs-3 mismatch (C6) is the bug this closes permanently. The `classify_failure` rule stays as defence-in-depth, not the primary gate.

**S-5 — Existing storefront-side STL quotes.** Unknown from here. The storefront may hold quote records referencing STL uploads ABERP never fetched. Needs the repo. See Q2.

**S-6 — Ordering.** Ship the storefront tightening **first or simultaneously**. If ABERP tightens first, customers upload STL successfully and get a silent Permanent failure hours later — the worst ordering.

### A.6 Effort — Part A

| | |
|---|---|
| **Extractor + wrapper + pipeline + tests (this repo)** | **S** — ~1 focused day. Mostly deletion. The thinking is the carrier-file sweep, the classifier pin, and the Portable reachability pin. |
| **CI** | **XS** *(revised down)* — the OCP venv provisioner already exists and already runs (C8). Confirm the gate calls it; no image work. |
| **Storefront (ABERP-site)** | **UNSIZEABLE from here.** Reads as S once the repo is available, but that estimate is worth nothing until someone opens the file. |

---

## PART B0 — Extractor catch-up to v3/v4/v5 (**separate from B**)

### B0.1 The decision: its own workstream

The C4 gap is **deliberately excluded from Part B** and named here:

> **B0 — "extractor schema catch-up": bring the Python extractor from v2 to v5, auto-populating `stock_form` (and feeding the existing operator paths for `gears` / `tolerance`).**

Note the corrected shape: the operator wiring for `stock_form` and `gears` **already ships** (C4). B0 is about adding ADR-0094's precedence item (b) — the **CAD-extract hint** — so the common case stops depending on a human remembering to click.

Four reasons to split it, in order of weight:

1. **Different problem class.** Located holes are *geometry mining* (walk B-rep faces, classify cylinders). `stock_form` is *rotational-symmetry classification*. `gears`/`tolerance` are not extractable from geometry at all — a STEP file does not say "AGMA 10, module 2" — which is exactly why ADR-0094 made them operator-supplied. Different inputs, different tests.
2. **It moves prices immediately; B does not.** Auto-populating `stock_form` changes the price of every turned part on the next re-quote. That needs its own before/after validation against the ADR-0094 goldens and its own operator communication. Bundling it with a schema extension makes the diff impossible to review for price impact.
3. **It unblocks money sooner** — and the cheapest slice may not be extractor work at all, but making the *existing* gear-ops SPA path impossible to forget (a required field, a warning banner on a part that looks geared). Worth pricing that option first.
4. **Clean versioning.** B0 takes Python 2 → 5 with **no Rust schema change** (the fields exist). B then takes 5 → 6. Two independent, independently-revertable steps.

**B0 is not designed here.** Recommended: **ADR-0113**. Same Defense scoping — the extractor is Defense-reachable only (C0), and the operator handlers act on `quote_pricing_jobs` rows that exist only in Defense.

**Sequencing:** B0 and B are independent once the C3 wrapper fix lands. If forced to pick: **B0 first** — it stops the bleeding.

---

## PART B — FeatureGraph v6: located holes from STEP via OCCT

### B.1 First commit: fix the version guard (C3) — **before any schema work**

```rust
// crates/aberp-cad-extract-wrapper/src/lib.rs
/// The newest Python-side `_schema_version` this build understands.
pub const EXPECTED_SCHEMA_VERSION: u32 = 6;
/// The oldest still-accepted version.
pub const MIN_SCHEMA_VERSION: u32 = 2;

if !(MIN_SCHEMA_VERSION..=EXPECTED_SCHEMA_VERSION).contains(&graph.schema_version) {
    return Err(ExtractError::SchemaVersionMismatch {
        expected: EXPECTED_SCHEMA_VERSION,
        got: graph.schema_version,
    });
}
```

Range, not equality — matching what the engine already does (`engine.rs:583`) and what the Rust docs already (wrongly) claim. Ship **on its own**, with the `feature_graph.rs:533-545` doc corrected in the same diff, and a test that v2 and v6 both pass while v1 and v7 both fail. Until this lands, **any** Python version bump is a Defense-line outage.

### B.2 The schema addition (additive, versioned)

```python
# python/.../feature_graph.py
SCHEMA_VERSION: int = 6

class HoleEndCondition(str, Enum):
    THROUGH = "through"
    BLIND   = "blind"
    UNKNOWN = "unknown"   # topology was ambiguous — never guessed silently

class LocatedHole(BaseModel):
    model_config = ConfigDict(extra="forbid")
    diameter_mm: float          = Field(gt=0.0)
    depth_mm: float             = Field(gt=0.0)
    axis_unit: List[float]      = Field(min_length=3, max_length=3)
    entry_point_mm: List[float] = Field(min_length=3, max_length=3)
    end_condition: HoleEndCondition = HoleEndCondition.UNKNOWN
    flat_bottom: bool = False
```

```rust
// crates/aberp-quote-engine/src/feature_graph.rs
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoleEndCondition { Through, Blind, Unknown }

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LocatedHole {
    pub diameter_mm: f64,
    pub depth_mm: f64,
    pub axis_unit: [f64; 3],
    pub entry_point_mm: [f64; 3],
    #[serde(default = "HoleEndCondition::unknown")]
    pub end_condition: HoleEndCondition,
    #[serde(default)]
    pub flat_bottom: bool,
}

// on FeatureGraph — v6, additive, inert when absent:
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub located_holes: Vec<LocatedHole>,
```

`FeatureGraph::SCHEMA_VERSION` → **6**.

**Design notes, each with a reason:**

- **`skip_serializing_if = "Vec::is_empty"`** — an empty vector emits **no JSON key**, so a re-serialised v2 graph is byte-identical. Same posture as `tolerance` / `critical_feature_tolerances` (`feature_graph.rs:513,521`), and it is what keeps `feature_graph_hash` stable (`quote_pricing_pipeline.rs:1085-1088` blake3-hashes the canonical encoding). It is **also** what makes Portable provably unmoved (see *Edition scoping*).
- **Separate `located_holes` field, not a richer `Feature`.** `Feature` is a locked wire contract with a golden (`tests/property.rs`); adding fields would force `#[serde(default)]` on a struct whose point is that all three fields are mandatory. Parallel vector — the same call ADR-0097 made for `critical_feature_tolerances` (`feature_graph.rs:271-278`).
- **`axis_unit` normalised by the extractor.** The engine "does not second-guess the extractor" (`feature_graph.rs:300-303`). Extractor normalises; engine may assert.
- **`end_condition: Unknown` is first-class, never a silent default to `Through`.** A blind hole mis-read as through under-counts peck cycles ⇒ under-prices. Unknown is priced conservatively (C.2) and reasoning-logged.
- **Millimetres, in the part's own coordinate system.** No WCS, no fixture offset — the extractor has no idea where the part sits on a table. Already unit-normalised to MM at read (`extractors/step.py:104`).
- **Deliberately NOT in v6:** counterbores, countersinks, threads, tapped-vs-clearance, tool access, hole *groups*. Threads are not reliably recoverable from B-rep without semantic PMI, and guessing "M6 because Ø5.0" is exactly the silent-wrong-value class this codebase refuses (`extractors/step.py:91-98`). See Q5.

### B.3 Back-compat for stored v2 graphs

Three layers, all precedented:

1. **Wrapper**: range guard from B.1 accepts 2..=6.
2. **Engine**: `schema_version > SCHEMA_VERSION` is the only rejection (`engine.rs:583`) — a v2 graph passes, `located_holes` deserialises to `vec![]`.
3. **Pricing**: empty `located_holes` ⇒ the Part C path is never entered ⇒ `drilling_minutes = 0.0`, **no reasoning-log line**, breakdown byte-identical. Every stored graph keeps re-pricing at its historical number.

**Hash caveat, stated plainly:** a v2 graph *re-extracted* from the same STEP under a v6 extractor yields a different `feature_graph_hash` (`:1085`) — it now carries holes. Correct and desirable, but any code treating hash equality as "same part" must not treat hash inequality as "different part". Verified: nothing currently does.

### B.4 How OCCT is invoked

No new dependency, no new process model, no new provisioning (C8). Same `[step]` extra, same `_load_step_shape()` reader (`extractors/step.py:82-115`), one new face-walk inside the existing `_silence_stdout_fd()` discipline.

New module **`python/.../holes.py`**, imported by `extractors/step.py`, guarded by the same `_OCP_AVAILABLE` flag.

```
extract_step(path, material_grade)                    # existing
  └── shape = _load_step_shape(path)                  # existing, unchanged
  └── located_holes = mine_cylindrical_holes(shape)   # NEW → holes.py
```

Algorithm sketch (OCCT calls named for review; exact API pinned at implementation):

1. `TopExp_Explorer(shape, TopAbs_FACE)` — walk every face.
2. `BRepAdaptor_Surface(face)`; keep `GetType() == GeomAbs_Cylinder`. Discard convex-outward cylinders (that is the bar OD, not a hole) by testing face orientation against the surface normal — **this test is one of two main correctness risks and needs its own fixture**.
3. `adaptor.Cylinder()` → `gp_Cylinder`: `.Axis()` gives position + direction, `.Radius()` gives `diameter_mm = 2·r`.
4. `BRepTools.UVBounds_s(face)` → the `V` span is axial length ⇒ `depth_mm`. The `U` span says how much circumference the face covers: a full `2π` is a real hole; a partial sweep is a fillet, a slot end, or a split cylinder.
5. **Merge coaxial faces.** A single drilled hole is frequently several faces (split at a seam, or stepped). Group by (axis direction within angular tolerance, axis line within positional tolerance), union the axial spans. Without this a Ø8 through-hole reports as two Ø8 holes — a **2× drilling over-price**. Second main correctness risk.
6. **`entry_point_mm`** = the axial extremum of the merged span nearest the solid's outer boundary along the axis.
7. **`end_condition`**: `Through` if the span reaches the solid boundary at both ends (classify both endpoints just outside the span with `BRepClass3d_SolidClassifier`); `Blind` if one terminates inside; `Unknown` if ambiguous or the merge was uncertain.
8. **`flat_bottom`**: a planar face perpendicular to the axis capping a blind cylinder ⇒ `true` (flat-bottom drill / end-mill, different cycle).
9. Sort deterministically (`entry_point_mm` lexicographic, then diameter) before emitting. **Non-negotiable:** OCCT explorer order is not contractually stable across versions, and the whole `feature_graph_hash` + golden regime depends on byte-identical output for identical input.

**Failure posture:** hole mining must **never** fail the extraction. Any exception inside `mine_cylindrical_holes` is caught, `located_holes` comes back empty, a diagnostic is emitted — the part still prices at today's hole-blind number rather than the quote dying. Deliberately the opposite of `extract_step`'s posture on *shape* errors (where failing loud is right, because the geometry is unusable). An empty hole list is a **known-conservative** degradation with an established meaning; a corrupt hole list is not.

### B.5 Where it lands

| File | Change |
|---|---|
| `crates/aberp-cad-extract-wrapper/src/lib.rs` | B.1 range guard; `EXPECTED_SCHEMA_VERSION` 2 → 6 |
| `crates/aberp-quote-engine/src/feature_graph.rs` | `HoleEndCondition`, `LocatedHole`, `FeatureGraph::located_holes`; `SCHEMA_VERSION` 5 → 6; correct the `<=` doc lie |
| `crates/aberp-quote-engine/src/lib.rs` | re-export |
| `python/.../feature_graph.py` | the two new models; `SCHEMA_VERSION` 2 → 6 |
| `python/.../holes.py` | **new** — the face-walk |
| `python/.../extractors/step.py` | call `mine_cylindrical_holes`, guarded |
| `python/.../tests/fixtures/` | **new** STEP fixtures: `plate_4_through_holes.step`, `blind_hole_flat_bottom.step`, `stepped_bore.step`, `coaxial_split_faces.step`, `tube_od_not_a_hole.step` |
| `python/.../tests/test_holes.py` | **new** — exact expected `LocatedHole` lists |
| `crates/.../tests/feature_graph_roundtrip.rs` | v6 round-trip, no loss |
| `crates/.../tests/schema_version.rs` | v2 accepted, v6 accepted, v1/v7 rejected |

### B.6 Effort — Part B

| | |
|---|---|
| **B.1 wrapper range guard** | **XS** — under an hour, but it gates everything |
| **B.2 schema (both sides) + round-trip tests** | **S** — one day. Mechanical, precedented three times |
| **B.4 OCCT face-walk + coaxial merge + end-condition** | **M** — 3–5 days. The algorithm is a day; fixtures and the two correctness risks are the rest |
| **Fixture authoring** | **S–M** — needs a CAD seat for five known-geometry STEP parts. Bounded, not free; see Q6 |
| **Part B total** | **M** (~1 week) |

---

## PART C — Drilling cycle-time pricing (Defense-gated)

Pure arithmetic once B lands, and structurally a clone of ADR-0094 Gap 2 / ADR-0097 T3. **Do not start before B.** This is the one part carrying a deliberate edition gate — see *Edition scoping* for the two layers.

### C.1 The rate table

New catalogue row type, mirroring `MachineRate` / `GearProcessRate` / `ToleranceCostRate` (`catalogue.rs:191-286`):

```rust
// crates/aberp-quote-engine/src/catalogue.rs
/// A row from `quoting_drilling_rates` (ADR-0112 Part C). Keyed by the
/// material's machining group. An EMPTY slice ⇒ the drilling path is never
/// entered ⇒ drilling_minutes = 0.0, NO reasoning line, breakdown
/// byte-identical to pre-ADR-0112. The Portable edition never seeds this
/// table, so the empty slice IS the edition gate (ADR-0093 posture).
pub struct DrillingRate {
    pub material_group: String,
    /// Cutting feed, mm/min per mm of drill diameter.
    pub feed_mm_per_min_per_mm_dia: f64,
    /// Peck depth as a multiple of diameter.
    pub peck_depth_dia_multiple: f64,
    /// Seconds lost per peck retract-and-return.
    pub peck_retract_sec: f64,
    /// Rapid approach + retract seconds, once per hole.
    pub rapid_per_hole_sec: f64,
    /// Tool change seconds, once per DISTINCT diameter on the part.
    pub tool_change_sec: f64,
    /// Multiplier for a blind flat-bottom hole.
    pub flat_bottom_factor: f64,
    /// Multiplier when end_condition is Unknown — the conservative branch. >= 1.0.
    pub unknown_end_condition_factor: f64,
}
```

Wiring module **`apps/aberp/src/quoting_drilling_rates.rs`**, a direct structural clone of `quoting_machine_rates.rs` (575 lines): prefixed ULID `qdr_<ULID>`, lazy `CREATE TABLE IF NOT EXISTS`, invariants in code not SQL CHECK (`[[no-sql-specific]]`), CRUD auditing via `EventKind::ParametersChanged` with a self-describing `"catalogue":"quoting_drilling_rates"` payload (same blast-radius reasoning as `quoting_machine_rates.rs:22-31` — `EventKind` has ~186 variants and is not `#[non_exhaustive]`).

**Seeded zero-contribution and Defense-only.** Two independent reasons the table moves nothing on day one: the ADR-0097 Q6 zero-contribution seed (so Defense CRUD has rows to edit but nothing moves until Ervin tunes them), and the edition gate (so Portable has no rows at all).

### C.2 The formula

Per hole `h`, with `d = h.diameter_mm`, `L = h.depth_mm`:

```
feed_mm_per_min = feed_mm_per_min_per_mm_dia · d
cut_min         = L / feed_mm_per_min · machining_difficulty
peck_count      = max(0, ceil(L / (peck_depth_dia_multiple · d)) − 1)
peck_min        = peck_count · peck_retract_sec / 60
rapid_min       = rapid_per_hole_sec / 60
hole_min        = (cut_min + peck_min + rapid_min) · end_factor(h)

  where end_factor = flat_bottom_factor           if h.flat_bottom
                     unknown_end_condition_factor if h.end_condition == Unknown
                     1.0                          otherwise

tool_change_min  = distinct_diameters(located_holes) · tool_change_sec / 60
drilling_minutes = Σ hole_min + tool_change_min
```

`machining_difficulty` multiplies the **cut** term only — pecking and rapids are machine-kinematic, not material-dependent. `distinct_diameters` counts after rounding to 0.01 mm so float noise cannot invent a tool change. Every term gets its own reasoning-log line, per hole, in the established `[machining] …` format (`engine.rs:818-849`) — the log is the trust signal.

### C.3 Where it enters the engine

`engine.rs:851`:

```rust
let machining_minutes_base = roughing_min + finishing_min + feature_machining_minutes;
//                                                        + drilling_min   ← ADR-0112 Part C
```

Critically **before** the S429 calibration scaling (`:882-895`) and before machining cost (`:909+`), so drilling minutes flow into the calibration coefficient, machining cost, subtotal, overhead, margin, total, **and** the lead-time / capacity projection with no extra plumbing. That is the whole reason to put it here rather than as a separate cost line.

**Double-count guard.** If B0 or a later cut populates `features[]` with `FeatureType::Hole`, holes would be charged twice — once via `feature_machining_minutes` (`:782`) and once via `drilling_minutes`. Rule, enforced in the engine with a loud reasoning line: **when `located_holes` is non-empty, `FeatureType::Hole` rows contribute complexity/inspection but NOT machining minutes.** Located geometry wins over counted geometry. Pinned by a test.

### C.4 Where it lands

| File | Change |
|---|---|
| `crates/aberp-quote-engine/src/catalogue.rs` | `DrillingRate` |
| `crates/aberp-quote-engine/src/engine.rs` | `CatalogueSnapshot.drilling_rates` (7th slice); drilling block; double-count guard; log lines |
| `crates/aberp-quote-engine/src/lib.rs` | re-export |
| `apps/aberp/src/build_profile.rs` | **new** `machining_cost_model_allowed_for` / `_allowed` / `assert_*` (Layer 2 gate) |
| `apps/aberp/src/quoting_drilling_rates.rs` | **new** — clone of `quoting_machine_rates.rs` |
| `apps/aberp/src/lib.rs` | `pub mod quoting_drilling_rates;` |
| `apps/aberp/src/serve.rs` | **Defense-gated** `ensure_schema` + seed at boot (beside `:1580-1593`); Defense-gated REST handlers (shape of `:23315`) |
| `apps/aberp/src/quote_pricing_pipeline.rs:1338` | load rows into the snapshot |
| `apps/aberp-ui/` | Quoting-tunables SPA tab, hidden in Portable |
| `crates/aberp-quote-engine/tests/drilling_cost.rs` | **new** — modelled on `tests/tolerance_cost.rs` |
| `apps/aberp/tests/` | **new** — both-arm edition assertions (see *Proving Portable didn't move*) |
| existing goldens | **untouched**, in both arms — empty slice ⇒ byte-identical |

### C.5 Effort — Part C

**S–M, ~2–3 days**, and small *only because* B delivered located holes. Engine math + tests ≈ 1 day; the wiring module is a mechanical clone ≈ 1 day; SPA tab ≈ 0.5 day. The edition gate is ≈ 1 hour (one `const fn` pair + three call sites), because ADR-0093 already built the pattern. The one piece of real thinking is the double-count guard.

**Prerequisite that is not code:** the seed coefficients need real numbers from Ervin — feeds per material group, peck policy, tool-change time on the actual machines. Zero-contribution seeds mean the feature ships inert and stays inert until those arrive. See Q7.

---

## PART D — Toolpath roadmap (SKETCH — deliberately not designed)

> **A separate programme, not a phase of the above.** Everything A–C is *pricing*: wrong output costs money. Part D *moves a spindle*: wrong output destroys a machine, a fixture, or a person. Different liability class, different engineering discipline. **Nothing here is a design** — it is scoping so the effort is not mistaken for "one more cut after C."

ADR-0014 remains the standing stub (Proposed, 2026-05-19) and **explicitly excludes CAM post-processor logic**. Part D is what would supersede it. **Posture:** in-house, vertically integrated, no vendor CAM lock-in — consistent with the rest of this system, and a multi-year commitment that should be entered knowingly. **Defense-only from birth**, with its own capability predicate and a runtime backstop at every emission point (see *Edition scoping*).

### D.1 The path

```
FeatureGraph v6 (located holes)
   → drilling cycles          ← the only toolpath primitive nearly free after B+C
   → 2.5D pocket recognition  ← planar-floor + vertical-wall face groups from the same OCCT walk
   → 2.5D contour + pocket clearing (offset / trochoidal)
   → tool library + feeds/speeds per material                    ← data, not code, and a lot of it
   → WCS + fixturing + stock model + setup planning              ← where "software" ends and "shop" begins
   → collision / gouge checking vs stock + fixture + holder
   → simulation + verification (material removal sim, dry-run proof)
   → per-controller post-processors (Fanuc / Haas / Siemens / Heidenhain / LinuxCNC / …)
   → G-code artifact, versioned + audited per ADR-0014
```

### D.2 Phases

**D-0 — Drilling G-code only (minimum verifiable).** Located holes → canned cycles (`G81`/`G83`/`G73`) on a **single, flat, already-fixtured** face, one controller dialect, **operator-verified in dry-run before every use**. Verifiable in the sense that a human can read it and confirm it. **Not shippable unattended.** Days — precisely because it does almost nothing.

**D-1 — 2.5D pockets and contours.** Pocket recognition from B-rep, offset clearing, tool selection from a real library, stock model tracking between operations. Where a real CAM kernel starts. Months.

**D-2 — Verification.** Material-removal simulation, gouge and collision detection against stock + fixture + holder + machine envelope. **The gate between "produces G-code" and "produces G-code you may run."** Nothing before D-2 is shippable to an unattended machine. Months, and the hardest single item.

**D-3 — Post-processor framework.** Per-controller dialect: canned-cycle syntax, tool-change protocol, coolant, work offsets, safety blocks, arc conventions (`R` vs `IJK`), retract modes (`G98`/`G99`), high-speed look-ahead. Every controller is its own dialect and **every one needs verification on the actual machine** — genuinely per-machine, not per-brand.

**D-4 — Fixturing and setup planning.** Multi-op, work-holding, part orientation, WCS assignment. The point where the software must model the *shop*, not the *part*. Arguably never fully automatable; realistic target is operator-assisted.

**Minimum that produces a verifiable toolpath:** D-0.
**Minimum that is shippable:** D-0 + D-2 + one dialect of D-3, machine-verified. There is no shortcut around D-2.

### D.3 The hard parts, named honestly

1. **Collision / gouge avoidance.** Not a feature — the entire correctness problem. Needs the tool, holder, spindle nose, fixture, stock at every intermediate state, and the machine envelope. A false negative crashes a spindle.
2. **Tool library.** Real geometry, real feeds/speeds per material/coating/diameter/stickout. A **data acquisition** problem whose absence silently produces plausible G-code that breaks tools.
3. **WCS / fixturing / setup.** The gap between "the part" and "the part clamped in a vise at a known offset" is where most CAM output goes wrong.
4. **Post-processor per controller dialect.** Combinatorial, and only ever validated on iron.
5. **Liability.** A pricing bug sends a wrong invoice. A toolpath bug sends a tool into a fixture at rapid. This changes the required test discipline, the sign-off, and probably the insurance conversation. **It should be an explicit written decision by Ervin — not an emergent consequence of C going well.** It is also why D's edition gate needs a runtime assertion at emission, not merely at boot.

### D.4 Effort — Part D

**XL / multi-quarter.** D-0 alone is days. D-1 through D-3 is a **product**, not a feature — comparable to everything `aberp-quote-engine` has cost to date, with a harsher failure mode. Do not begin D-1 without a dedicated ADR (recommended: **ADR-0114**) superseding ADR-0014 and stating the liability posture explicitly.

---

## Consequences

**Easier.** One CAD path, not two — every extractor test exercises the path production uses. Geometry becomes *located*, the precondition for both real drilling cost and any toolpath work. The rate-table pattern is used a fourth time and is thoroughly proven. The C3 version-guard bug is found and fixed before it can fire. The ADR-0093 capability-gate pattern gets a second user, confirming it generalises past storefront reach.

**Harder.** A venv-less environment now fails all extractor tests, not one. Customers with only STL must re-export, and some will not (a real conversion cost, quantified nowhere in this repo — Q1). Two repos must ship in a coordinated order (S-6). `feature_graph_hash` changes on re-extraction of any part with holes. Part C introduces the **first deliberate cost-model asymmetry between the editions** — machine rates and tolerance bands seed in both, drilling rates seed only in Defense — which is a small but real divergence in what "the quote engine" means per edition, and must be documented where operators read it.

**Locked in.** STEP as the single input format — reversing means restoring a deleted parser. B-rep face-walking as the feature-mining approach (mesh-based recognition is now unavailable by construction). OCCT/OCP as a hard runtime dependency of the extractor, not an optional extra. And, if Part D proceeds, an in-house CAM kernel — the deepest commitment here by an order of magnitude.

---

## Adversarial review

**1. "You are deleting a working parser for seven paying quotes' worth of format coverage."**
Accepted, with the A.4(a) mitigation: stored graphs keep re-pricing, issued PDFs are unaffected, only in-flight/retry jobs break. The case for keeping STL is stronger than it looks — but it fails on Part D. A pipeline whose downstream stages assume located geometry cannot have an upstream stage that structurally cannot provide it; keeping STL means every future stage carries an "except for STL" branch forever. Delete it now, while the cost is seven files.

**2. "The Python `SCHEMA_VERSION` bump will take down the Defense pipeline."**
Correct as the code stands — that is C3, and it is why B.1 is a standalone first commit with its own test, shipped before any schema change. Skipped or merged out of order, the outage is certain, silent until deploy, and classified Permanent so it will not self-heal. **The single most important line in this ADR.**

**3. "Coaxial face merging is a heuristic and heuristics silently over-price."**
Correct, and named as one of two primary correctness risks in B.4. A hole split into two faces is a 2× over-price on that hole. Mitigations: a dedicated `coaxial_split_faces.step` fixture with known-correct output; deterministic sort so failures reproduce; and the reasoning log naming every merged group so an operator sees what the extractor believed. Not eliminated — *visible*.

**4. "Part C's edition gate is redundant — Portable can't reach the engine anyway."**
The strongest objection, and it is right about *today*. It fails on tomorrow: `build_profile.rs:239-244` states in writing that Portable *should* gain a manual quoting path, and on the day someone builds it, an ungated drilling model starts moving Portable prices with nobody having decided that. The gate costs ~1 hour because ADR-0093 already built the pattern; the alternative costs a silent behaviour change in a frozen edition. Cheap insurance against a documented plan.

**5. "Zero-contribution seeds mean Part C ships doing nothing."**
Yes, deliberately — the ADR-0097 Q6 precedent. The alternative is inventing feed rates, moving real prices on numbers nobody measured. The feature ships inert; the first real number is Ervin's. The accepted risk is that it sits inert indefinitely and is mistaken for "done"; Q7 exists to force that conversation.

**6. "The storefront contract is written without reading the storefront."**
Stated explicitly and repeatedly in A.5. The format list, the count (11 vs 13), and the existence of storefront-side STL records are all **unverified**. A.5 is a specification to validate against `ABERP-site`, not a description of it. The largest unknown in Part A, and why its effort is UNSIZEABLE rather than estimated.

**7. "Part D is a CAM company disguised as a feature."**
Agreed — hence a sketch with an explicit "do not begin D-1 without ADR-0114" gate and an explicit liability paragraph. The in-house/no-vendor-lock posture is honoured, but honouring it in CAM is a multi-year commitment that should be entered by written decision, not by momentum from a successful Part C.

---

## Alternatives considered

**Keep STL as a degraded tier ("STL quotes but cannot be toolpathed").** Rejected: a permanent conditional in every downstream stage, and a guarantee that the one part reaching the toolpath generator without geometry is a surprise at the machine, not at upload.

**Convert STL → STEP automatically on ingest.** Rejected: mesh-to-B-rep reconstruction is lossy, slow, and produces a *plausible* solid with fictitious "holes" — precisely the silent-wrong-value class this codebase refuses (`extractors/step.py:91-98`).

**Put located holes inside `Feature` rather than a parallel vector.** Rejected: `Feature`'s three fields are mandatory by design and golden-locked; optional fields would weaken a currently-strong contract. ADR-0097 already made this call.

**Charge drilling as a `ComplexityRule` on `FeatureType::Hole`.** Rejected: rules are per-size-bucket flat times and cannot express depth, pecking, or tool-change amortisation — the terms that dominate drilling. It would also collide with the located-hole path (C.3's guard).

**Gate Part C with a Cargo feature on `aberp-quote-engine`.** Rejected: the engine is pure and edition-blind by design (FOUNDATION §2); making its behaviour depend on build configuration breaks the determinism guarantee that makes it testable. The gate belongs in the wiring, where every other ADR-0093 gate lives.

**Gate Part C by reusing `storefront_polling_allowed()`.** Rejected: semantic overload. That predicate means "may reach abenerp.com"; drilling cost has nothing to do with network reach. A separate predicate at the same site costs one `const fn` pair and keeps both meanings honest.

**Bundle B0 into Part B.** Rejected for the four reasons in B0.1 — chiefly that B0 moves prices immediately and B does not, so bundling makes the price-impact review impossible.

**Buy a CAM kernel for Part D.** Not rejected — deferred to ADR-0114. It contradicts the stated in-house posture, but it is the only alternative that meaningfully changes D's multi-quarter shape and deserves a real hearing rather than a reflexive no.

---

## Open questions

- **Q1 — (BLOCKING Part A) Are there live STL quotes in flight?** The A.4(a) query must be run against each live **Defense** tenant DB by an operator. Non-empty ⇒ drain before shipping, or accept customer re-uploads. **Ervin's confirmation that dropping STL is acceptable for already-stored STL quotes is required before Part A merges.**
- **Q2 — (BLOCKING A.5) Where is `ABERP-site`?** Repo location/clone. Needed to confirm the real accepted-format list (11? 13?), find the sniffer, check for storefront-side STL records, and size the change. Until then A.5 is a specification, not a plan.
- **Q3 — Ordering of the two-repo ship.** S-6 recommends storefront-first-or-simultaneous. Confirm the release mechanism can do that, or accept a window where customers upload STL that fails later.
- **Q4 — B0 as ADR-0113, and is the cheapest slice even extractor work?** C4's correction means the operator paths already exist. The fastest fix for geared under-pricing may be making the *existing* gear-ops SPA path impossible to forget, not auto-detection. Worth pricing before committing to B0's scope. Recommendation stands: **B0 before B**.
- **Q5 — Threads and counterbores in v6, or later?** Current design says later: not reliably recoverable from B-rep without PMI, and guessing "M6 from Ø5.0" is the silent-wrong-value class. If tapping is a material share of shop time, revisit — but with a **separate operator-supplied input**, not a geometric guess.
- **Q6 — Who authors the five STEP test fixtures?** Needs a CAD seat and known-exact geometry. Bounded, on B's critical path, not a coding task.
- **Q7 — Real drilling numbers for the Part C seeds.** Feeds per material group, peck policy, tool-change seconds on the actual machines. Without these Part C ships inert and stays inert. Not a blocker to merging C; a blocker to C being worth anything.
- **Q8 — Does Part D proceed at all, and under whose written sign-off?** The D.3(5) liability step-change is a business decision. Recommend deferring until C is measurably improving quote accuracy.
- **Q9 — Is the Defense/Portable cost-model asymmetry acceptable?** Part C makes drilling a Defense-only capability while machine rates, gear processes and tolerance bands seed in both editions (C0 finding 3). That is the correct call for an active-vs-frozen line, but it means "the ABERP quote engine" now means slightly different things per edition. Confirm, and confirm where it gets documented for operators.
- **Q10 — Should the frozen Portable edition be pinned harder than "tests stay green"?** ADR-0093 proves prod untouched via a tree-hash. There is no equivalent artefact for "Portable behaviour unmoved" — the *source* legitimately changes in shared crates. The proposal in *Proving Portable didn't move* is test-based. If Ervin wants a stronger guarantee, the options are a recorded Portable-arm golden-output bundle or a separate Portable release branch — both are real work and neither is designed here.
