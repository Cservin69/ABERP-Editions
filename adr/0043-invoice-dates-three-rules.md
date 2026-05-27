# ADR-0043 — Three Invoice Dates: Immutable Issue, Bidirectional Payment Deadline, Comfort-Zone Delivery

**Status:** Accepted — PR-84 (2026-05-27)
**Author:** Ervin Áben (ABERP), session 108
**Supersedes / amends:** none (additive surface)
**Related:** ADR-0007 (operator-as-threat-actor — immutable issue date), ADR-0008 (audit ledger — tamper-evident regulatory trail), ADR-0009 (NAV invoice issuing — XSD-positioned date fields), ADR-0042 (notes recipient-facing — contrast: dates ARE regulatory and DO reach NAV)

## Context

The Hungarian invoice template (per
`reference_aberp_invoice_template.md`) carries three distinct date
fields: **SZÁMLA KELTE** (invoice / issue date), **TELJESÍTÉS KELTE**
(delivery / fulfillment date), and **FIZETÉSI HATÁRIDŐ** (payment
deadline). Pre-PR-84 ABERP's NAV emitter and PDF renderer both
surfaced all three, but the operator-facing UX did not let the
operator set them — the form had no date inputs, the wire body
defaulted everything to the system date, and the NAV emitter silently
mirrored `<invoiceIssueDate>` into both `<invoiceDeliveryDate>` and
`<paymentDate>` (`apps/aberp/src/nav_xml.rs:946,950`).

This was a regulatory bug. The three dates have **different**
semantics:

- **`<invoiceIssueDate>`** — when the invoice was issued. By Hungarian
  practice this is the system date at issuance time; it is also the
  one date the operator must NEVER set (operator-as-threat-actor:
  back-dating an issue is fraud).
- **`<invoiceDeliveryDate>`** — when the underlying supply was
  delivered / the service was performed. **Drives which VAT period
  the invoice belongs to** (and therefore which month the operator
  must declare + pay VAT for). This MAY differ from the issue date —
  invoicing in May for an April delivery is a common case.
- **`<paymentDate>`** — when payment is due. Commonly expressed as
  "invoice date + N days" but the absolute date is what the buyer
  sees and what the bank reconciliation uses.

Silently mirroring the issue date into both regulated-and-recipient
fields is the *worst* class of bug: the invoice looks plausible, the
NAV submit succeeds, and the resulting VAT-period mis-file surfaces
only at the next ÁFA return — possibly months later, when the operator
discovers the back-of-the-month invoice landed in the wrong reporting
period.

## Decision

Each of the three dates gets a **distinct rule**:

### 1. Invoice date (Számla kelte) — server-stamped, immutable

The system clock at issuance time is the only source. The wire body
does not carry an issue-date field; the SPA's IssueInvoice form
displays today's local date read-only for UX-anchoring but the server
ignores any client-provided value. Per ADR-0007 §"Operator-as-threat-actor"
the operator cannot influence this.

The display value on the form is purely a comfort affordance — the
form's payment-deadline offset is calculated against the displayed
date, but the regulatory truth is the server's clock at
`OffsetDateTime::now_utc()` inside `issue_from_parsed`.

### 2. Payment deadline (Fizetési határidő) — bidirectional offset/absolute

The form exposes both inputs (offset-days + absolute-date); each
edits the other live via the `paymentDeadlineFromOffset` /
`daysBetween` helpers. The resolved **absolute date** is what travels
on the wire and persists. The default offset is 8 days
(`DEFAULT_PAYMENT_OFFSET_DAYS`); the operator can pick any non-negative
integer (zero permitted for cash sales).

Stored as a `DATE` column (`invoice.payment_deadline`); emitted as
`<paymentDate>YYYY-MM-DD</paymentDate>` in the NAV body. Pinned by
`apps/aberp-ui/ui/src/lib/invoice-dates.test.ts`'s round-trip pair
(offset → date → offset).

### 3. Delivery date (Teljesítési dátum) — REGULATORY, comfort-zone guarded, audit-logged

This is the NAV `<invoiceDeliveryDate>` field. The operator can pick
any calendar date but the form classifies the choice against the
**comfort zone `[invoice_date, payment_deadline]`** (closed interval,
inclusive of both endpoints):

| Zone                              | Form UX                            | Audit flag             |
| --------------------------------- | ---------------------------------- | ---------------------- |
| `InRange`                         | Silent — commit immediately        | `None`                 |
| `BeforeInvoiceDate`               | Inline "Are you sure?" confirm     | `"BeforeInvoiceDate"`  |
| `AfterPaymentDeadline`            | Inline "Are you sure?" confirm     | `"AfterPaymentDeadline"` |

The audit flag is stamped onto the `InvoiceDraftCreated` payload via
the new `with_invoice_dates` builder. Closed vocab — an unknown
string on a future audit row indicates ledger tampering or schema
drift the inspector should investigate. Defence in depth: the
backend re-classifies via the domain helper
`aberp_billing::classify_delivery_date` so a curl bypass that sends
a wrong override flag still produces an inspector-recoverable trace.

The comfort-zone classifier is mirrored exactly between the SPA
(`apps/aberp-ui/ui/src/lib/invoice-dates.ts::comfortZone`) and the
Rust domain layer
(`modules/billing/src/domain/invoice_dates.rs::classify_delivery_date`).
The mirror-invariant precedent (A156 / A161) applies: a regression
that drifts one side surfaces at the per-side vitest / cargo test
pin, not at a silent operator misclassification.

## Storage + wire shape

### DuckDB

Two new nullable `DATE` columns on `invoice`:

```sql
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS payment_deadline DATE;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS delivery_date    DATE;
```

Migration `MIGRATE_PR_84_SQL` is idempotent (`ADD COLUMN IF NOT EXISTS`).
**No backfill `UPDATE` runs** — pre-PR-84 rows have NULL for both
columns, and the read path's `load_invoice` falls back to
`issue_date` for NULL. This preserves byte-on-wire behaviour for
invoices issued before the migration ran (the pre-PR-84 emitter also
mirrored issue_date for both fields, so a NULL→issue_date fallback
is structurally identical to the pre-PR-84 behaviour).

### Wire body

`IssueInvoiceRequest` (Rust + TS mirror invariant) gains three
optional fields:

- `paymentDeadline: Option<String>` — YYYY-MM-DD
- `deliveryDate: Option<String>` — YYYY-MM-DD
- `deliveryDateOverride: Option<String>` — closed vocab
  (`"BeforeInvoiceDate"` / `"AfterPaymentDeadline"` / null)

All three are optional for back-compat: integration tests and CLI
callers pinned at the pre-PR-84 shape continue to type-check, and the
issuance pipeline defaults missing dates to the server-stamped issue
date (preserves pre-PR-84 wire byte shape).

### Audit payload

`InvoiceDraftCreatedPayload` gains three new fields (additive,
`#[serde(default)]`):

- `payment_deadline: Option<String>`
- `delivery_date: Option<String>`
- `delivery_date_override: Option<String>`

Stamped by the new `with_invoice_dates(invoice, override)` builder
in the audit-payload constructor chain.

## Three load-bearing invariants

1. **Immutable issue date.** The server stamps `<invoiceIssueDate>`
   from `OffsetDateTime::now_utc()` inside `issue_from_parsed`. The
   wire body does NOT carry an issue-date field. Pinned by
   `apps/aberp-ui/ui/src/lib/issue-invoice.test.ts`'s "does NOT carry
   the form's invoiceDate" test.

2. **Bidirectional payment-deadline calc.** `addDays(invoice,
   daysBetween(invoice, deadline)) === deadline` for any well-formed
   pair. Pinned by `apps/aberp-ui/ui/src/lib/invoice-dates.test.ts`'s
   round-trip block (both directions, 5 sample pairs + parametric
   over integers).

3. **Comfort-zone classifier — closed interval inclusive on both
   endpoints.** A delivery date equal to the invoice date OR equal to
   the payment deadline is in-range (no confirm, no audit flag).
   Pinned at both layers (Rust `invoice_dates::tests` and TS
   `invoice-dates.test.ts::comfortZone`) with explicit endpoint pins.

## Alternatives considered

### A1. Surface a NAV-equivalent `<paymentMethod>` picker in the same PR

Out of scope. The current emit hardcodes `<paymentMethod>TRANSFER</>`
which matches Áben Consulting's actual payment posture; a future PR
can widen this when a second payment method becomes operationally
relevant. PR-84 stays narrowly on the three dates (rule 3: surgical
changes).

### A2. Carry `delivery_date` + `payment_deadline` on every typestate
(SubmittedInvoice → AbandonedInvoice)

Rejected. The existing typestate chain carries `issue_date` forward
because the `into_*` transitions are pure renames and the audit-
evidence bundle reconstructs the body from any state. The two new
dates ride only on `DraftInvoice` + `ReadyInvoice` — the duckdb row
is the authoritative regulatory record for the submission-lifecycle
states (which is how `print_invoice.rs` reads dates today anyway: off
the parsed NAV XML, not off the in-memory typestate). Adding two
fields to seven typestates was a CLAUDE.md rule 2 abstraction trap.

### A3. Default the payment-deadline offset to 30 days (more common
business convention)

Rejected for Áben Consulting's posture (Ervin: 8 days is the typical
operational term). The constant `DEFAULT_PAYMENT_OFFSET_DAYS = 8`
lives in one file (`apps/aberp-ui/ui/src/lib/invoice-dates.ts`) and is
trivial to change as the business default shifts; the operator can
override per-invoice regardless.

## Consequences

- **Pro:** the three dates are no longer a silent bug. The operator
  controls the regulatory `<invoiceDeliveryDate>` explicitly + the
  tamper-evident audit trail records every out-of-range override.
- **Pro:** the bidirectional payment-deadline UX matches how operators
  think about payment terms (some specify offsets, some specify
  absolute dates, both update each other live).
- **Con:** the storno / modification chain paths default both dates
  to the chain-issue's server-stamped date — same posture as pre-PR-84
  for those flows. A future PR can surface operator pickers in the
  storno + modification UIs when the operational need arises.
- **Con:** the PDF model carried `fulfillment_date` + `payment_due_date`
  fields pre-PR-84 that print_invoice.rs sourced via NULL→issue_date
  fallbacks; the fallback is now dead code on post-PR-84 invoices
  but harmless on pre-PR-84 invoices (the migration is additive, not
  backfilling). The fallback stays in place for cross-version
  compatibility.

## Pin tests added in PR-84

- `apps/aberp-ui/ui/src/lib/invoice-dates.test.ts` — 35 pins
  (parseIsoDate, addDays, daysBetween, comfortZone, round-trip).
- `apps/aberp-ui/ui/src/lib/issue-invoice.test.ts` (extended) —
  paymentDeadlineFromOffset, deliveryDateOverrideFor, composer
  emits paymentDeadline / deliveryDate / deliveryDateOverride
  verbatim, composer does NOT emit invoiceDate.
- `modules/billing/src/domain/invoice_dates.rs::tests` — 9 pins
  (comfort-zone classifier at every boundary + the zero-day cash-sale
  edge case + the malformed range refusal).
- `apps/aberp/tests/pr_84_invoice_dates.rs` — 2 pins (NAV emit
  surfaces three distinct dates; DuckDB round-trip preserves both
  operator-supplied dates).

## How to apply

- When touching `nav_xml::write_invoice_detail`: the signature is
  now `(w, delivery_date, payment_deadline, currency, rate_metadata)`.
  Do NOT pass `issue_date` in either of the two date slots — the
  pin `nav_emit_surfaces_three_distinct_dates` catches this regression
  but read this ADR first to avoid the round-trip.
- When adding a new NAV-XML emitter (annulment is the next candidate):
  decide explicitly whether the three dates apply (typically yes for
  invoice-shaped wire bodies, no for annulment which has only an
  `annulmentTimestamp`).
- When extending the audit-payload schema: stamp `with_invoice_dates`
  via the builder chain alongside `with_notes` + `with_bank_snapshot`.
  Forgetting it is silently OK at compile time (the new fields
  default to None per `#[serde(default)]`) but a missing stamp
  means the audit row carries no date trail — surfacing as an
  inspector-visible gap, not a hard fail.
