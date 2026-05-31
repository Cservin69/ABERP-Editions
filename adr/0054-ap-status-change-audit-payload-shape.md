# ADR-0054 — AP `IncomingInvoiceStatusChanged` audit payload: stay minimal, join against `ap_invoice` for traceability

**Status:** Accepted — S198 / PR-198 (2026-05-31). Pins the de facto payload
shape that S177 / PR-177 shipped and that S178/S179/S197 inherited
unchanged.
**Author:** Ervin Áben (ABERP), session 198 brief — close the 💭 question
raised by the S172-S181 adversarial review.
**Supersedes / amends:** none — additive pin on a payload contract the
S177 PR landed without explicit ADR backing.
**Related:** ADR-0008 (audit ledger — chain-of-custody contract), ADR-0019
(relational storage strategy — mirror tables as queryable read-side),
ADR-0041 (ERP module architecture — AP module boundary), the AP module
backend (S177), the AP auto-sync daemon (S178), the AP SPA tabs (S179),
the AP queryInvoiceData XML fetch (S197).

## Context

`IncomingInvoiceStatusChangedPayload` (defined in
`apps/aberp/src/audit_payloads.rs:2161-2178`) carries:

```rust
pub struct IncomingInvoiceStatusChangedPayload {
    pub ap_invoice_id: String,          // `apinv_<ULID>` — the local row id
    pub idempotency_key: String,        // F8 carry-forward
    pub from_status: String,            // "Paid" / "Outstanding" / "Irrelevant"
    pub to_status: String,              // same closed vocab
    pub reason: Option<String>,         // REQUIRED when to_status=="Irrelevant"
}
```

The dedup tuple `(supplier_tax_number, nav_invoice_number)` that
`IncomingInvoiceIngestedPayload` carries — and that the `ap_invoice` mirror
table indexes on — is NOT in the status-change payload. To reconstruct
"which NAV-side invoice was this status change about?", a future export
must JOIN against the `ap_invoice` row by `ap_invoice_id`.

The session 182 adversarial review framed this as: if a future export needs
cross-tenant traceability without joining against the (mutable) `ap_invoice`
row, the payload would need extending. Worth deciding now or acknowledging
the deferred decision.

## Decision

**Keep the payload minimal.** `ap_invoice_id` is the canonical link; future
exports JOIN against `ap_invoice` for the supplier-tax-number /
NAV-invoice-number tuple.

### Why minimal

1. **Single-table-of-truth, queryable.** ADR-0019 commits to the relational
   tables as the queryable read-side; the audit ledger is the chain-of-
   custody. The dedup tuple lives in `ap_invoice` (UNIQUE-indexed on
   `(tenant_id, supplier_tax_number, nav_invoice_number)`), which is exactly
   where a future read should look. Mirroring the tuple into every audit
   payload would create two sources of truth that can drift (audit payload
   says X; mirror row says Y).
2. **`ap_invoice_id` IS the durable handle.** The local ULID is generated at
   ingest and never re-issued. The `ap_invoice` row is INSERT-once / UPDATE-
   on-status; the row's `id` column is stable for the row's full lifetime.
   An export joining audit-by-id-to-mirror-by-id is the canonical pattern
   the rest of ABERP uses (`InvoiceDraftCreated.invoice_id` → `invoice.id`).
3. **The session 182 concern's premise is hypothetical.** The question
   names "a future export that needs cross-tenant traceability without
   joining against the (mutable) `ap_invoice` row". No current export has
   this requirement; the per-tenant DB model (ADR-0002) makes cross-tenant
   exports a separate-and-different operator path that does not exist yet.
   When such an export is named, the payload-extension question can be re-
   asked at that point — but pre-emptively extending the payload for an
   un-named export is a CLAUDE.md rule 13 violation (delete before
   optimize; don't carry weight for a future that may not arrive).

### What "mutable" means and why it's OK

The session 182 review noted `ap_invoice` is "mutable" — the
`local_status` / `local_status_reason` / `local_status_changed_at` columns
are UPDATEd by the status-change handler. But the columns the dedup tuple
lives in (`supplier_tax_number`, `nav_invoice_number`) are INSERT-once and
never UPDATEd. A future export joining on `ap_invoice_id` to read the
tuple reads from immutable columns; the join is sound even though other
columns of the row are mutable.

The status-change AUDIT payload reconstructs the status-transition history
(from→to + reason + when + who); the `ap_invoice` row reconstructs the
NAV-side identity (supplier + invoice number). The two surfaces answer
different questions and should not be conflated.

### When to extend the payload

The trigger to revisit is the operator-named export use case:

- A "what status changes happened across all tenants?" report that
  consolidates audit ledgers from multiple per-tenant DBs without access
  to each tenant's `ap_invoice` mirror. Today this report does not exist;
  if it does, the cross-tenant export is the surface that needs naming
  first (not the payload shape).
- A "replay-from-audit-only" disaster recovery posture for AP (analogous
  to NAV-as-DR for outgoing). Today AP has no such posture — the mirror
  is rebuildable from NAV's `queryInvoiceData` directly (S178 / S197).
  If a future PR introduces audit-ledger-as-AP-DR, the payload needs the
  tuple to reconstruct the mirror without re-fetching NAV. That trigger
  would also fire ADR-0034's adjacent concern (audit-only reconstruction
  contract).

Neither trigger fires today. The decision is to accept the deferred
decision explicitly, with the trigger named.

## Consequences

### Wins

- Payload shape is the smallest correct one for today's exports (the
  status-history surface inside one tenant).
- The single-source-of-truth posture (ADR-0019) holds — the dedup tuple
  has one home (`ap_invoice` row), not two (row + every audit payload).
- A future schema change to the dedup tuple (e.g., NAV introduces a
  third identity field) lands in one mirror table, not in every audit
  payload that ever shipped.

### Trade-offs

- Cross-tenant audit-only consolidation is not possible today without the
  mirror. This is named explicitly here so a future maintainer reading the
  audit ledger in isolation knows where to look (the mirror, joined by
  `ap_invoice_id`).
- A `tracing::warn!` on a status-change does NOT name the
  NAV-side invoice (the log line carries `ap_invoice_id` only). Operators
  debugging "why did this status change?" need to round-trip
  `ap_invoice_id` → DB → `(supplier_tax_number, nav_invoice_number)`.
  Acceptable today (single-operator + SPA-driven workflow); flagged as a
  future ergonomics PR if the round-trip becomes painful.

### When to revisit

- A cross-tenant audit-consolidation export is named (see triggers above).
- An audit-only AP-DR posture is named.
- An operator reports "I cannot reconstruct what happened from the audit
  trail alone" — though this is unlikely while the mirror table is co-
  located in the same DB file.

## Adversarial review

- *"What if the `ap_invoice` row is deleted between status-change and
  export?"* Today no path deletes `ap_invoice` rows (the status-change
  handler is INSERT-once / UPDATE-on-status; there is no DELETE handler).
  ADR-0019's no-FK posture means a future DELETE would not cascade — but
  there is no DELETE today. If a future GDPR-erasure PR adds a DELETE,
  the audit-ledger's chain-of-custody requirement (ADR-0008) says the
  audit entry survives even when the mirror row is gone. At that point
  the payload-extension trigger fires (the audit entry must carry enough
  to be standalone-readable post-erasure).
- *"What if the supplier-tax-number was wrong in the original ingest and
  needs correcting?"* The mirror row's `supplier_tax_number` column is
  INSERT-once today; a correction would re-ingest (new row, new ULID,
  new audit chain). The audit entries pointing at the old `ap_invoice_id`
  remain valid for the old row's history. No correction-in-place pattern
  exists, and this ADR does not endorse adding one.
- *"What about the tenant_id?"* The audit entry carries `tenant_id` in its
  envelope (ADR-0008 — the `Entry.tenant_id` field). The payload does not
  need to repeat it; the audit chain is per-tenant by ADR-0002.

## Alternatives considered

- **Extend the payload with `(supplier_tax_number, nav_invoice_number)`
  upfront.** Rejected per §"Why minimal" — two sources of truth, drift
  risk, pre-emptive carry-weight for an un-named export.
- **Extend the payload with the full `IncomingInvoiceIngestedPayload`
  contents.** Considered + rejected as gross redundancy (the ingest
  payload already lives in the audit chain; a status-change is a
  reference to the ingest, not a duplicate of it).
- **Add a `prev_audit_entry_seq: u64` pointer.** Considered + rejected:
  the audit chain is already linearly walkable by `ap_invoice_id` filter;
  a prev-pointer would be a parallel walk surface that has to be kept in
  sync with the chain. Walk-by-filter is the canonical pattern.
- **No audit entry at all — just UPDATE the mirror row.** Rejected per
  ADR-0008 — the status transition is a chain-of-custody event; the
  audit entry IS the immutable record (the mirror is the queryable
  projection). The ledger entry must exist; the question was only about
  its shape.

## Invariants pinned

- `IncomingInvoiceStatusChangedPayload` field set is the canonical contract;
  a future field addition is an ADR-amendment trigger.
- `ap_invoice.id` is the durable handle; the column is INSERT-once and
  never re-issued. Pinned by the absence of any UPDATE-id or DELETE
  path in `incoming_invoices.rs`.
- The `(tenant_id, supplier_tax_number, nav_invoice_number)` UNIQUE index
  on `ap_invoice` is the dedup contract; per ADR-0019 the index is the
  read-side authority for "is this NAV-side invoice already known?". A
  future change to this index is a separate ADR (dedup-tuple change is
  load-bearing across S177, S178, S197).
