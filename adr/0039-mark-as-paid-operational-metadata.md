# ADR-0039 — Operational "mark as paid" event + `POST /api/invoices/:id/mark-paid` route (Tier-2 UX lifted to Tier-1, PR-70)

- **Status:** Accepted
- **Date:** 2026-05-26
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — operational payment-
  recording surface. Distinct from every prior ADR-0009-class
  decision in one specific way: the new event does NOT touch the
  NAV regulatory state ladder. Payment-vs-unpaid is parallel
  operational metadata; the regulatory ladder
  (Draft / Ready / Pending / Submitted / Finalized / Stornoed /
  Modified / Abandoned) continues to be driven by NAV ack evidence
  alone per ADR-0036.
- **Related:**
  - **ADR-0009 §2** — typestate ladder. PR-70 does NOT extend the
    ladder; `derive_state` is unchanged. The `InvoicePaymentRecorded`
    audit event sits orthogonal to the ladder.
  - **ADR-0036** — `derive_state` mirror of `stuck_precondition`.
    Adding a state to the ladder would require both surfaces to
    agree; PR-70 explicitly avoids that coupling by keeping
    payment OFF the ladder.
  - **ADR-0008** — append-only audit ledger + hash chain. The new
    `InvoicePaymentRecorded` kind is the twelfth landing of the
    F12 four-edit ritual (PR-6.1 / PR-7-B-3 / PR-8 / PR-10 / PR-11
    / PR-12 / PR-13 / PR-14 / PR-15 / PR-19 / PR-20 / PR-70).
  - **`project_aberp_ux_roadmap`** Tier-2 first item — at session
    81 the operator named the "quick mark as paid" model as the
    winner over full double-entry / bank reconciliation: *"in
    all my previous use cases" this was the right shape — not
    full double-entry, not bank reconciliation, just mark paid
    + date + amount + method + optional reference."*

## Context

Ervin (CEO of Áben Consulting KFT.) needs to record that an
invoice has been paid so the operator-facing list view shows at
a glance which invoices still need follow-up. Two design shapes
were considered at session 81 (filed in `project_aberp_ux_roadmap`):

1. **Full double-entry + bank reconciliation** — multi-step,
   would require a chart of accounts, a bank-statement CSV
   importer, journal entries, partial-payment handling. Months
   of work; ERP territory.
2. **Quick mark as paid** — operator clicks "Mark as paid" on
   a Finalized invoice, fills in a four-field form (date /
   amount / method / optional reference), the invoice gets a
   `Paid` chip next to the regulatory state chip.

Ervin chose shape (2) explicitly. The session-92 brief scoped
PR-70 to that exact shape; the rest is out of scope.

## Decision

PR-70 lands the "quick mark as paid" shape end-to-end:

### §1 — Audit-ledger event

A new `EventKind::InvoicePaymentRecorded` variant lands via the
F12 four-edit ritual (enum body, `as_str` arm, `from_storage_str`
arm, `round_trip_for_every_variant` array). The payload
(`audit_payloads::InvoicePaymentRecordedPayload`) carries:

- `invoice_id` — the invoice this payment is recorded against
  (prefixed `inv_<ULID>` form, same shape as every other
  invoice-bearing payload).
- `idempotency_key` — operator-decision key minted by the
  mark-paid command; threads through F8 only within the
  payment-recording surface.
- `paid_at` — canonical `YYYY-MM-DD` string (rationale below
  in §"Canonical-string date" subsection).
- `amount_minor` — i64 in the invoice's stored minor-unit form
  (whole forints for HUF, EUR cents for EUR).
- `currency` — ISO-4217 wire string (`"HUF"` / `"EUR"`).
- `method` — closed-vocab `PaymentMethod` enum (§2).
- `reference` — optional operator-supplied free-form note.

### §2 — Closed-vocab `PaymentMethod`

Four variants, PascalCase wire form: `BankTransfer`, `Cash`,
`Card`, `Other`. Closed-vocab posture per CLAUDE.md rule 7:
serde fails loud on unrecognised wire strings (no
`#[serde(other)]` fallback). The four cover every payment shape
Áben Consulting has seen in practice (bank transfer / cash /
card / catch-all `Other`); a future PR can add a fifth variant
additively without breaking the existing four wire shapes.

Hungarian + English labels are at the SPA dropdown layer:

```
Bank transfer (Átutalás)
Cash (Készpénz)
Card (Kártya)
Other (Egyéb)
```

### §3 — `POST /api/invoices/:id/mark-paid` route

Mutation route. Preconditions:

1. The invoice must be in `Finalized` state (NAV-side SAVED ack).
   Otherwise: `409 Conflict` with the current state named in the
   message. Storno / Modified / Abandoned / Pending / Submitted
   etc. all bounce here.
2. The request `currency` MUST match the invoice's stored
   currency. Otherwise: `400 Bad Request` (defence-in-depth
   against a curl bypass; the SPA pre-locks the form's currency
   display to the invoice's currency).
3. The `paid_at` string MUST parse as ISO-8601 `YYYY-MM-DD`.
   Otherwise: `400 Bad Request`.
4. No `InvoicePaymentRecorded` entry may exist for this invoice.
   Otherwise: `409 Conflict` with the existing payment record
   echoed in a typed `already_paid` body so the SPA can render
   "this invoice was already paid on X by Y" inline rather than
   surfacing a generic conflict.

On success: append `InvoicePaymentRecorded` to the audit ledger
under a single DuckDB transaction; verify the chain post-commit;
sync the mirror file; return the appended payment record + the
verify count.

### §4 — Read-side accessors

Two new `audit_query` functions:

- `is_invoice_paid(ledger, invoice_id) -> bool` — used by the
  route layer's idempotency gate (a future `latest_payment_for`
  consolidation can replace this).
- `payment_record_for(ledger, invoice_id) -> Option<PaymentRecord>`
  — used by the route's 409 echo body and by the
  `InvoiceListItem.payment` / `InvoiceDetailResponse.payment`
  wire-shape fields.

Both walk the ledger entries-rev order so the most-recent entry
wins. v1 enforces no-double-pay at the route layer, so only one
entry per invoice exists in practice; the rev-walk is defensive
against a hypothetical future "amend payment" PR.

### §5 — Wire shape

`InvoiceListItem` and `InvoiceDetailResponse` both gain an
optional `payment: PaymentRecordSummary | null` field
(`PaymentRecordSummary` mirrors the read-side `PaymentRecord`
with PascalCase serde for the SPA). The SPA's TS interface in
`apps/aberp-ui/ui/src/lib/api.ts` types it as
`PaymentRecordSummary | null`; a backend drift surfaces at
`npm run check`.

### §6 — `derive_state` UNCHANGED

The NAV regulatory state ladder
(`Unknown / Ready / Pending / PendingNavExists / Submitted /
Recovered / Finalized / Rejected / Storno / Amended / Abandoned`)
is NOT extended by PR-70. `derive_state` does not consult
`InvoicePaymentRecorded` entries. The Paid chip is a SEPARATE
visual signal that sits next to the state chip everywhere a
state chip renders.

Rationale: ADR-0036 §1 names a mirror invariant between
`derive_state` and `stuck_precondition`; adding a Paid arm to
either would require the other to mirror it, and
`stuck_precondition` is NAV-side recovery logic that has no
business knowing about local payment recording. Keeping the
two surfaces decoupled is the strict scope discipline per
CLAUDE.md rule 13 (delete before optimize — do not add a state
ladder arm that does not need to exist).

### §7 — Canonical-string date

`paid_at` is stored as `String` in canonical `YYYY-MM-DD` form
rather than a typed `time::Date`. Same posture as
`InvoiceModificationIssuedPayload.modification_issue_date`
(ADR-0024 §5): the operator already supplies the value in
canonical form via the HTML `<input type="date">`; a typed-time
wrapper would force serde-with adapters for a value that is
canonical on the wire. The route layer validates the string with
`time::Date::parse` and rejects malformed input with 400 per
CLAUDE.md rule 12; the audit payload stores the canonical string
as the source of truth.

### §8 — `amount_minor` matches the invoice's minor-unit form

The amount is stored as i64 in the invoice's stored minor-unit
form: whole forints for HUF (`Huf(pub i64)`), EUR cents for EUR
(matching the issuance-path posture per ADR-0037). The SPA's
per-currency formatter divides by 100 on the EUR branch for
display; the form's `<input type="number">` defaults to the
invoice's `total_gross` so the operator-most-common case (full
payment, no partial split) is one click.

Partial payments as a typed lifecycle are out of scope per the
session-92 brief — v1 records the operator-supplied amount
verbatim; the operator can choose to record a partial as a full
amount (or skip the record entirely until the full amount lands)
per their own bookkeeping discipline. A future PR can lift
partial-payment tracking when Ervin names the operational need.

## Out of scope (per the session-92 brief)

- No bank reconciliation, no CSV bank-statement import.
- No double-entry, no chart of accounts.
- No project profitability, no multi-warehouse inventory.
- No "edit payment" or "unpay" affordance. If a payment is
  recorded in error, the operator fixes it via direct ledger
  inspection (rare; not v1).
- No partial payments as a typed lifecycle.
- No payment-due-date reminders / outstanding-balance dashboards.

## Adversarial review

1. **"Why isn't Paid a state-chip arm?"** — Because the state
   chip mirrors the NAV regulatory ladder per ADR-0036 §1.
   Mixing the two on one chip would muddy the operator's
   "what does NAV think?" vs "did I get paid?" mental model.
   The brief is explicit: paid-vs-unpaid is operational
   metadata, NOT a NAV-side lifecycle state.
2. **"Why a closed-vocab `PaymentMethod` enum instead of free
   text?"** — Operator-named-load-bearing: filtering /
   reporting / a future "show me unpaid Bank-transfer invoices"
   surface needs typed categorisation. Closed-vocab with four
   pragmatic values + `Other` catch-all per CLAUDE.md rule 7.
3. **"What about partial payments?"** — Out of scope per the
   brief. v1 records one entry per invoice; the operator
   records the full amount when they get it, or chooses to
   wait. A future PR adds typed partial-payment support when
   Ervin names the need.
4. **"What if NAV bounces the invoice AFTER mark-paid?"** — The
   audit ledger is append-only. The state chip flips to
   `Rejected` (via `derive_state`'s `ABORTED` arm); the Paid
   badge remains (operational fact: ABERP received money).
   This is the intended decoupling per §6 — the two surfaces
   tell two different stories.
5. **"Currency-match is enforced at the SPA already; why
   defence-in-depth at the route?"** — A curl bypass against
   the loopback could submit a mismatched currency. The route
   layer rejects with 400 per CLAUDE.md rule 12 (fail loud) so
   the audit ledger never accepts an inconsistent record.

## Open questions

- **Multi-payment per invoice** — named-deferred to a future PR
  when Ervin reports the operational need. v1 enforces
  no-double-pay; widening to multi-payment is additive (relax
  the route's idempotency gate; widen `payment_record_for` to
  walk all matching entries instead of taking the most recent).
- **Bank-statement CSV ingestion** — Tier-3 per the roadmap;
  out of scope.
- **Outstanding-balance dashboard** — Tier-3 per the roadmap;
  out of scope.

## Mirror invariant

PR-70 adds the `payment: Option<PaymentRecord>` field to
`InvoiceTrace` in `serve.rs` and threads it through
`list_invoices` + `get_invoice_detail` so the wire shape is
populated from one walker pass. The `audit_query::payment_record_for`
helper exists in parallel for callers that want the value
without re-walking the full ledger; the two surfaces share one
typed-decode body (`audit_payloads::InvoicePaymentRecordedPayload`)
so drift between them surfaces at compile time.
