# ADR-0050 — Payment Method per Invoice: NAV-Aligned Closed Vocab (no OWN escape hatch)

**Status:** Accepted — S160 / PR-105 (2026-05-30)
**Author:** Ervin Áben (ABERP), session brief on operator-selectable payment method
**Supersedes / amends:** None (new per-invoice field). Mirrors the
closed-vocab posture of ADR-0046 (`ProductUnit` / `unitOfMeasure`) and
ADR-0037 §3 (`Currency`).
**Related:** ADR-0046 (unit-of-measure closed vocab — the structural
precedent, with the key DIFFERENCE noted below), ADR-0039
(mark-as-paid operational metadata — a DISTINCT payment-method concept),
ADR-0042 (notes never reach the NAV XML — the side-store-snapshot
precedent for per-invoice operator data), ADR-0007 §"Operator-as-threat-
actor" (server stamps immutable issue date), PR-84 (three-date issuance
UX — the dates block the dropdown sits beside).

## Context

Before S160 the NAV emitter hardcoded
`<paymentMethod>TRANSFER</paymentMethod>` for every invoice, and the
printed PDF read that token back as "Átutalás". Ervin needs the payment
method to be operator-selectable per invoice — primarily **Készpénz**
(cash) for the rare cash payment, but the full NAV vocabulary should be
reachable.

Payment method is a property of the **transaction**, not the party: the
same buyer may pay by bank transfer one month and cash the next. So it
is captured on the Issue form and snapshotted on the invoice, never on
the partner record.

## Decision

### 1. Closed vocab, NO free-text escape hatch

`PaymentMethod` (in `modules/billing/src/domain/payment_method.rs`) is a
single closed-vocab enum mirroring NAV v3.0's `paymentMethodType`:
`TRANSFER` / `CASH` / `CARD` / `VOUCHER` / `OTHER`. SCREAMING_SNAKE
serde, so the wire body and the NAV `<paymentMethod>` token agree by
construction — identical shape to `NavUnitOfMeasure` (ADR-0046).

**The load-bearing difference from ADR-0046:** unit-of-measure is an
`Nav(enum) | Own(String)` SUM because NAV's `LineType` genuinely carries
a paired `<unitOfMeasure>OWN</...>` + `<unitOfMeasureOwn>{free-text}</...>`
escape hatch. **NAV's `paymentMethodType` has NO such companion** — there
is no `<paymentMethodOwn>` element in the v3.0 InvoiceData schema
(confirmed against `nav-xsd-validator`'s `walk_invoice_detail` allowlist,
which admits `paymentMethod` but has no `paymentMethodOwn` slot).
Emitting one would be rejected by ABERP's own validator
(`UnexpectedElement`) AND by NAV (`SCHEMA_VIOLATION`).

So `OTHER` (rendered "Egyéb") **is** the escape hatch NAV provides — a
catch-all that carries no free text. A `PaymentMethod::Own(String)`
variant was deliberately NOT built: it would be a wrapper around a
payload the wire cannot carry (CLAUDE.md rule 13). The original PR-105
brief specified the `Nav | Own` shape + `<paymentMethodOwn>` emit + an
XSD pair rule by analogy to unit-of-measure; all three are dropped as
NAV-invalid. If operator-facing free text for `OTHER` is ever wanted on
the printed PDF, it must ride a separate channel (the PDF reads the NAV
XML today, which by definition cannot carry it) and would never reach
NAV — a follow-up, not part of this ADR.

### 2. Default = TRANSFER (Átutalás)

`PaymentMethod::default()` is `Transfer`, and the form seeds the dropdown
to it. This preserves the pre-S160 hardcoded behaviour byte-for-byte.

### 3. Snapshot via side-store + NAV XML, NOT a DuckDB column

The audit-immutable snapshot is the on-disk side-store `input.json`
(`~/.aberp/serve/<tenant>/issued/<ULID>.input.json`) plus the rendered
NAV XML — exactly how the unit-of-measure (S159) and buyer address are
snapshotted. `InvoiceInputJson.payment_method` carries it with
`#[serde(default)]` (pre-S160 bodies → `Transfer`); the NAV emitter
reads it at issue time and writes `<paymentMethod>{token}</...>`; the PDF
renderer reads the token back from the on-disk NAV XML
(`print_invoice::payment_method_display`, which already maps all five
tokens to Hungarian labels). Storno / modification re-emits inherit the
value because they reconstruct `InvoiceInputJson` from the base's
side-stored `input.json`.

**No `invoice` table column was added.** The brief proposed
`payment_method_*` columns, but no consumer reads them: the PDF reads the
NAV XML, and storno/modification read the side-store `input.json`. A
write-only column would be dead weight (CLAUDE.md rules 2 & 13). This
diverges from `currency` / `payment_deadline` / `delivery_date` (which
DO have columns) — but those columns exist because the invoice list /
detail views query them; payment method is not displayed in those views
today. Add the column when a query consumer materialises.

### 4. Distinct from the mark-as-paid PaymentMethod (ADR-0039)

There is an unrelated `PaymentMethod` enum in `audit_payloads.rs`
(`BankTransfer` / `Cash` / `Card` / `Other`) for the PR-70 / ADR-0039
operational "mark as paid" event — that records HOW a payment was
RECEIVED after issuance. This ADR's `PaymentMethod` is the NAV
`<paymentMethod>` on the invoice itself. The TS mirrors keep the names
distinct (`InvoicePaymentMethod` vs `PaymentMethod`).

## Consequences

- Operators pick the payment method on the Issue form; the default
  (Transfer) needs no action and is byte-identical to pre-S160.
- The full NAV vocabulary is reachable; `OTHER` covers everything else.
- No migration runs; pre-S160 invoices keep emitting TRANSFER via the
  serde default — no backfill, no schema change.
- SPA modification route defaults to Transfer rather than inheriting the
  base invoice's method (same posture as its note/date handling, which
  also default rather than inherit). CLI modification DOES inherit (it
  parses the base `input.json`). Full SPA-route inheritance is a
  follow-up if an operational need surfaces.

## Adding a variant

Confirm the token against NAV's v3.0 `paymentMethodType` schema, extend
the Rust enum (`nav_token` / `from_nav_token` / `hu_label` / `en_label`),
the TS `InvoicePaymentMethod` union, and `paymentMethodOptions()`. The
paired Rust + vitest label pins fail loud on drift.
