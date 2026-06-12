# ADR-0078 — `supplier.*` audit EventKind family + AVL overlay on partners: the Approved-Vendor-List surface.

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Ervin (via S361 / PR-48 defense-pivot batch session 6).
- **Supersedes:** none — first ADR of the Approved-Vendor-List strand.
- **Related:** ADR-0008 (audit ledger — the hash-chained home these events live in), ADR-0048 (partners table — the master-data record this session overlays AVL columns onto), ADR-0071 (`aberp-compliance` crate — home of the `avl` module these events lean on; wired as a dep of `apps/aberp` by S355), ADR-0073 (`personnel.*` family — the access-trail strand whose decision-event posture the `supplier.export_screened` kind mirrors), ADR-0074 (`material.*` family — the additive-nullable-columns migration pattern this session reuses), ADR-0076 (`export.*` family — the export-control strand whose `Jurisdiction` storage-string discipline the `DpasRating` / `ExportScreeningStatus` newtypes parallel), ADR-0077 (`cui.*` family — immediately preceding prefix family, same no-firing-site-yet posture and kind-split rationale), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[mock-everything-principle]]`, `[[trust-code-not-operator]]`.

## Context

The aerospace/defense gap analysis (S330) named the Approved Vendor List (AVL) as one of three structural must-builds of the pivot: a defense manufacturer must be able to (a) qualify *who* it buys from (AS9100D §8.4 supplier control), (b) carry a **DPAS priority rating** so a rated defense order can compel a supplier to prioritise it (FAR 11.6 / DPAS regulation **15 CFR Part 700**), and (c) **screen** every supplier against the export-control denied-party lists (the BIS Entity List, OFAC SDN, State DDTC debarred parties — the consolidated screening required by **EAR § 744**). Two of these obligations are decisions that must be recorded in a tamper-evident form:

1. **DPAS priority accountability (15 CFR § 700.13 / FAR 11.604):** when a rated order is placed, the rating the supplier is approved to service is an obligation the AVL must evidence — *which rating, set by whom, when*.
2. **Denied-party diligence (EAR § 744 / consolidated screening):** a supplier screen that returns a hit blocks transacting; an audit of the screen — *clear / hit / inconclusive, against which list, run by whom, when* — is the artifact that proves the diligence happened.

`aberp-compliance::avl` (S345 / ADR-0071) already scaffolded the *vocabulary*: the `DpasRating` enum (None / DoC1 / DxC1), the `ExportScreeningStatus` enum, the `QualLevel` qualification gate, and the `ApprovedSupplierEntry` record. What it lacked is (a) the **audit vocabulary** so a forensic walker can glob "every DPAS / screening decision on this install", (b) the **canonical storage-string contract** the firing site and the partner columns need, and (c) the **data-model columns** on the partner record to hold a supplier's current AVL state. This session adds all three. It does **not** wire firing sites — no AVL CRUD surface exists in code yet — and it does **not** add a real screening backend (`MockExportControlProvider` stays unchanged, per `[[mock-everything-principle]]`).

## Decision

**A new `supplier.*` EventKind prefix family with two members, four additive nullable AVL columns on the `partners` table, and `as_str` / `from_storage_str` storage-string pairs on `aberp_compliance::avl::DpasRating` and `ExportScreeningStatus`. The kinds carry free-form `serde_json::Value` payloads (the same posture every recent `personnel.*` / `material.*` / `part.*` / `export.*` / `cui.*` kind takes); their documented field shapes are pinned by serialization tests. No firing site is added in S361.**

### 1. The two kinds

| EventKind | storage string | fires when | payload |
| --- | --- | --- | --- |
| `SupplierDpasPrioritySet` | `supplier.dpas_priority_set` | a DPAS priority rating is assigned to a supplier | `{partner_id, dpas_rating, operator_user_id, set_at_ms}` |
| `SupplierExportScreened` | `supplier.export_screened` | a supplier is screened against the denied-party lists | `{partner_id, screening_result, screening_source, screened_at_ms, operator_user_id, hit_details?}` |

### 2. Why two kinds (DPAS / screening) and not one

These are two different facts in the AVL lifecycle, separated for the same reason ADR-0073 split identity from access, ADR-0076 split classification from access, and ADR-0077 split marking from access: distinct facts get distinct kinds so none is inferred by diffing payloads.

- **`dpas_priority_set` is an assignment record.** It answers *"what DPAS rating is this supplier approved to service, set by whom?"* It is additive — a re-rating is a new record, and the history reconstructs how a supplier's priority authority changed over time.
- **`export_screened` is a decision event.** It answers *"who screened this supplier, against which list, and what did it return?"* Both clear and non-clear outcomes are recorded (not hits-only) — the *clear* result is exactly the evidence that EAR § 744 diligence was performed. The optional `hit_details` carries the list / reason string, present only on a hit / inconclusive.

A single `supplier.event` kind with a `type` discriminator was rejected for the same reasons ADR-0076 §2 / ADR-0077 §2 give: it would force every consumer to branch on an in-payload tag the prefix family is meant to make globbable, and it would defeat the per-kind exhaustiveness gate.

### 3. The four AVL columns on `partners` — additive, nullable, NO SQL `DEFAULT`

The partner record is the home for a supplier's current AVL state, so the columns land on `partners` (not a new table — a supplier is already a `PartnerKind`):

| column | type | holds |
| --- | --- | --- |
| `dpas_rating` | `VARCHAR` nullable | the `DpasRating::as_str` form (15 CFR 700.12 `<DO\|DX>-<program symbol>`, e.g. `DO-A1` / `DX-A7`); NULL = unrated |
| `eccn` | `VARCHAR` nullable | the supplier's product Export Control Classification Number (free-form) |
| `export_screening_status` | `VARCHAR` nullable | the `ExportScreeningStatus::as_str` vocab (`not_screened` / `clear` / `hit` / `inconclusive`) |
| `export_screened_at` | `VARCHAR` nullable | RFC3339 stamp of the last screen |

The migration follows the **S357 `quoting_materials` pattern exactly**: a `PARTNERS_S361_AVL_MIGRATION_SQL` of four `ALTER TABLE partners ADD COLUMN IF NOT EXISTS … VARCHAR` statements, run by `ensure_schema` after the `CREATE TABLE IF NOT EXISTS`. **No SQL `DEFAULT`** — the DuckDB DEFAULT-on-replay trap (pinned on the S357 migration and `aberp_quote_intake::log_table::S271_MIGRATION_SQL`) re-applies the default on every replay, and `ensure_schema` runs at the top of every partner writer, so a DEFAULT-bearing column would be clobbered on every unrelated `update_partner` call. NULL is the "not yet on the AVL" sentinel; an idempotency-and-no-clobber test proves a written value survives repeated `ensure_schema` calls.

**Validation lives at the (future) write boundary, never as a DB CHECK** (per `[[no-sql-specific]]`): the firing site validates an inbound `dpas_rating` / `export_screening_status` through `DpasRating::parse` / `ExportScreeningStatus::from_storage_str` before it reaches the column, so the column only ever holds a well-formed token. `eccn` is validated for *shape* through `export_control::validate_eccn` (`[0-9][A-E][0-9]{3}` or the literal `EAR99`) — a structured but open vocabulary (`7A994`, `EAR99`, …) the classification service determines, never a closed enum. No production writer exists yet (S367 review F14): these validators are the mandatory path the future AVL CRUD handler must route through — a compliance column may only be written via its `aberp_compliance` validator.

**`export_screened_at` is `VARCHAR` (RFC3339), not a SQL `TIMESTAMP`** — every timestamp on `partners` (`created_at` / `updated_at`) is already a `VARCHAR NOT NULL` RFC3339 string, and keeping one timestamp representation per table beats matching the brief's loose "timestamp" wording. This is the same flag S357 raised for `cert_attached_at`; it is carried here, not silently resolved.

### 4. Storage-string newtypes — `DpasRating` / `ExportScreeningStatus` reshaped to the screening vocab

The S345 scaffold gave `DpasRating` (None / DoC1 / DxC1) and `ExportScreeningStatus` (NotScreened / Clear / Restricted / Denied) but **no canonical string form** — the column and the audit payload both need one. S361 adds `as_str` / `from_storage_str` round-trip pairs to both, mirroring the `export_control::Jurisdiction` discipline (ADR-0076 §3): a free-text rating / status can never reach the ledger or the column — it must round-trip through the typed pair first, and an unknown string fails loud (a mis-parse of an unscreened supplier to `clear` would be the worst-class export-control bug).

`ExportScreeningStatus` is **reshaped**, not merely extended: the scaffold's placeholder `Restricted` / `Denied` variants are dropped in favour of the denial-list-screening outcome the BIS Consolidated Screening List / OFAC / State DDTC actually return — `Clear` (no match), `Hit` (a denied-party match), `Inconclusive` (a partial / common-name match needing manual review). The restricted-vs-denied *adjudication* is the job of `export_control::ScreeningResult` (which keeps those variants), not the stored AVL *status*. This consolidation means the column, the `supplier.export_screened` payload `screening_result` field, and the type all speak **one** vocabulary (`not_screened` / `clear` / `hit` / `inconclusive`). The scaffold is S345-foundation-only (never wired to production), so reshaping its vocabulary now that the screening is actually being wired is a delete-first cleanup (CLAUDE.md #13), not a breaking change to anything real.

> **Correction (S367, 2026-06-12 — review F13):** the original `DpasRating` here was a closed enum `{None, DoC1, DxC1}` whose tokens (`NONE` / `DO-C1` / `DX-C1`) hardcoded a single program-identification symbol — it could not represent `DO-A1`, the canonical aircraft-program rating and 15 CFR 700's own worked example. Per **15 CFR 700.12**, a priority rating is a *rating symbol* (`DO` / `DX`) joined to a *program identification symbol* from Schedule I (`A1`…`A7` aircraft, `C1`…, `F1`, …). S367 remodelled `DpasRating` as `{ priority: DpasPriority (DO|DX), program_symbol: String }`, validated `[A-F][1-9]` against the Schedule I shape via `DpasRating::validate_program_symbol`, rendering `DO-A1` form through `as_str` and parsing it back through `DpasRating::parse`. "Unrated" is now the *absence* of a rating (`Option<DpasRating>::None` / NULL column), not a sentinel `NONE` token. No firing site or ledger row existed, so the column/payload tokens were re-pinned freely.

### 5. A new `supplier.*` prefix family (the twelfth)

The codebase segregates audit traffic by prefix so each consumer's glob stays narrow: `invoice.*` (per-OUTGOING-invoice export bundle, ADR-0009 §8), `system.*`, `mes.*`, `quote.*`, `inventory.*`, `email.*`, `personnel.*` (ADR-0073), `material.*` (ADR-0074), `part.*` (ADR-0075), `export.*` (ADR-0076), `cui.*` (ADR-0077). AVL events are none of these. Folding them into `export.*` would conflate the item-export-classification surface with the supplier-qualification surface (DPAS is not an export concern at all); folding them into `personnel.*` would conflate operator-identity traffic with supplier decisions; folding them into `invoice.*` would let the per-invoice export bundle's `invoice.*` glob sweep a supplier row into an invoice's evidence bundle.

So `supplier.*` is a **twelfth** prefix family. The per-invoice export bundle excludes it by construction (the glob is `invoice.*`); the two workspace exhaustive-match gates (`aberp-verify::extract_nav_xml`, `apps/aberp::export_invoice_bundle::extract_nav_xml`) classify both kinds on the no-NAV-bytes arm — they carry app-layer JSON, never verbatim NAV XML — with a belt-and-braces runtime pin in `export_invoice_bundle`.

### 6. The mock-everything boundary for BIS / State.gov denial-list APIs

The real `supplier.export_screened` firing site (later session) will consult a denied-party screening service — the BIS Consolidated Screening List API, the OFAC SDN list, the State DDTC debarred-parties list. S361 adds **none** of that: per `[[mock-everything-principle]]`, the swap-point is `aberp_compliance::export_control::ExportControlProvider` (the trait already shipped by S345), and `MockExportControlProvider` — which answers `ScreeningResult::Clear` for everything — **stays unchanged**. The `supplier.*` audit vocabulary and the AVL columns are the durable contract; the real screening backend slots in behind the existing trait later without re-litigating the event shape or the storage tokens. The `screening_source` payload field exists precisely so the mock (`"mock-bis-csl"`) and a future real backend (`"bis-csl-api"`) are distinguishable in the ledger.

### 7. Payload contract pinned in tests, not typed structs

Same call as ADR-0073 §3 / ADR-0074 §5 / ADR-0076 §6 / ADR-0077 §6: every recent kind stores a free-form `serde_json::Value` built inline at the firing site, not a typed `audit_payloads.rs` struct. S361 has no firing site yet, so a typed struct now would be unused speculative surface (CLAUDE.md #2 / #13). Each payload's documented shape is pinned by a `*_payload_serializes` test that round-trips a sample through serde and asserts the documented fields and JSON types (including the `screening_result` string and the optional `hit_details`). The contract lives in-repo and is enforced; the struct does not exist until a firing site needs it.

## Consequences

**Positive.** The audit ledger now *speaks* AVL decisions, the partner record can *hold* a supplier's current AVL state, and `aberp-compliance` has the canonical storage-string contract both need; a later session adds firing sites (DPAS-set on rating assignment, export-screened on a screening run) without re-litigating the vocabulary or the column schema. The DPAS / screening split puts the two AVL facts in the same tamper-evident hash chain that already carries the fiscal moat, the access trail, the material-traceability surface, the per-unit serialization surface, the export-control surface, and the CUI surface. The new prefix keeps every existing glob consumer untouched. The screening backend stays mocked, so no live denial-list API call is made until a session deliberately wires one.

**Negative / deferred.** Nothing fires these kinds yet — the AVL surface is an empty contract until firing sites land (which themselves wait on an AVL CRUD surface existing in code). The payloads are untyped `serde_json::Value`; a future session may promote them to typed structs once a firing site fixes the shape. The four AVL columns have no SPA surface and no CRUD writer yet. `export_screened_at` is a `VARCHAR` RFC3339 string (flagged §3). The `ExportScreeningStatus` reshape drops `Restricted` / `Denied` — any future consumer that wanted the stored *adjudication* (rather than the raw screening outcome) must reach for `export_control::ScreeningResult`.

**Future work (not S361):** firing sites for both kinds; the AVL CRUD surface (qualify / rate / screen a supplier); the AVL-entry UI; wiring a real `ExportControlProvider` backend behind the screening firing site; a bulk re-screening cron (denied-party lists change, so a one-time screen goes stale); promoting the `supplier.*` payloads to typed structs once a firing site fixes the shape; a `QualLevel` gate that blocks a quote / PO to a `Disapproved` or denied-party-`Hit` supplier.
