# ADR-0074 — `material.*` audit EventKind family + the lot/heat/cert data model on `quoting_materials`: the material-traceability surface.

- **Status:** Proposed
- **Date:** 2026-06-11
- **Deciders:** Ervin (via S357 / PR-44 defense-pivot batch session 2).
- **Supersedes:** none — first ADR of the material-traceability strand.
- **Related:** ADR-0008 (audit ledger — the hash-chained home these events live in), ADR-0071 (`aberp-compliance` crate — home of the `lot_heat::LotId` / `HeatId` / `MaterialTraceabilitySeed` types these events carry; wired as a dep of `apps/aberp` by S355), ADR-0073 (`personnel.*` family — the sibling access-trail strand, same no-firing-site-yet posture), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[trust-code-not-operator]]`, `[[no-sql-specific]]`.

## Context

The aerospace/defense gap analysis (S330) named three structural must-builds: operator identity + e-signature (ADR-0070, audit surface in ADR-0073's `personnel.*`), and **lot/heat material traceability + material certs** — this ADR's subject.

AS9100D §8.5.2 (identification and traceability) and the DFARS 252.225-7008 "specialty metals" clause require that every piece of stock be traceable to its mill heat / lot, with the mill certificate (3.1 CoC / CofA / heat-treatment cert) retained for the life of the part, and that the traceability chain be reviewable. ABERP's commercial core treats material as a fungible per-grade scalar (`quoting_materials` is a catalogue of grades, not instances; `material_inventory.rs` is quantity bookkeeping). The S345 `aberp-compliance::lot_heat` scaffold introduced the *identity* types (`LotId` / `HeatId` / `MaterialTraceabilitySeed`) but nothing wired them to data or to the audit ledger.

Two distinct facts need a durable, tamper-evident home:

1. **A certificate was filed against a grade** — a *record* event. A document (mill cert / CofA / heat-treatment cert) now backs this material. A grade accrues many certs over its life; each filing is its own append-only landmark.
2. **A lot + heat was bound to a material instance** — a *state transition*. The material is now traceable through mill heat H of lot L. This is the load-bearing identity a part's traceability chain resolves through.

The ADR-0008 audit ledger is the right home for both. What it lacked is the **typed vocabulary** that lets a forensic walker glob "every cert / lot-heat assignment on this install" without scraping free-text — and a place on the data model to record the *current* lot/heat/cert of a grade. This session adds both; it does **not** wire firing sites (later session, when receiving / cert-upload / consumption-linkage flows exist in code).

## Decision

**A new `material.*` EventKind prefix family with two members, plus four nullable traceability columns on `quoting_materials`. The kinds carry free-form `serde_json::Value` payloads (the same posture every recent `quote.*` / `personnel.*` kind takes); their documented field shapes are pinned by serialization tests so a later firing site has a stable contract. No firing site is added in S357.**

### 1. The two kinds

| EventKind | storage string | fires when | payload |
| --- | --- | --- | --- |
| `MaterialCertAttached` | `material.cert_attached` | a material certificate is attached to a `quoting_materials` row | `{material_id, cert_kind, cert_url, attached_at_ms, operator_user_id, lot_id?}` |
| `MaterialHeatLotAssigned` | `material.heat_lot_assigned` | a lot/heat is assigned to a material instance | `{material_id, lot_id, heat_id, source_supplier?, assigned_at_ms, operator_user_id}` |

### 2. Why `cert_attached` (record) is split from `heat_lot_assigned` (state transition)

This is the load-bearing design call. The two facts have different cardinality, different mutability, and different audit semantics, so collapsing them into one kind would lose information a traceability auditor needs:

- **Cardinality.** A grade/lot accrues *many* certs (the mill cert at receipt, a CofA, a later heat-treatment cert). It has *one* current lot/heat binding at a time. A single "material updated" kind would force a reader to diff payloads to tell "another cert filed" from "rebound to a new heat".
- **Mutability.** `cert_attached` is purely additive — a filed cert is never un-filed; the record stands forever. `heat_lot_assigned` is a *transition* — the current binding can change (a re-spool, a corrected entry), and the audit chain is precisely how you reconstruct the binding-at-time-T. Mixing an immutable record stream with a mutable-state stream in one kind muddies both.
- **Authority / payload shape.** A cert filing answers "what document, what kind, where retained, by whom" (`cert_kind` / `cert_url`). A heat/lot binding answers "which mill heat, which lot, sourced from which AVL supplier" (`lot_id` / `heat_id` / `source_supplier`). The fields barely overlap; one kind would be a mostly-null union.

Keeping them separate means a forensic walker can answer "show me every cert ever filed for this grade" (`material.cert_attached`) and "reconstruct the lot/heat binding history" (`material.heat_lot_assigned`) as two clean globs. This mirrors the ADR-0073 grant/deny split rationale: distinct facts get distinct kinds so neither is inferred by diffing.

`lot_id` / `heat_id` on `MaterialHeatLotAssigned` are the load-bearing traceability anchors. The firing site (later session) constructs them through `aberp_compliance::lot_heat::{LotId, HeatId}` — validated (`[A-Za-z0-9-]`, ≤32 chars) at the write boundary — so a malformed id can never reach the ledger or the column.

### 3. A new `material.*` prefix family (the eighth)

The codebase segregates audit traffic by prefix so each consumer's glob stays narrow: `invoice.*` (per-OUTGOING-invoice export bundle, ADR-0009 §8), `system.*`, `mes.*`, `quote.*`, `inventory.*`, `email.*`, `personnel.*` (ADR-0073). Material-traceability events are none of these. Note the deliberate naming care: several existing kinds are *named* `Material*` but live under `quote.*` (`MaterialCatalogueChanged` = catalogue edit) or `inventory.*` (`MaterialReserved` = quantity reservation). Those are commercial-core facts; the new traceability events are a defense-grade cross-cutting concern. Folding them into `quote.*` would let the auto-quoting consumers sweep traceability rows; folding them into `invoice.*` would let the per-invoice export bundle's `invoice.*` glob sweep a cert filing into an invoice's evidence bundle, which is wrong.

So `material.*` is an **eighth** prefix family. The per-invoice export bundle excludes it by construction (the glob is `invoice.*`); the two workspace exhaustive-match gates (`aberp-verify::extract_nav_xml`, `apps/aberp::export_invoice_bundle::extract_nav_xml`) classify both kinds on the no-NAV-bytes arm — they carry app-layer JSON, never verbatim NAV XML — with a belt-and-braces runtime pin in each.

### 4. The data model: four nullable columns on `quoting_materials`

The audit ledger records the *history* of cert/lot/heat facts; the `quoting_materials` row records the *current* state for display and quoting. Four additive, nullable columns:

| column | type | meaning |
| --- | --- | --- |
| `current_lot_id` | `VARCHAR` | current lot, written by the `material.heat_lot_assigned` firing site; validated as a `LotId` at the write boundary |
| `current_heat_id` | `VARCHAR` | mill heat the current lot was poured from; validated as a `HeatId` |
| `cert_url` | `VARCHAR` | where the last attached cert is retained (mirrored from the `material.cert_attached` payload) |
| `cert_attached_at` | `VARCHAR` | RFC3339 stamp of the last cert attach |

NULL is the "not yet captured / not yet traced" sentinel; the future firing site interprets it in the app layer.

**No SQL `DEFAULT`, no DB CHECK.** Per `[[no-sql-specific]]` the lot/heat format validation lives in Rust (the `aberp-compliance` newtypes at the write boundary), never a DuckDB CHECK. And — pinned by the existing `quoting_materials` schema doc and the `aberp_quote_intake::log_table::S271_MIGRATION_SQL` evidence trail — the columns carry **no `DEFAULT`**: DuckDB re-applies an `ALTER … ADD COLUMN IF NOT EXISTS … DEFAULT V` default on *every replay* of the statement, and `ensure_schema` runs at the top of every writer, so a DEFAULT-bearing column would be clobbered on every unrelated write. The migration is `ADD COLUMN IF NOT EXISTS … <type>` with no default; NULL is the implicit fill for pre-S357 rows. Idempotency (and the no-clobber guarantee) is pinned by a test that runs `ensure_schema` four times and asserts a written value survives.

> **Standing rule (S367 — review F3):** no production writer for `current_lot_id` / `current_heat_id` exists yet (all writes today are `#[cfg(test)]`). When the firing site lands, the ONLY path to those columns must route through `aberp_compliance::lot_heat::{LotId, HeatId}::new` (which reject on malformed input), with a write-rejection test (malformed lot id → 400, row unchanged) — a compliance column may only be written via its `aberp_compliance` type. The newtypes exist; nothing structurally forces the future writer through them until this rule is honoured at that boundary.

**`cert_attached_at` is `VARCHAR` (RFC3339), not a SQL `TIMESTAMP`.** This matches `quoting_materials`' own `updated_at` convention (the table's module doc: "Timestamps are stored as RFC3339 VARCHAR … not a SQL TIMESTAMP"). Keeping one timestamp representation per table beats matching the S357 brief's loose word "timestamp" with a fork. Flagged in the PR report as a deliberate deviation from the brief wording.

### 5. Payload contract pinned in tests, not typed structs

Same call as ADR-0073 §3: every recent kind stores a free-form `serde_json::Value` built inline at the firing site, not a typed `audit_payloads.rs` struct. S357 has no firing site yet, so a typed struct now would be unused speculative surface (CLAUDE.md #2 / #13). Each payload's documented shape is pinned by a `*_payload_serializes` test that round-trips a sample through serde and asserts the documented fields and JSON types. The contract lives in-repo and is enforced; the struct does not exist until a firing site needs it.

## Consequences

**Positive.** The audit ledger now *speaks* material-traceability events, and `quoting_materials` can *hold* the current lot/heat/cert state; a later session adds firing sites (receiving, cert upload, consumption linkage) without re-litigating the vocabulary or the schema. The cert-record / lot-heat-transition split puts the AS9100D §8.5.2 / DFARS 252.225-7008 facts in the same tamper-evident hash chain that already carries the fiscal moat. The new prefix keeps every existing glob consumer untouched.

**Negative / deferred.** Nothing fires these kinds and nothing writes the four columns yet — the traceability surface is an empty contract until the firing sites land (which themselves wait on the receiving / cert-upload / consumption flows existing in code). The payloads are untyped `serde_json::Value`; a future session may promote them to typed structs once a firing site fixes the shape. The data model records only the *current* lot/heat (one binding per grade row); per-instance / per-receipt lot tracking (the full `MaterialTraceabilitySeed` capture) is a larger inventory-model change deferred with the firing sites.

**Future work (not S357):** firing sites for both kinds; the cert-attach UI; the lot/heat capture flow at receiving; wiring `aberp-compliance::avl` supplier ids into `source_supplier`; promoting the per-grade `current_lot_id` to a per-instance/per-receipt model once the inventory model grows lot granularity.
