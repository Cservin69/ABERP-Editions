# ADR-0112 — STEP-only CAD pipeline, located-hole FeatureGraph extension, drilling cycle-time pricing, and the toolpath roadmap

- **Status:** Proposed (design pass; no code. Three items flagged for Ervin — see *What we need from Ervin* and *Open questions* Q1/Q2/Q7)
- **Date:** 2026-08-16
- **Deciders:** Ervin
- **Grounds:** ADR-0014 (CAD/CAM artifacts — Proposed stub; explicitly puts "CAM post-processor logic" out of scope), ADR-0066 (auto-quoting engine architecture), ADR-0083 (CAD encryption at rest), ADR-0093 (product-line saw-off), ADR-0094 (cost-model gap closures — the `StockForm`/`GearOp` catalogue+serde-default pattern this ADR mirrors), ADR-0097 (tolerance cost driver — the `ToleranceCostRate` dormant-table pattern this ADR mirrors), FOUNDATION.md §2/§5 (engine purity; path-derived inputs).
- **Scope guard:** Authored in the **ABERP-Editions** tree (Defense/Editions line). Portable (`ABERP.git`) is **FROZEN** and is never a build target for any part of this ADR. Frozen prod is never touched.

---

## Context

Ervin's 2026-08-15 operator decisions: **(1)** drop STL entirely — the CAD pipeline becomes STEP-only; **(2)** then build machining **toolpaths**; **(3)** this reaches into the storefront (`ABERP-site`) upload validation.

This section states only what was **verified against the code in this tree**, with file:line. Several widely-repeated beliefs about this pipeline turned out to be wrong, and two of them are load-bearing.

### C1 — What the extractor actually produces

`aberp-cad-extract` emits a **scalar** FeatureGraph: bounding box, volume, surface area, material grade, plus a `features[]` list of `{feature_type, count, representative_size_mm}` (`python/aberp-cad-extract/aberp_cad_extract/feature_graph.py:44-87`). There are **no coordinates, no axes, no depths** anywhere in the schema.

Worse: **both** extractors emit `features: []` unconditionally in production. The STL path returns `features or []` with production callers passing `None` (`extractors/stl.py:114`), and the STEP path does the same (`extractors/step.py:201`). The `features` argument exists only for test injection. So `feature_machining_minutes` in the engine is **always 0.0** today — which the engine says out loud (`crates/aberp-quote-engine/src/engine.rs:762-766`).

Machining time today is therefore entirely:

```
roughing_min  = (stock_volume − part_volume)/1000 · machining_difficulty / mrr_rough_ref   (engine.rs:811-817)
finishing_min = surface_area_cm2 · t_finish_min_per_cm2 · machining_difficulty             (engine.rs:841-842)
machining_minutes_base = roughing_min + finishing_min + 0.0                                (engine.rs:851)
```

A part with forty M6 holes and a part with none price **identically** if their volume and area match. That is the gap Parts B and C close.

### C2 — The schema version is 5, not 2 (correction to the brief)

The prior reconcile said "FeatureGraph v2". That is true of the **Python** side only. In fact there are **three different versions live in three places**:

| Site | Value | Reference |
|---|---|---|
| Rust engine `FeatureGraph::SCHEMA_VERSION` | **5** | `crates/aberp-quote-engine/src/feature_graph.rs:546` |
| Python `SCHEMA_VERSION` | **2** | `python/aberp-cad-extract/aberp_cad_extract/feature_graph.py:24` |
| Wrapper `EXPECTED_SCHEMA_VERSION` | **2** | `crates/aberp-cad-extract-wrapper/src/lib.rs:98` |

So the located-hole extension is **v6**, not "v3". Every reference to "FeatureGraph v3" in the brief means v6 here.

### C3 — The wrapper's version guard is exact-equality, and its own docs are wrong

`feature_graph.rs:533-545` states four times that "the version guard accepts any `schema_version <= N`". **It does not.** The only guard on the wrapper path is:

```rust
if graph.schema_version != EXPECTED_SCHEMA_VERSION {   // lib.rs:228 — EXACT equality, against 2
    return Err(ExtractError::SchemaVersionMismatch { .. });
}
```

The `<=` guard the docs describe exists **only** in the engine (`engine.rs:583`), which is downstream of the wrapper and never reached on the daemon path when the wrapper rejects first.

**Consequence, and it is the single highest-risk item in this ADR:** bumping the Python `SCHEMA_VERSION` from 2 to 6 without changing `lib.rs:228` **bricks every quote in the pipeline** with a `SchemaVersionMismatch` that `classify_failure` marks **Permanent** (`quote_pricing_pipeline.rs:2893`) — i.e. no auto-retry, every in-flight quote parked until an operator clicks Retry on each one, after a rebuild. This is a silent-until-deploy trap. Fixing it is **Part B's first commit**, before any schema work.

### C4 — v3/v4/v5 fields exist in Rust and nothing populates them

`stock_form` (v3, ADR-0094 Gap 1), `gears` (v4, ADR-0094 Gap 3), `tolerance` + `critical_feature_tolerances` (v5, ADR-0097) are all `#[serde(default)]` on the Rust struct (`feature_graph.rs:494`, `:501`, `:513`, `:521`) and are **absent from the Python schema entirely** — the Pydantic model is `extra="forbid"` with only the v2 fields (`feature_graph.py:68-87`).

The pricing effect is concrete and one-directional:

- `stock_form` defaults to `RectangularBlock` ⇒ a turned Ø-bar part is billed on `bx·by·bz` instead of `π/4·d²·L`, i.e. **~27 % over** on material *and* it "roughs away" four corners that were never bought (`engine.rs:811`). Turned parts are **over**-priced.
- A `Tube` blank is billed with its bore as solid metal — again **over**-priced.
- `gears` defaults to empty ⇒ tooth generation costs **zero**. Geared parts are **under**-priced, and this is the one that loses money.

So the net "silently under-prices turned/tubular/geared parts" framing is half right: geared parts are under-priced (dangerous), turned/tubular are over-priced (uncompetitive). Both are wrong; only one bleeds cash.

The workstream that closes this is **not** the located-hole work — see **B0** below.

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
| 11 | `crates/.../tests/error_paths.rs` ×5, `schema_version.rs` ×1 | `.stl` files as **carriers only** — every one uses a stub module (`with_module`), the file is never parsed |
| 12 | `python/.../tests/test_stl_extractor.py`, `test_cli.py:26,42,111`, `conftest.py` | STL unit + CLI tests, `cube_stl_path` fixture, `_cube_mesh` writer |

### C6 — The storefront allow-list is wider than the extractor, and the mismatch is already handled

`classify_failure` documents the situation verbatim (`quote_pricing_pipeline.rs:2860-2866`): *"the storefront accepts 11 CAD formats but the Python dispatcher only routes `.stl`/`.step`/`.stp`. Anything else (`.iges`, `.dxf`, `.sldprt`, …) raises `ValueError` with the literal 'Unsupported file extension'"* → classified **Permanent** (`:2918`).

Two things follow:

1. The in-tree count is **11**, not 13. The authoritative list lives in `ABERP-site`, which is **not on this machine** — the 11 is what `apps/aberp` believes, not what the storefront enforces. Treat both numbers as unverified until the repo is available.
2. **The rejection mechanism for STL already exists and is free.** If the STEP-only rejection message keeps the literal substring `Unsupported file extension`, it inherits Permanent classification with **no change to `classify_failure`**. This is the cheapest correct path and Part A takes it.

### C7 — Seven real STL quotes exist in this tree

`quote-artifacts/` holds 7 quote directories, **all STL**, 4 of them with an already-issued `priced.pdf`:

```
210308f6…/GearSppinners100.stl      96fffdb2…/pump_adapter.stl   + priced.pdf
22384e8c…/GearSppinners100.stl      9dec4b87…/poolbasket_V1.stl  + priced.pdf
852ba33d…/GearSppinners100.stl      d3d9e1db…/poolbasket_V1.stl  + priced.pdf
                                    e7e09974…/GearSppiners60.stl + priced.pdf
```

These are **real customer quotes with PDFs that went out**. Their blast radius under a STEP-only pipeline is analysed in *Part A — What breaks*.

### C8 — OCCT is already wired, already used, and already optional

`extractors/step.py` is a working OCCT/OCP extractor: unit normalisation to MM (`:104`), `STEPControl_Reader` (`:105-111`), solid counting (`:118-125`), `BRepBndLib.AddOptimal_s` bbox (`:169`), `BRepGProp.VolumeProperties_s` (`:178`), `BRepGProp.SurfaceProperties_s` (`:190`), plus an OS-level fd-1 silencer so OCCT's C++ progress bytes don't corrupt the JSON on stdout (`:59-79`). It is gated behind the optional `[step]` extra (`pyproject.toml:25-29`, ~63 MB wheel). **Part B needs no new dependency and no new process model** — it adds a face-walk to a reader that already runs.

---

## Decision

Four workstreams, sequenced. **A → B0 → B → C** are the value path; **D** is a separate, much larger programme that is scoped here and not designed.

Every schema and catalogue change follows the ADR-0094/0097 **inert-by-default** discipline that this codebase has now used three times: `#[serde(default)]` new fields, empty catalogue slice ⇒ zero contribution ⇒ **byte-identical** `QuoteBreakdown` *and* `reasoning_log`, so every existing golden, determinism and property test stays green without edit.

---

## PART A — Drop STL; STEP-only contract

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

Three properties, all deliberate:

1. **The literal `Unsupported file extension` is preserved**, so `classify_failure` (`quote_pricing_pipeline.rs:2918`) marks it **Permanent** with **zero Rust change**. No auto-retry storm; the operator sees one clear failure.
2. **STL gets its own branch and its own message** — the customer is told *why* and *what to do*, not just "unsupported". This is the operator-facing text; it will be surfaced bilingually in the SPA per house convention.
3. The generic branch stays for `.iges`/`.dxf`/`.sldprt`/…, unchanged in behaviour.

The structured stderr shape is unchanged (`cli.py:25-27`): `{"error": {"stage": "input", "message": "…"}}`, exit **2**. No new error taxonomy, no new wrapper variant, no new classifier rule.

### A.3 Removals and edits (file-level plan)

| Action | Target |
|---|---|
| **Delete** | `python/aberp-cad-extract/aberp_cad_extract/extractors/stl.py` |
| **Delete** | `python/aberp-cad-extract/aberp_cad_extract/tests/test_stl_extractor.py` |
| **Delete** | `crates/aberp-cad-extract-wrapper/tests/extract_smoke.rs` (superseded by `step_extract_smoke.rs`) |
| **Edit** | `extractors/__init__.py` — export `extract_step`; delete the stale "step is a stub" docstring |
| **Edit** | `cli.py:4,29-37,43` — route table, usage line, `--help` description |
| **Edit** | `pyproject.toml:8,15` — drop `numpy-stl`; rewrite description. **Keep `numpy`** (`heuristics.py` uses it) |
| **Edit** | `quote_pricing_pipeline.rs:621-626` — CAD picker drops `.stl`; comment updated |
| **Edit** | `wrapper/src/lib.rs:250-253` — `ExtractRequest` doc: STEP-only |
| **Edit** | `cad_blob.rs:15,338-339` — extension-preservation rationale is now "the extractor requires a `.step`/`.stp` suffix" |
| **Edit** | `tests/common/mod.rs` — retire `write_cube_stl`; add `copy_step_fixture()` returning a path to `unit_cube.step` |
| **Edit** | `tests/error_paths.rs` (×5), `tests/schema_version.rs` (×1) — carrier files `.stl` → `.step`. **Behaviourally inert** (all use stub modules; the file is never parsed) but required for honesty |
| **Edit** | `python/.../tests/conftest.py` — retire `_cube_mesh`/`cube_stl_path`; add a `step_fixture_path` fixture pointing at the committed `unit_cube.step` |
| **Edit** | `python/.../tests/test_cli.py:26,42,111` — port to the STEP fixture |
| **Add** | `python/.../tests/test_cli.py` — new test: `.stl` input exits **2**, stderr contains `Unsupported file extension` **and** `STEP` |
| **Add** | `apps/aberp/tests/` — pin that `classify_failure("extract", <the STL message>) == Permanent`. This is the regression that protects the free-classification trick in A.2 |

### A.4 What breaks

**(a) The seven stored STL quotes (C7) — the honest answer is "mostly fine, with one sharp edge."**

Re-pricing does **not** re-extract. `get_job_artifacts` reads the persisted `feature_graph_json` column (`quote_pricing_jobs.rs:1230`), and the engine accepts any `schema_version <= FeatureGraph::SCHEMA_VERSION` (`engine.rs:583`). A stored v2 graph from an STL keeps deserialising and keeps re-pricing forever. Issued PDFs are unaffected.

The sharp edge: a job sitting in `Fetched` or `Extracting` state, or an operator clicking **Retry** on a Failed STL job, routes through `advance_extract` → `CadExtractor::extract` (`quote_pricing_pipeline.rs:1078-1083`) and **will now fail Permanent**. That is the correct outcome — but it must be *communicated*, not discovered.

**Required before merge:** an operator-run inventory of live jobs whose `cad_filename` ends `.stl` and whose `state` is not terminal. This is a read-only query against the live tenant DB and is an **operator action** (this ADR is read-only on running systems):

```sql
SELECT quote_id, state, cad_filename, failure_kind
  FROM quote_pricing_jobs
 WHERE lower(cad_filename) LIKE '%.stl'
   AND state NOT IN ('Posted', 'Failed');
```

If that returns rows, they must be drained (priced and posted) **before** the STEP-only build ships, or the customers re-upload as STEP. See Q1.

**(b) `real_part_gearspinner60_validation.rs` survives, but loses reproducibility.** Its golden graph is a hard-coded literal described as what "the frozen-prod schema_v2 extractor produced for this STL" (`:206`). The test compiles and passes untouched. What is lost is the ability to **regenerate** it from `GearSppiners60.stl`. Mitigation: convert the STL to STEP once (offline, one-time), commit the STEP alongside, and add a comment recording that the golden's provenance is a pre-ADR-0112 STL run. Do **not** re-derive the numbers — that would silently move a validation baseline.

**(c) CI gets strictly heavier, and this is the real cost of Part A.** Today `extract_smoke` proves the whole Python↔Rust wire **without OCCT** — plain `pip install -e .` and numpy-stl. After the drop, **every** end-to-end extractor test requires the ~63 MB `cadquery-ocp` wheel plus `vtk`. There is no longer a light lane. Accepted deliberately (a light lane that exercises a code path we are deleting is worse than no light lane), but it must be planned: the CI image needs `pip install -e '.[step,dev]'` seeded and cached, and `ABERP_TEST_PYTHON` pointed at it (`step_extract_smoke.rs:9-11`).

**(d) Nothing in `aberp-quote-engine` changes.** The engine has never known what a file format is. Its only STL mentions are comments (`breakdown.rs:76`, `engine.rs:762-763`), which get a wording pass.

### A.5 Storefront (ABERP-site) — CONTRACT ONLY

> **⚠ `ABERP-site` is NOT on this machine.** Nothing in A.5 can be implemented from this repo. It is a contract for the storefront maintainer, and the format list below is derived from what `apps/aberp` *believes* (`quote_pricing_pipeline.rs:2860-2866`), **not** from reading the storefront's own sniffer. The real list must be read from the repo before any of this is coded. See Q2.

**S-1 — Accepted formats become exactly two.** `.step`, `.stp`. Both extension **and** content sniff must agree; extension alone is not sufficient (a `.step` file holding an ASCII STL mesh must be rejected at upload, not at extraction). A STEP file's first non-blank line begins `ISO-10303-21;` — that is the sniff.

**S-2 — Everything else is rejected at upload time,** with a hard distinction between two classes:

- **STL** (`.stl`; ASCII `solid ` prefix, or an 80-byte header + `uint32` triangle count matching file length) → its **own** error code and message. STL is the format customers are most likely to have, so a generic "unsupported" here is a conversion-killer.
- **All other CAD** (`.iges`, `.igs`, `.dxf`, `.dwg`, `.sldprt`, `.ipt`, `.3mf`, `.obj`, `.x_t`, `.sat`, …) → generic unsupported-format error.

**S-3 — Error shape.** Mirrors the extractor's `{"error": {"stage", "message"}}` envelope, extended with a machine-readable code so the storefront UI can branch without string-matching:

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

**S-4 — The storefront must never accept a file ABERP will reject.** The allow-list is now a **subset relationship, enforced in one direction**: storefront-accepted ⊆ extractor-accepted. The current 11-vs-3 mismatch (C6) is the bug this closes permanently. The `classify_failure` `unsupported file extension` rule stays as defence-in-depth, not as the primary gate.

**S-5 — Existing storefront-side STL quotes.** Unknown from here. The storefront may hold quote records referencing STL uploads that ABERP has never fetched. Determining that needs the repo. See Q2.

**S-6 — Ordering.** Ship the storefront tightening **first or simultaneously**. If ABERP tightens first, customers upload STL successfully and then get a silent Permanent failure hours later — the worst possible ordering.

### A.6 Effort — Part A

| | |
|---|---|
| **Extractor + wrapper + pipeline + tests (this repo)** | **S** — ~1 focused day. Mostly deletion. The only thinking is the carrier-file sweep and the classifier regression pin. |
| **CI image rework (OCP seeded, cached, `ABERP_TEST_PYTHON` wired)** | **S–M** — half a day if the CI lane is already parameterised; a full day if the OCP wheel needs caching from scratch. |
| **Storefront (ABERP-site)** | **UNSIZEABLE from here.** Reads as S once the repo is available (sniffer table + two error codes + copy), but that estimate is worth nothing until someone opens the file. |

---

## PART B0 — Extractor catch-up to v3/v4/v5 (**separate from B**)

### B0.1 The decision: this is its own workstream, not part of the located-hole cut

The C4 gap — `stock_form`, `gears`, `tolerance` unpopulated — is **deliberately excluded from Part B** and named here as its own piece of work:

> **B0 — "extractor schema catch-up": bring the Python extractor from v2 to v5, populating `stock_form` and (operator-supplied) `gears` and `tolerance`.**

Four reasons to split it, in order of weight:

1. **It is not the same kind of problem.** Located holes are a *geometry mining* problem (walk B-rep faces, classify cylinders). `stock_form` is a *rotational-symmetry classification* problem, and `gears`/`tolerance` are not extractable from geometry at all — a STEP file does not say "AGMA 10, module 2". ADR-0094 already decided their source is **operator or storefront input**, with a CAD hint as a fallback (ADR-0094 Gap 1 part B precedence list). Different inputs, different UI, different tests.
2. **It moves prices immediately; B does not.** Populating `stock_form` changes the price of every turned part on the next re-quote. That needs its own before/after validation against the ADR-0094 goldens and its own operator communication. Bundling it with a schema extension makes the diff impossible to review for price impact.
3. **It unblocks money sooner.** Geared parts are under-priced *today* (C4). B0's `gears` wiring is a bigger commercial win than anything in B or C and does not depend on either.
4. **The versioning works out cleanly.** B0 takes Python 2 → 5 with **no Rust schema change at all** (the fields already exist). B then takes 5 → 6. Two independent, independently-revertable steps.

**B0 is not designed in this ADR.** It needs its own ADR because the `gears`/`tolerance` input path is an SPA + storefront question, not a geometry question. Recommended: **ADR-0113**.

**Sequencing:** B0 and B are independent after the C3 wrapper fix lands. Do the wrapper fix once, then B0 and B in either order. If forced to pick: **B0 first** (it stops the bleeding).

---

## PART B — FeatureGraph v6: located holes from STEP via OCCT

### B.1 First commit: fix the version guard (C3) — **before any schema work**

```rust
// crates/aberp-cad-extract-wrapper/src/lib.rs
/// The newest Python-side `_schema_version` this build understands.
pub const EXPECTED_SCHEMA_VERSION: u32 = 6;
/// The oldest still-accepted version. Graphs below this predate the
/// fields the engine now requires and must be re-extracted.
pub const MIN_SCHEMA_VERSION: u32 = 2;

if !(MIN_SCHEMA_VERSION..=EXPECTED_SCHEMA_VERSION).contains(&graph.schema_version) {
    return Err(ExtractError::SchemaVersionMismatch {
        expected: EXPECTED_SCHEMA_VERSION,
        got: graph.schema_version,
    });
}
```

Range, not equality — matching what the engine already does (`engine.rs:583`) and what the Rust docs already (wrongly) claim. Ship this **on its own**, with the `feature_graph.rs:533-545` doc corrected in the same diff, and a test that a v2 graph and a v6 graph both pass while v1 and v7 both fail. Until this lands, **any** Python version bump is a production outage.

### B.2 The schema addition (additive, versioned)

```python
# python/aberp-cad-extract/aberp_cad_extract/feature_graph.py
SCHEMA_VERSION: int = 6

class HoleEndCondition(str, Enum):
    THROUGH = "through"
    BLIND   = "blind"
    UNKNOWN = "unknown"   # topology was ambiguous — never guessed silently

class LocatedHole(BaseModel):
    model_config = ConfigDict(extra="forbid")
    diameter_mm: float          = Field(gt=0.0)
    depth_mm: float             = Field(gt=0.0)
    axis_unit: List[float]      = Field(min_length=3, max_length=3)  # unit vector
    entry_point_mm: List[float] = Field(min_length=3, max_length=3)  # XYZ of the mouth
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

- **`skip_serializing_if = "Vec::is_empty"`** — an empty vector emits **no JSON key**, so a re-serialised v2 graph is byte-identical to what it was. This is exactly the `tolerance` / `critical_feature_tolerances` posture (`feature_graph.rs:513,521`) and it is what keeps `feature_graph_hash` stable for stored graphs (`quote_pricing_pipeline.rs:1085-1088` blake3-hashes the canonical encoding).
- **Separate `located_holes` field, not a richer `Feature`.** `Feature` is a locked wire contract with a golden (`tests/property.rs`); adding fields to it would force `#[serde(default)]` on a struct whose whole point is that all three fields are mandatory. A parallel vector is the same choice ADR-0097 made for `critical_feature_tolerances` (`feature_graph.rs:271-278`) and for the same reason.
- **`axis_unit` normalised by the extractor, not the engine.** The engine "does not second-guess the extractor" (`feature_graph.rs:300-303`). The extractor normalises; the engine may assert.
- **`end_condition: Unknown` is a first-class value, never a silent default to `Through`.** A blind hole mis-read as through under-counts peck cycles and over-counts nothing — it under-prices. Unknown is priced conservatively (see C.3) and reasoning-logged.
- **Millimetres and the part's own coordinate system.** No WCS, no fixture offset — the extractor has no idea where the part sits on a table. Everything is in the STEP file's own frame, already unit-normalised to MM at read (`extractors/step.py:104`).
- **What is deliberately NOT in v6:** counterbores, countersinks, threads, tapped-vs-clearance, tool-access/collision, and hole *groups*. Threads are not reliably recoverable from B-rep without semantic PMI, and guessing "M6 because Ø5.0" is exactly the silent-wrong-value class this codebase refuses (`extractors/step.py:91-98`). Deferred; see Q5.

### B.3 Back-compat for stored v2 graphs

Three layers, all already precedented:

1. **Wrapper**: range guard from B.1 accepts 2..=6.
2. **Engine**: `schema_version > SCHEMA_VERSION` is the only rejection (`engine.rs:583`) — a v2 graph passes and `located_holes` deserialises to `vec![]`.
3. **Pricing**: empty `located_holes` ⇒ the Part C drilling path is never entered ⇒ `drilling_minutes = 0.0`, **no reasoning-log line**, breakdown byte-identical. Every stored graph keeps re-pricing at exactly its historical number.

**The hash caveat, stated plainly:** a v2 graph *re-extracted* from the same STEP under a v6 extractor produces a different `feature_graph_hash` (`quote_pricing_pipeline.rs:1085`) — because it now carries holes. That is correct and desirable (the geometry knowledge genuinely changed), but any code that treats hash equality as "same part" must not treat hash inequality as "different part". Verified: nothing currently does.

### B.4 How OCCT is invoked

No new dependency. The same `[step]` extra, the same `_load_step_shape()` reader (`extractors/step.py:82-115`), one new face-walk inside the existing `_silence_stdout_fd()` discipline.

New module: **`python/aberp-cad-extract/aberp_cad_extract/holes.py`**, imported by `extractors/step.py` and guarded by the same `_OCP_AVAILABLE` flag.

```
extract_step(path, material_grade)                    # extractors/step.py — existing
  └── shape = _load_step_shape(path)                  # existing, unchanged
  └── located_holes = mine_cylindrical_holes(shape)   # NEW → holes.py
```

Algorithm sketch (OCCT calls named for review; exact API pinned at implementation):

1. `TopExp_Explorer(shape, TopAbs_FACE)` — walk every face.
2. `BRepAdaptor_Surface(face)`; keep `GetType() == GeomAbs_Cylinder`. Discard convex-outward cylinders (that is the bar OD, not a hole) by testing the face orientation against the surface normal — **this test is the main correctness risk in B and needs its own fixture**.
3. `adaptor.Cylinder()` → `gp_Cylinder`: `.Axis()` gives position + direction, `.Radius()` gives `diameter_mm = 2·r`.
4. `BRepTools.UVBounds_s(face)` → the `V` span is the axial length ⇒ `depth_mm`. The `U` span tells you how much of the circumference the face covers: a full `2π` is a real hole; a partial sweep is a fillet, a slot end, or a split cylinder.
5. **Merge coaxial faces.** A single drilled hole is frequently several faces (split at a seam, or stepped). Group by (axis direction within angular tolerance, axis line within positional tolerance), then union the axial spans. Without this step a Ø8 through-hole reports as two Ø8 holes — a **2× drilling over-price**. This is the second main correctness risk.
6. **`entry_point_mm`** = the axial extremum of the merged span nearest the part's outer boundary along the axis.
7. **`end_condition`**: `Through` if the cylinder's axial span reaches the solid's boundary at both ends (classify both endpoints with `BRepClass3d_SolidClassifier` just outside the span); `Blind` if one end terminates inside; `Unknown` if the classifier is ambiguous or the merge was uncertain.
8. **`flat_bottom`**: a planar face perpendicular to the axis capping a blind cylinder ⇒ `true` (end-mill/flat-bottom drill, different cycle) — otherwise `false`.
9. Sort deterministically (by `entry_point_mm` lexicographic, then diameter) before emitting. **Non-negotiable:** OCCT explorer order is not contractually stable across versions, and the whole `feature_graph_hash` + golden-test regime depends on byte-identical output for identical input.

**Failure posture:** hole mining must **never** fail the extraction. Any exception inside `mine_cylindrical_holes` is caught, `located_holes` comes back empty, and a diagnostic is emitted — a part still prices at today's (hole-blind) number rather than the quote dying. This is the opposite of `extract_step`'s posture on *shape* errors (where failing loud is right, because the geometry is unusable). Justification: an empty hole list is a **known-conservative** degradation with an established meaning; a corrupt hole list is not.

### B.5 Where it lands

| File | Change |
|---|---|
| `crates/aberp-cad-extract-wrapper/src/lib.rs` | B.1 range guard; `EXPECTED_SCHEMA_VERSION` 2 → 6 |
| `crates/aberp-quote-engine/src/feature_graph.rs` | `HoleEndCondition`, `LocatedHole`, `FeatureGraph::located_holes`; `SCHEMA_VERSION` 5 → 6; correct the `<=` doc lie |
| `crates/aberp-quote-engine/src/lib.rs` | re-export the two new types |
| `python/.../feature_graph.py` | `HoleEndCondition`, `LocatedHole`, `FeatureGraph.located_holes`; `SCHEMA_VERSION` 2 → 6 |
| `python/.../holes.py` | **new** — the face-walk |
| `python/.../extractors/step.py` | call `mine_cylindrical_holes`, guarded |
| `python/.../tests/fixtures/` | **new** STEP fixtures: `plate_4_through_holes.step`, `blind_hole_flat_bottom.step`, `stepped_bore.step`, `coaxial_split_faces.step`, `tube_od_not_a_hole.step` |
| `python/.../tests/test_holes.py` | **new** — exact expected `LocatedHole` lists |
| `crates/.../tests/feature_graph_roundtrip.rs` | v6 round-trip: Python-emitted JSON → Rust struct, no loss |
| `crates/.../tests/schema_version.rs` | extend: v2 accepted, v6 accepted, v1/v7 rejected |

### B.6 Effort — Part B

| | |
|---|---|
| **B.1 wrapper range guard** | **XS** — under an hour, but it gates everything |
| **B.2 schema (both sides) + round-trip tests** | **S** — one day. Mechanical, well-precedented three times over |
| **B.4 OCCT face-walk + coaxial merge + end-condition** | **M** — 3–5 days. The algorithm is a day; the fixtures and the two correctness risks (convex-outward rejection, coaxial merge) are the rest |
| **Fixture authoring** | **S–M** — needs a CAD seat to author five known-geometry STEP parts. Bounded but not free; see Q6 |
| **Part B total** | **M** (~1 week) |

---

## PART C — Drilling cycle-time pricing

Pure arithmetic once B lands, and structurally a clone of ADR-0094 Gap 2 / ADR-0097 T3. **Do not start before B.**

### C.1 The rate table

New catalogue row type, mirroring `MachineRate` / `GearProcessRate` / `ToleranceCostRate` (`catalogue.rs:191-286`) exactly:

```rust
// crates/aberp-quote-engine/src/catalogue.rs
/// A row from `quoting_drilling_rates` (ADR-0112 Part C). Keyed by the
/// material's machining group. An EMPTY slice ⇒ the drilling path is never
/// entered ⇒ drilling_minutes = 0.0, NO reasoning line, breakdown
/// byte-identical to pre-ADR-0112.
pub struct DrillingRate {
    /// Material machining group this row prices (joins the material catalogue).
    pub material_group: String,
    /// Cutting feed, mm/min per mm of drill diameter. Actual feed for a Ø d
    /// hole = feed_mm_per_min_per_mm_dia · d.
    pub feed_mm_per_min_per_mm_dia: f64,
    /// Peck depth as a multiple of diameter. Depth > this · d ⇒ peck cycle.
    pub peck_depth_dia_multiple: f64,
    /// Seconds lost per peck retract-and-return.
    pub peck_retract_sec: f64,
    /// Rapid approach + retract seconds charged once per hole.
    pub rapid_per_hole_sec: f64,
    /// Tool change seconds, charged once per DISTINCT diameter on the part.
    pub tool_change_sec: f64,
    /// Multiplier for a blind flat-bottom hole (slower plunge, no break-through).
    pub flat_bottom_factor: f64,
    /// Multiplier applied when end_condition is Unknown — the conservative
    /// branch. >= 1.0.
    pub unknown_end_condition_factor: f64,
}
```

Wiring module **`apps/aberp/src/quoting_drilling_rates.rs`**, a direct structural clone of `quoting_machine_rates.rs` (575 lines): prefixed ULID `qdr_<ULID>`, lazy `CREATE TABLE IF NOT EXISTS quoting_drilling_rates`, invariants in code not SQL CHECK (`[[no-sql-specific]]`), CRUD auditing via `EventKind::ParametersChanged` with a self-describing `"catalogue":"quoting_drilling_rates"` payload (the same blast-radius reasoning as `quoting_machine_rates.rs:22-31` — `EventKind` has ~186 variants and is not `#[non_exhaustive]`).

**Seeded zero-contribution**, per ADR-0097 Q6: boot seeds one row per material group with all coefficients at values that produce zero minutes, so the CRUD has rows to edit but **nothing moves until Ervin tunes them**. Pricing after the Part C merge is byte-identical until an operator deliberately changes a number.

### C.2 The formula

Per hole `h` in `located_holes`, with `d = h.diameter_mm`, `L = h.depth_mm`:

```
feed_mm_per_min = feed_mm_per_min_per_mm_dia · d
cut_min         = L / feed_mm_per_min · machining_difficulty
peck_count      = max(0, ceil(L / (peck_depth_dia_multiple · d)) − 1)
peck_min        = peck_count · peck_retract_sec / 60
rapid_min       = rapid_per_hole_sec / 60
hole_min        = (cut_min + peck_min + rapid_min) · end_factor(h)

  where end_factor = flat_bottom_factor          if h.flat_bottom
                     unknown_end_condition_factor if h.end_condition == Unknown
                     1.0                          otherwise

tool_change_min = distinct_diameters(located_holes) · tool_change_sec / 60
drilling_minutes = Σ hole_min + tool_change_min
```

Notes: `machining_difficulty` multiplies the **cut** term only — pecking and rapids are machine-kinematic, not material-dependent. `distinct_diameters` is counted after rounding to 0.01 mm so float noise cannot invent a tool change. Every term gets its own reasoning-log line, per-hole, in the established `[machining] …` format (`engine.rs:818-849`) — the log is the trust signal.

### C.3 Where it enters the engine

`engine.rs:851`:

```rust
let machining_minutes_base = roughing_min + finishing_min + feature_machining_minutes;
//                                                        + drilling_min   ← ADR-0112 Part C
```

Critically, it lands **before** the S429 calibration scaling (`engine.rs:882-895`) and before machining cost (`engine.rs:909+`), so drilling minutes flow into the calibration coefficient, machining cost, subtotal, overhead, margin, total, **and** the lead-time / machine-capacity projection with no extra plumbing. That is the whole reason to put it here rather than as a separate cost line.

**Double-count guard.** If B0 or a later cut ever populates `features[]` with `FeatureType::Hole`, holes would be charged twice — once through `feature_machining_minutes` (`engine.rs:782`) and once through `drilling_minutes`. Rule, enforced in the engine with a loud reasoning line: **when `located_holes` is non-empty, `FeatureType::Hole` rows in `features[]` contribute complexity/inspection but NOT machining minutes.** Located geometry wins over counted geometry. Pinned by a test.

### C.4 Where it lands

| File | Change |
|---|---|
| `crates/aberp-quote-engine/src/catalogue.rs` | `DrillingRate` |
| `crates/aberp-quote-engine/src/engine.rs` | `CatalogueSnapshot.drilling_rates` (7th slice); drilling block; double-count guard; log lines |
| `crates/aberp-quote-engine/src/lib.rs` | re-export |
| `apps/aberp/src/quoting_drilling_rates.rs` | **new** — clone of `quoting_machine_rates.rs` |
| `apps/aberp/src/lib.rs` | `pub mod quoting_drilling_rates;` |
| `apps/aberp/src/serve.rs` | `ensure_schema` + `seed_*_if_absent` at boot (≈`:1580-1593`); REST handlers (≈`:4547`) |
| `apps/aberp/src/quote_pricing_pipeline.rs:1338` | load rows into the snapshot |
| `apps/aberp-ui/` | Quoting-tunables SPA tab, mirroring the tolerance-cost-rates tab |
| `crates/aberp-quote-engine/tests/drilling_cost.rs` | **new** — modelled on `tests/tolerance_cost.rs` |
| existing goldens | **untouched** — empty slice ⇒ byte-identical |

### C.5 Effort — Part C

**S–M, ~2–3 days**, and genuinely small *only because* B delivered located holes. Engine math + tests ≈ 1 day; the `quoting_drilling_rates` wiring module is a mechanical clone ≈ 1 day; SPA tab ≈ 0.5 day. The single piece of real thinking is the double-count guard.

**Prerequisite that is not code:** the seed coefficients need real numbers from Ervin — feeds per material group, peck policy, tool-change time on the actual machines. Zero-contribution seeds mean the feature ships inert and stays inert until those arrive. See Q7.

---

## PART D — Toolpath roadmap (SKETCH — deliberately not designed)

> **This is a separate programme, not a phase of the above.** Everything A–C is *pricing*: wrong output costs money. Part D *moves a spindle*: wrong output destroys a machine, a fixture, or a person. The liability class is different and the engineering discipline must be different. **Nothing in this section is a design.** It is a scoping note so the effort is not mistaken for "one more cut after C."

ADR-0014 remains the standing stub (Proposed, 2026-05-19) and it **explicitly excludes CAM post-processor logic** from its scope. Part D is what would supersede it. **Posture:** in-house, vertically integrated, no vendor CAM lock-in — consistent with the rest of this system. That posture is a multi-year commitment, and it should be entered knowingly.

### D.1 The path

```
FeatureGraph v6 (located holes)
   → drilling cycles          ← the ONLY toolpath primitive that is nearly free after B+C
   → 2.5D pocket recognition  ← planar-floor + vertical-wall face groups from the same OCCT walk
   → 2.5D contour + pocket clearing toolpaths (offset / trochoidal)
   → tool library + feeds/speeds per material                        ← data, not code, and it is a lot of data
   → WCS + fixturing + stock model + setup planning                  ← where "software" ends and "shop" begins
   → collision / gouge checking against stock + fixture + holder     ← the hard one
   → simulation + verification (material removal sim, dry-run proof)
   → per-controller post-processors (Fanuc / Haas / Siemens / Heidenhain / LinuxCNC / …)
   → G-code artifact, versioned + audited per ADR-0014
```

### D.2 Phases

**D-0 — Drilling G-code only (minimum verifiable).** Located holes → canned cycles (`G81`/`G83`/`G73`) on a **single, flat, already-fixtured** face, one controller dialect, **operator-verified in dry-run before every use**. Verifiable in the sense that a human can read the output and confirm it. **Not shippable unattended.** Small — days — precisely because it does almost nothing.

**D-1 — 2.5D pockets and contours.** Pocket recognition from B-rep, offset-based clearing, tool selection from a real library, stock model tracking between operations. This is where a real CAM kernel starts. Months.

**D-2 — Verification.** Material-removal simulation, gouge and collision detection against stock + fixture + holder + machine envelope. **This is the gate between "produces G-code" and "produces G-code you may run."** Nothing before D-2 is shippable to an unattended machine. Months, and the hardest single item on the list.

**D-3 — Post-processor framework.** Per-controller dialect: canned-cycle syntax, tool-change protocol, coolant, work offsets, safety blocks, arc conventions (`R` vs `IJK`), retract modes (`G98`/`G99`), high-speed look-ahead. Every controller is its own dialect and **every one needs verification on the actual machine.** Genuinely per-machine, not per-brand — two Haas mills with different option packages are two post-processors.

**D-4 — Fixturing and setup planning.** Multi-op, work-holding, part orientation, WCS assignment. This is the point where the software must model the *shop*, not the *part*. Arguably never fully automatable; the realistic target is operator-assisted.

**Minimum that produces a verifiable toolpath:** D-0.
**Minimum that is shippable:** D-0 + D-2 + one dialect of D-3, machine-verified. There is no shortcut around D-2.

### D.3 The hard parts, named honestly

1. **Collision / gouge avoidance.** Not a feature — the entire correctness problem. Needs the tool, the holder, the spindle nose, the fixture, the stock at every intermediate state, and the machine envelope. A false negative crashes a spindle.
2. **Tool library.** Real geometry, real feeds/speeds per material/coating/diameter/stickout. This is a **data acquisition** problem, and its absence silently produces plausible-looking G-code that breaks tools.
3. **WCS / fixturing / setup.** The gap between "the part" and "the part clamped in a vise at a known offset" is where most CAM output goes wrong.
4. **Post-processor per controller dialect.** Combinatorial, and only ever validated on iron.
5. **Liability.** A pricing bug sends a wrong invoice. A toolpath bug sends a tool into a fixture at rapid. This changes the required test discipline, the required sign-off, and probably the insurance conversation. **It should be an explicit, written decision by Ervin — not an emergent consequence of C going well.**

### D.4 Effort — Part D

**XL / multi-quarter.** D-0 alone is days. D-1 through D-3 is a **product**, not a feature — comparable in scope to everything `aberp-quote-engine` has cost to date, with a harsher failure mode. Do not begin D-1 without a dedicated ADR (recommended: **ADR-0114**) that supersedes ADR-0014 and states the liability posture explicitly.

---

## Consequences

**Easier.** One CAD path, not two — every extractor test exercises the path production uses. Geometry becomes *located*, which is the precondition for both real drilling cost and any toolpath work. The rate-table pattern is now used a fourth time and is thoroughly proven. The C3 version-guard bug is found and fixed before it can fire.

**Harder.** Every CI lane now needs the ~63 MB OCP wheel — the light lane is gone. Customers with only STL must re-export, and some will not (a real conversion cost, quantified nowhere in this repo — see Q1). Two repos must ship in a coordinated order (A.5 S-6). `feature_graph_hash` changes on re-extraction of any part with holes.

**Locked in.** STEP as the single input format — reversing means restoring a deleted parser. B-rep face-walking as the feature-mining approach (the alternative, mesh-based recognition, is now unavailable by construction). OCCT/OCP as a hard runtime dependency of the extractor, not an optional extra. And, if Part D proceeds, an in-house CAM kernel — the deepest commitment in this document by an order of magnitude.

---

## Adversarial review

**1. "You are deleting a working parser for seven paying quotes' worth of format coverage."**
Accepted, with the mitigation in A.4(a): stored graphs keep re-pricing, issued PDFs are unaffected, and only in-flight/retry jobs break. The counter-argument for keeping STL is stronger than it looks — but it fails on Part D. A pipeline whose downstream stages assume located geometry cannot have an upstream stage that structurally cannot provide it; keeping STL means every future stage carries an "except for STL" branch forever. Delete it now, while the cost is seven files, not seven hundred.

**2. "The Python `SCHEMA_VERSION` bump will take down production."**
Correct as the code stands today — that is finding C3, and it is why B.1 is a standalone first commit with its own test, shipped before any schema change. If B.1 is skipped or merged out of order, the outage is certain, silent until deploy, and classified Permanent so it will not self-heal. **This is the single most important line in this ADR.**

**3. "Coaxial face merging is a heuristic and heuristics silently over-price."**
Correct, and it is named as one of two primary correctness risks in B.4. A single hole split into two faces is a 2× drilling over-price on that hole. Mitigations: a dedicated `coaxial_split_faces.step` fixture with a known-correct expected output; deterministic sort so failures are reproducible; and the reasoning log naming every merged group so an operator can see what the extractor believed. Not eliminated — *visible*.

**4. "Zero-contribution seeds mean Part C ships doing nothing."**
Yes, and deliberately — the ADR-0097 Q6 precedent. The alternative is inventing feed rates, which would move real prices based on numbers nobody measured. The feature ships inert; the first real number is Ervin's. The risk this accepts is that it can sit inert indefinitely and be mistaken for "done"; Q7 exists to force the conversation.

**5. "The storefront contract is written without reading the storefront."**
Stated explicitly and repeatedly in A.5. The format list, the count (11 vs 13), and the existence of storefront-side STL records are all **unverified**. A.5 is a specification to be validated against `ABERP-site`, not a description of it. This is the largest unknown in Part A and the reason A.5's effort is marked UNSIZEABLE rather than estimated.

**6. "Part D is a CAM company disguised as a feature."**
Agreed — which is why D is a sketch with an explicit "do not begin D-1 without ADR-0114" gate and an explicit liability paragraph. The in-house/no-vendor-lock posture is honoured, but honouring it in CAM is a multi-year commitment that should be entered by written decision, not by momentum from a successful Part C.

---

## Alternatives considered

**Keep STL as a degraded tier ("STL quotes but cannot be toolpathed").** Rejected: it puts a permanent conditional in every downstream stage and guarantees that the one part that reaches the toolpath generator without geometry is a surprise at the machine, not at upload.

**Convert STL → STEP automatically on ingest.** Rejected: mesh-to-B-rep reconstruction is lossy, slow, and produces a *plausible* solid with fictitious "holes". Precisely the silent-wrong-value failure class this codebase refuses (`extractors/step.py:91-98`).

**Put located holes inside `Feature` rather than a parallel vector.** Rejected: `Feature`'s three fields are mandatory by design and golden-locked; adding optional fields to it weakens a contract that is currently strong. ADR-0097 already made this call for `critical_feature_tolerances`.

**Charge drilling as a `ComplexityRule` on `FeatureType::Hole` instead of a cycle-time model.** Rejected: rules are per-size-bucket flat times and cannot express depth, pecking, or tool-change amortisation — the terms that actually dominate drilling. It would also collide with the located-hole path (C.3's double-count guard).

**Bundle B0 (stock-form/gear catch-up) into Part B.** Rejected for the four reasons in B0.1 — chiefly that B0 moves prices immediately and B does not, so bundling them makes the price-impact review impossible.

**Buy a CAM kernel for Part D.** Not rejected — deferred to ADR-0114. It contradicts the stated in-house posture, but it is the only alternative that meaningfully changes D's multi-quarter shape, and it deserves a real hearing rather than a reflexive no.

---

## Open questions

- **Q1 — (BLOCKING Part A) Are there live STL quotes in flight?** The `quote_pricing_jobs` query in A.4(a) must be run against each live tenant DB by an operator. If non-empty: drain before shipping, or accept that those quotes need customer re-uploads. **Ervin's confirmation that dropping STL is acceptable for already-stored STL quotes is required before Part A merges.**
- **Q2 — (BLOCKING A.5) Where is `ABERP-site`?** Repo location/clone. Needed to: confirm the real accepted-format list (11? 13? something else), find the sniffer, check for storefront-side STL quote records, and size the change. Until then A.5 is a specification, not a plan.
- **Q3 — Ordering of the two-repo ship.** A.5 S-6 recommends storefront-first-or-simultaneous. Confirm the release mechanism can do that, or accept a window where customers upload STL that will fail later.
- **Q4 — B0 as ADR-0113?** Confirm the extractor catch-up (`stock_form`/`gears`/`tolerance`) gets its own ADR and its own priority slot. Recommendation: **do B0 before B** — geared parts are under-priced today, which is the only part of C4 that loses money.
- **Q5 — Threads and counterbores in v6, or later?** Current design says later: neither is reliably recoverable from B-rep without PMI, and guessing "M6 from Ø5.0" is the silent-wrong-value class. If tapping is a material share of shop time, that judgement should be revisited — but with a **separate** operator-supplied input, not a geometric guess.
- **Q6 — Who authors the five STEP test fixtures?** Needs a CAD seat and known-exact geometry. Bounded work, but it is on the critical path for B and it is not a coding task.
- **Q7 — Real drilling numbers for the Part C seeds.** Feeds per material group, peck policy, tool-change seconds on the actual machines. Without these Part C ships inert and stays inert. Not a blocker to merging C; a blocker to C being worth anything.
- **Q8 — Does Part D proceed at all, and under whose written sign-off?** The liability step-change in D.3(5) is a business decision, not an engineering one. Recommend deferring the answer until C is measurably improving quote accuracy.
- **Q9 — CI budget for the OCP wheel.** A.4(c) removes the light test lane permanently. Confirm the CI image can cache a ~63 MB wheel plus `vtk` without an unacceptable cold-start penalty.
