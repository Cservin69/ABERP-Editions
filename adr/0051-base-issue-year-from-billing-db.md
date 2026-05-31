# ADR-0051 — Base-issue year for NAV references reads `billing.invoice.issue_date.year()`, never `Entry.time_wall.year()`

**Status:** Accepted (implicit; documented retroactively in S198 / PR-198,
2026-05-31). De facto pinned by PR-183 / S183 across all four call sites.
**Author:** Ervin Áben (ABERP), session 198 brief — close the 💭 question
raised by the S172-S181 adversarial review.
**Supersedes / amends:** none — additive pin against an invariant that already
held at four of five call sites pre-PR-183 and now holds at all five.
**Related:** ADR-0008 (audit ledger as tamper-evident record, but NOT as the
authoritative read-side for invoice business data), ADR-0019 (relational
storage as source of truth; audit ledger is the chain-of-custody, the billing
DB is the queryable read), ADR-0023 (storno chain-link), ADR-0024 (modification
chain), ADR-0025 (technical annulment), ADR-0028 (observe-receiver-confirmation),
ADR-0045 (operator-configurable invoice numbering — `Segment::Year` lives in
the template language and is the surface that makes year-source choice
load-bearing).

## Context

The session 182 adversarial review flagged that
`request_technical_annulment.rs:425` (pre-PR-183) rendered the base invoice
number with `latest_sequence_year` captured from `Entry.time_wall.year()` —
the audit-ledger wall clock at the moment the base invoice's sequence was
*reserved*. Every other render-call site sources the year from
`billing.invoice.issue_date.year()`:

- `issue_invoice.rs:759`
- `issue_modification.rs:407` and `:409`
- `issue_storno.rs:471` and `:473`
- `observe_receiver_confirmation.rs:369`

For tenants on the default numbering template the two clocks are
indistinguishable — the default template has no `Segment::Year`, so the year
value is consumed but never emitted. For any tenant who adopts a year-bearing
template the moment an invoice is back-dated or post-dated across a year
boundary (a common HU end-of-year-bookkeeping case), the two clocks diverge:
the audit-ledger wall clock can name year N+1 while the operator-typed
`issue_date` names year N. The technical annulment path would then send
`<annulmentReference>` carrying the wrong year — silent NAV-side mismatch with
no operator-visible symptom until reconciliation surfaced it.

The review's 💭 question framed the choice as a deliberate posture call: is
the audit-ledger wall clock the source of truth for "what year does this
invoice belong to" (the original CLI-only annulment's intent, where opening
the billing DB was deliberately avoided), or is the operator-typed
`issue_date` the source of truth (the posture every other call site already
adopted)?

## Decision

**`billing.invoice.issue_date.year()` is the source of truth.** Every render
call that needs the year-segment of a NAV reference reads it from the
billing DB row, never from the audit ledger's `Entry.time_wall`.

Concretely:

1. The annulment path (`request_technical_annulment.rs`) opens the billing
   store and reads `base_invoice.issue_date.year()` via the
   `load_base_invoice_issue_year` helper, mirroring
   `observe_receiver_confirmation::load_base_nav_invoice_number`'s posture.
2. The walker's old `Entry.time_wall.year()` capture is deleted (CLAUDE.md
   rule 13 — delete before optimize; the walker no longer needs to thread
   wall-clock years through the chain at all).
3. The contract is pinned by
   `check_base_is_annullable_renders_year_from_base_issue_date_param` and
   `check_base_is_annullable_cross_year_cites_base_original_year` in
   `request_technical_annulment.rs`, so a future refactor that re-introduces
   the `time_wall` capture trips a loud test.

This adopts the posture of "the operator's typed date is what the law cares
about; the audit ledger records when the operator typed it, not what the
typed value means". ADR-0019's relational-SoT pin already says this for every
other surface; PR-183 / this ADR pins it for the annulment surface too.

## Why not `Entry.time_wall.year()`?

The audit-ledger wall clock is a *what-happened-when* record, not a *what-this-
invoice-is-about* record. Two failure modes the `time_wall` posture cannot
defend against:

- **Back-dating across NYE.** Operator at 02:00 CET on 2027-01-02 issues an
  invoice with `issue_date = 2026-12-30` (HU end-of-year bookkeeping). The
  audit ledger's `Entry.time_wall.year() == 2027`; the invoice number renders
  `ABERP/2027/...` on the annulment reference but `ABERP/2026/...` on the
  base invoice's own NAV XML. NAV rejects on reference mismatch.
- **Forward-dating across NYE.** Operator on 2026-12-30 issues an invoice
  with `issue_date = 2027-01-02` (forward-stamped for a customer's accounts-
  payable cycle). Symmetric: base invoice carries year 2027; annulment
  reference carries year 2026. Same rejection.

The `time_wall` posture is correct ONLY when the audit-ledger entry was
written in the same calendar year as the operator-typed `issue_date`. For
default-template tenants this is always true by elision (no year segment to
emit). For year-bearing-template tenants this is true except on the days when
the operator most needs it to be true. The posture fails in exactly the
operational scenarios the year segment exists to handle, and there is no
defensive code that can recover the right answer from the wrong clock — the
information is simply not in the ledger entry.

## Why not store the year in the audit payload?

The session 182 review's first recommendation was to capture
`base_issue_year` from the `InvoiceDraftCreated` payload's `issue_date`
field. Investigation surfaced that the `InvoiceDraftCreatedPayload` does NOT
carry an `issue_date` field — only `IncomingInvoiceIngestedPayload` and
`InvoiceRestoredFromNavPayload` do. Adding it would be a payload extension
across every issuance / storno / modification path for the sole benefit of
the annulment renderer. The billing-DB read achieves the same outcome at one
call site for one path, with zero schema or payload churn — strictly less
surface.

## Consequences

- The annulment path now requires a billing DB read at render time. This is
  the posture every other render path already had — `observe_receiver_
  confirmation::load_base_nav_invoice_number` is the existing precedent. No
  new abstraction; the helper sits next to the existing one.
- The walker no longer threads `latest_sequence_year` through the chain.
  That parameter is deleted from `WalkOutcome`, simplifying the contract.
- For default-template tenants (today: all of them) the change is byte-
  identical to pre-PR-183 — the year value is consumed by
  `template.render_for_build` but the template doesn't emit it.
- For year-bearing-template tenants the change is the difference between
  "silent NAV-reference mismatch on cross-year operations" and "correct
  reference". The operator-visible change is "annulment of a back/forward-
  dated invoice no longer fails with NAV reference-mismatch ABORTED".
- ADR-0019's relational-SoT pin now applies uniformly across the five
  render-call sites. The audit ledger is the chain-of-custody; the billing
  DB is the queryable read-side. No call site mixes them.

## Adversarial review

- *"What if the billing row is missing at annulment time?"* The annulment
  precondition already requires the base invoice to be in `Finalized` state
  (`audit_query::stuck_precondition`). A Finalized invoice has a billing-DB
  row by ADR-0019's two-write contract; if it doesn't, the annulment
  precondition fails before the render call, with a clearer error than the
  render would emit.
- *"What if the operator edits `issue_date` post-issuance?"* Issue-date is
  immutable post-issuance per ADR-0007's operator-as-threat-actor clause —
  the server stamps it at issue and no UI surface exposes an edit affordance.
  A direct DB mutation is an operator-shoots-self attack outside the model.
- *"Does this make storno of a year-bearing-template invoice slower?"* No —
  the storno path already does the billing-DB read for `base_issue_year`
  (`issue_storno.rs:975`). Annulment is the same shape of read; performance is
  one extra DuckDB query (~ms) per annulment, which is a rare operator-
  initiated path.

## Alternatives considered

- **Status quo (use `Entry.time_wall.year()` everywhere for consistency).**
  Rejected: would require changing four other call sites (issue, storno,
  modification, observe-receiver-confirmation) to read from the audit ledger
  instead of the billing DB, which is the wrong direction per ADR-0019 — and
  the wall-clock posture still fails the back/forward-dating cases above.
- **Extend `InvoiceDraftCreatedPayload` with `issue_date` for downstream
  readers.** Rejected per §"Why not store the year in the audit payload?".
- **Add a synthetic `original_issue_year` field to a future side-store
  artifact.** Rejected: the billing-DB column already exists, is already
  populated, and is already the source of truth for every other read.
  Adding a parallel snapshot is redundant.

## Invariants pinned by test

- `check_base_is_annullable_renders_year_from_base_issue_date_param` —
  renders trip on parameter substitution drift.
- `check_base_is_annullable_cross_year_cites_base_original_year` — the
  NYE-boundary scenario is exercised so a future regression that re-
  introduces `time_wall.year()` capture fails loud.
