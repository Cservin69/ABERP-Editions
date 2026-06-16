# ADR-0091 — Purchasing: operator-driven purchase-order module (AVL-gated, receiving→NCR)

- **Status:** Accepted
- **Date:** 2026-06-16
- **Deciders:** Ervin (S440 defense-backlog brief)
- **Supersedes:** none.
- **Related:** ADR-0068 (vendor-PO *spend authorization* — the deferred-in-v1 purchasing module this ADR delivers; ADR-0068 decided the autonomous-firing spend ceiling, NOT a procurement workflow), ADR-0084 (S431 AVL vendors — the gate this module fires from the PO-create path), ADR-0090 (S439 NCR/CAPA — receiving auto-creates an NCR on failed inspection), ADR-0085 (S432 heat-lot traceability — defense materials capture a heat lot on receipt), the invoice numbering allocator (S159 / `aberp_billing::allocate_in_tx`), and `[[trust-code-not-operator]]`, `[[hulye-biztos]]`, `[[no-sql-specific]]`.

## Context

ADR-0068 decided the *spend-authorization posture* for autonomous PO firing (per-PO ceiling + rolling daily cap, gate above) and explicitly deferred the **purchasing/supplier module itself** ("no purchasing module in v1"). The S425 defense audit flagged ADR-0068 as the last Proposed strand of the defense pivot: the manual procurement surface a defense shop needs — draft a PO against an approved vendor, issue it, receive deliveries, and route a failed incoming inspection into the quality system — did not exist.

S431 shipped the Approved Vendor List (AVL) with a `po_eligibility` gate and a never-fired `supplier.po_blocked_by_vendor_status` kind, designed to be the PO-create gate "when a full PO surface ships." S439 shipped the NCR/CAPA quality workflow. S432 shipped heat-lot traceability. The pieces a purchasing module must wire into already exist.

## Decision

**A sparse, operator-driven purchase-order module (`aberp::purchasing`) with three code-enforced invariants ([[trust-code-not-operator]]): an AVL gate at create + issue, a state machine where receipts (not the operator) drive the received states, and a failed incoming inspection that auto-creates an NCR.**

### Data model ([[no-sql-specific]])

Four additive tables, no CHECK / no DEFAULT / no surrogate id / no index (scan + filter in Rust):

- `purchase_orders` — header: `po_id` (`po_<ULID>`), operator-facing `po_number` (`PO-YYYY-NNNN`, annual per-tenant sequence), vendor, currency, `subtotal_minor`/`vat_rate_pct`/`vat_minor`/`total_minor` (integer minor units, mirroring `products.unit_price_minor`), `state`, the `vendor_avl_status` snapshot, issue/approval stamps, notes.
- `purchase_order_lines` — `pol_id`, product ref (optional), description, quantity (integer — discrete defense parts), unit price, `expected_heat_lot_required`, running `received_quantity`.
- `purchase_order_receipts` — **one row per (delivery, line)**: delivery-note number, received quantity, per-line `inspection_pass` + notes, optional `heat_lot_assigned`, and the auto-NCR `ncr_id` link. (The brief's receipt schema lists per-line inspection + heat-lot fields, which only make sense per line; the row carries `pol_id` + `received_quantity` to honour "line-by-line received quantities".)
- `po_number_state` — `(tenant_id, year)` → `next_number`. Atomic read-seed-advance inside the create transaction, mirroring the invoice `allocate_in_tx` floor-and-advance minus the template/NAV machinery. Monotonic + gap-free within a year; a new calendar year is a fresh bucket starting at 1 (annual reset by construction of the PK).

### AVL gate (the [[trust-code-not-operator]] core)

Resolved in code at two points, never by operator discipline:

- **Create** — a `Suspended`/`Revoked` vendor (`ApprovedStatus::blocks_po`) is refused before any number is burned; fires the S431 `supplier.po_blocked_by_vendor_status` kind (its FIRST firing from a real PO path). `Pending`/`Conditional`/`Approved`/unlisted are allowed; the status is snapshotted onto the PO (`Conditional` → the SPA's yellow chip).
- **Issue** — `Draft → IssuedToVendor` requires an `approved_by_operator` AND re-checks the **live** AVL status: a vendor suspended/revoked after create, or still `Pending` approval, blocks the issue. (Snapshot is for display; the live re-check is the gate.)

### State machine

`Draft → IssuedToVendor → PartiallyReceived → Received → Closed`, plus `Cancelled` from any pre-`Received` state. Only the operator edges (issue / cancel / close) live in `allowed_transition`; the receipt-driven `PartiallyReceived`/`Received` states are **computed** from line quantities (`receipt_state_after`), never operator-set.

### Receiving → quality

A receipt increments each line's `received_quantity`, derives the new state, and for every line with `inspection_pass == false` auto-creates a `SupplierIssue` NCR (S439) tagged with the PO + vendor + line, firing `po.incoming_inspection_failed`. A defense material line (`expected_heat_lot_required`) refuses a receipt with no heat lot.

### Audit

Nine new `po.*` EventKinds (`po.created`, `po.line_added`, `po.issued`, `po.receipt_recorded`, `po.partially_received`, `po.received`, `po.closed`, `po.cancelled`, `po.incoming_inspection_failed`) — count 159 → 168. `PoBlockedByVendorStatus` stays in the `supplier.*` family (S431); the nine lifecycle kinds open the new `po.*` family. All nine carry app-layer JSON, never NAV bytes (pinned in `aberp-verify` + `export_invoice_bundle` exhaustive-match arms + the count `const _`).

## Consequences

- The manual procurement surface ADR-0068 deferred now exists; ADR-0068's autonomous-firing spend ceiling (gate the *DEAL-saga* auto-PO above a threshold) remains future work and would layer onto this module's `create_po`.
- Money is integer minor units with a free ISO-4217 PO currency (procurement spans USD/EUR/HUF), kept independent of the NAV fiscal `Currency` enum so no invoice-money path is touched.
- Quantities are integers (discrete units); decimal-measure procurement (e.g. kg of bar by mass) is a future widening.
- A non-procurement tenant has zero rows here (sparse by design).
