# ADR-0085 — Lot/heat material traceability: assignment UI, defense WO-start gate, chain-of-custody view.

- **Status:** Accepted
- **Date:** 2026-06-16
- **Deciders:** Ervin (via S432 brief — second defense firing-site session, auto-mode).
- **Implements:** the firing site for the `material.heat_lot_assigned` EventKind that the S344–S371 defense foundation shipped kind-only (S357 / ADR-0074), never fired. Wires lot/heat chain-of-custody through the materials pipeline.
- **Related:** ADR-0074 (`material.*` traceability family + the never-fired `heat_lot_assigned`/`cert_attached` kinds — this is the first firing site), ADR-0069 (`inventory_balances` per-grade material balances — the heat-lot columns land here), S428 (`partner.customer_type` — the defense/aerospace discriminant the gate reads), S429 (`work_orders.source_quote_id` — the WO→quote link the gate walks), ADR-0062 (WO state machine — the `Start` transition the gate guards), ADR-0081 (`aberp-verify` NAV-leakage coverage), `[[trust-code-not-operator]]`, `[[hulye-biztos]]`, `[[no-sql-specific]]`.

## Context

The defense pivot shipped `EventKind::MaterialHeatLotAssigned` (S357) with a documented `lot_id`+`heat_id` payload sketch but **no firing site** — the S425 audit flagged it as an existing kind that never fires. Aerospace/defense material traceability (AS9100D §8.5.2, DFARS 252.225-7008 specialty-metals) requires every piece of stock be traceable to its mill heat/lot for chain-of-custody.

Three facts from the codebase shaped the design (verified, not assumed):

1. **Material stock is a per-grade scalar** — `inventory_balances` is keyed `(tenant, material_grade)` (ADR-0069). There is no per-physical-lot row. The heat lot is therefore bound to the grade's balance row (one heat lot per grade in v1), matching the brief's "add fields to the existing schema."
2. **A validated `LotId` type already exists** — `aberp_compliance::lot_heat::LotId` (S345): non-empty, ≤32 chars, `[A-Za-z0-9-]`. The brief's `heat_lot_number` is exactly this; reused rather than re-validated (CLAUDE.md rule 8/13).
3. **The WO carries no direct material link**, but `work_orders.source_quote_id` (S429) → `quote_pricing_jobs.{material_grade, buyer_partner_id}` (S428) → `partners.customer_type` is a clean, already-persisted chain. The gate walks it; no new WO↔material plumbing is invented.

## Decision

**Extend `inventory_balances` with four nullable heat-lot columns, fire `material.heat_lot_assigned` from a new operator assignment action, refuse defense/aerospace WO `Start` when the material has no heat lot, and add an operational Material Traceability chain-of-custody view.** Three new `material.*` EventKinds (count 135 → 138).

### Schema (additive, nullable, no SQL DEFAULT)

`inventory_balances` gains `heat_lot_number`, `mill_test_report_url`, `heat_assigned_at_utc`, `heat_assigned_by_operator` — all NULLABLE. **Existing rows are NOT backfilled**: a missing heat lot is the signal the WO-start gate reads. No SQL `DEFAULT` (the DuckDB replay-clobber trap, same posture as `qty_unit_kind`). `[[no-sql-specific]]` — validation is in code (`LotId`), not a CHECK.

### `material.heat_lot_assigned` payload divergence (surfaced, not blended)

S357's doc-comment sketched a `lot_id`+`heat_id` split. The S432 operator surface is **one** supplier-issued `heat_lot_number`, so the fired payload follows the brief: `{material_id, heat_lot_number, mtr_url, assigned_by, assigned_at}`. The kind is untyped at the ledger, so it is reused; the divergence from the S357 sketch is documented here (same posture as S431's reuse of `supplier.export_screened` in ADR-0084). The S357 payload-shape test is a hand-built JSON literal and does not constrain the firing site.

### New EventKinds (3; count 135 → 138), all `material.*`

- `material.wo_blocked_no_heat_lot` — the defense WO-start refusal record.
- `material.mtr_uploaded` — fired alongside an assignment that carries an MTR `file://` URL (future-proof for the operator-uploaded MTR field).
- `material.traceability_viewed` — the AS9100D record-of-access: who queried a part's chain-of-custody, when.

All three sit in `material.*` to keep the traceability surface globbable as one prefix; app-layer JSON payloads only, never NAV XML — added to both NAV-leakage exhaustive arms (`aberp-verify`, `export_invoice_bundle`) + all three `== 138` count pins.

### The WO-start gate ([[trust-code-not-operator]])

`resolve_heat_lot_gate(conn, tenant, &wo)` is a pure resolver: WO → `source_quote_id` → quote `material_grade`+`buyer_partner_id` → `partner.customer_type`; if Defense/Aerospace AND the material's `heat_lot_number` is empty/NULL → `Blocked`. The route enforcer fires `material.wo_blocked_no_heat_lot` and returns 409 with "Material X requires heat lot before defense/aerospace WO start". **Commercial path unaffected**: any other `customer_type`, or a WO with no quote/partner link, passes. The refusal is in code at the `Start` transition, not operator discipline.

### Traceability view (sparse-but-honest)

`material_traceability::trace` resolves a `material_id` or `heat_lot_number` to the balance row + the quotes that priced that grade + the WOs originating from those quotes. WO→physical-consumption is not yet wired and the downstream invoice leg is not tracked at all; those render as `(not tracked yet)` placeholders rather than being omitted (CLAUDE.md rule 12 — the absence must be visible to an auditor). Operational SPA tab; the query fires `material.traceability_viewed`.

## Consequences

- One heat lot per grade in v1 (the balance row holds one). Per-physical-lot rows + per-WO consumption are a later slice; the traceability view's WO linkage is via the originating quote's grade, not a hard consumption record.
- The gate is inert until a WO actually carries a `source_quote_id` whose quote has a defense/aerospace `buyer_partner_id` — no MES/WO-quote link fires it in PROD today, but the path is proven e2e via the real module functions (`tests/heat_lot_gate.rs`).
- MTR URLs are constrained to `file://` (local retention) in v1; remote URLs are a later slice. The scheme gate is in code (`validate_mtr_url`), the path is not checked for existence (the file may be on an operator workstation the server cannot see).
