# ADR-0034 — Recover-from-NAV chain reconstruction — explicit `aberp recover-from-nav` operator command reconstructs the missing local `InvoiceSubmissionResponse` audit entry from NAV's `queryInvoiceData` response on a state-2 Pending invoice whose most-recent `InvoiceCheckPerformed` outcome is `"exists"`; the `nav-transport` crate's `query_invoice_data` module gains one additive parse helper (`parse_audit_data_transaction_id`) for the NAV-side `<auditData>/<transactionId>` field while preserving the ADR-0028 verbatim-bytes-first posture for the OK response body; `audit_query::stuck_precondition` is UNCHANGED (Layer-2 entries remain informational-only per ADR-0033 §6; the §6 pin tests stay valid) and `recover-from-nav` carries its own typed precondition checker that loud-fails on every non-recoverable shape; the reconstructed entry reuses the existing `InvoiceSubmissionResponse` `EventKind` + `InvoiceSubmissionResponsePayload` shape with the preceding `InvoiceCheckPerformed(outcome=exists)` entry as the provenance marker per the audit-evidence chain (F12 four-edit ritual does NOT fire); `InvoiceAckStatus` entries are NOT fabricated — the operator runs `aberp poll-ack` next to drive the recovered chain to its authoritative terminal state (CLAUDE.md rule 12 — don't fabricate facts ABERP cannot itself verify); closes F48; `retry-submission`'s state-2 + Exists loud-warned summary gains a pointer at `recover-from-nav`; `mark-abandoned` remains unchanged (F49 still named-deferred)

- **Status:** Accepted
- **Date:** 2026-05-22
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — first PR after
  ADR-0033 / PR-20 to introduce the **second half** of ADR-0009 §5's
  Layer-2 idempotency design intent ("If NAV already has it, fetch
  the chain via `queryInvoiceData` and reconstruct local state.").
  Closes F48 at the chain-reconstruction level. F49 (Layer-2-aware
  `mark-abandoned`) and F50 (`queryInvoiceCheck` rate-limit
  cooldown) remain named-deferred with their existing triggers.
  Audit-ledger crate is **unchanged** — no new `EventKind`
  variant; the F12 four-coordinated-edit ritual does NOT fire.
  The binary's `apps/aberp/src/audit_payloads.rs` is **unchanged** —
  the recovered Response entry reuses the existing
  `InvoiceSubmissionResponsePayload` shape. The
  `crates/nav-transport/src/operations/query_invoice_data.rs`
  module gains one additive `parse_audit_data_transaction_id`
  helper function (the existing `call` / `QueryInvoiceDataOutcome`
  surface is unchanged per ADR-0028 §"Surfaced conflict 3"'s
  verbatim-bytes-first posture). The binary gains one new
  orchestration module `apps/aberp/src/recover_from_nav.rs` and
  one new CLI subcommand `aberp recover-from-nav`. The binary's
  `apps/aberp/src/audit_query.rs` is **unchanged** at the
  classifier level — `recover-from-nav` carries its own typed
  precondition checker per the operator-facing-twin posture
  (CLAUDE.md rule 2). The binary's
  `apps/aberp/src/retry_submission.rs` gains a one-line pointer
  in its state-2 + Exists loud-warned operator summary (replaces
  the F48-named-deferred wording PR-20 left as a placeholder).
  Does **not** supersede ADR-0009, ADR-0028, ADR-0032, ADR-0033,
  or any prior ADR; all remain in force.
- **Related:**
  - **ADR-0009 §5 Layer-2 idempotency** — the parent posture:
    "If the process crashed between `manageInvoice` returning and
    the `transactionId` being persisted (no Layer-1 record yet),
    the retry path **first calls `queryInvoiceCheck`** against the
    invoice number. If NAV already has it, **fetch the chain via
    `queryInvoiceData` and reconstruct local state.** If NAV does
    not have it, submit fresh." ADR-0033 realised the first half
    of this intent (the `queryInvoiceCheck` call + state-2
    skip-on-exists). ADR-0034 realises the second half (the chain
    fetch + local-state reconstruction). After PR-21 lands the
    §5 Layer-2 idempotency design intent is met end-to-end at the
    operator-driven level; the automatic equivalent (F45 — automatic
    state-2 retry loop) remains named-deferred.
  - **ADR-0033 §1** — three-phase posture for state-2
    `retry-submission`. On `Exists` outcome PR-20's
    `retry-submission` writes `InvoiceCheckPerformed(outcome=exists)`,
    skips the re-POST, and prints an operator-visible summary
    that named the chain-reconstruction gap loud (F48). ADR-0034
    closes that gap by introducing the `recover-from-nav` command
    the summary now points at.
  - **ADR-0033 §6** — Layer-2 entries are informational-only.
    `audit_query::stuck_precondition` does NOT consult
    `InvoiceCheckPerformed` entries. ADR-0034 PRESERVES this
    contract — `recover-from-nav` carries its own typed
    precondition checker (state-2 Pending AND most-recent
    `InvoiceCheckPerformed.outcome == "exists"`) instead of
    elevating Layer-2 to a classification-bearing fact in the
    shared walker. The §6 pin tests in PR-20 remain valid; the
    classifier's deliberate minimal scope is preserved per
    CLAUDE.md rule 3 (surgical changes).
  - **ADR-0033 §9 F48 named trigger** — "first PR that introduces
    a `recover-from-nav` operator command OR the deferred NAV
    historical/reconciliation read-path ADR (per ADR-0010
    §Deferred)." PR-21 fires the first half (the explicit
    operator-command surface); ADR-0010's deferred read-path
    ADR remains separate (a future `queryInvoiceDigest` /
    `queryInvoiceChainDigest` reconciliation surface).
  - **ADR-0028 §3 + §"Surfaced conflict 3"** — `queryInvoiceData`
    verbatim-bytes-first posture. PR-21 PRESERVES this for the
    receiver-confirmation field (which is what ADR-0028
    deliberately deferred parsing). The new
    `parse_audit_data_transaction_id` helper extracts a
    **different** field (the `<auditData>/<transactionId>`
    block — NAV's record of the original submission's
    transactionId) that is NOT the receiver-confirmation status
    field; the ADR-0028 §"Surfaced conflict 3" amendment trigger
    is therefore NOT fired by PR-21. The verbatim response_xml
    bytes still flow through `QueryInvoiceDataOutcome.response_xml`
    unchanged.
  - **ADR-0008 §"Storage"** — one-tx-per-state-change posture.
    PR-21's reconstruction writes one entry (the recovered
    `InvoiceSubmissionResponse`) in one DuckDB transaction.
    Same posture every prior Response-writing path uses; the
    reconstruction is its own state change (ABERP-side
    discovery that NAV has the prior submission and decision to
    record the recovered transactionId locally).
  - **ADR-0009 §8 audit-evidence bundle.** The recovered
    `InvoiceSubmissionResponse` entry's `response_xml` carries
    the verbatim `<QueryInvoiceDataResponse>` bytes — same
    posture every other NAV-bearing variant uses. The
    preceding `InvoiceCheckPerformed(outcome=exists)` entry +
    this entry form the provenance pair: a NAV inspector
    reading the bundle sees "ABERP confirmed NAV had the
    invoice (Layer-2), then queried the chain and recorded
    NAV's transactionId locally" as a coherent two-entry
    recovery trace. The `extract_nav_xml` exhaustive-match arm
    for `InvoiceSubmissionResponse` already exists from
    PR-7-B-3 — PR-21 does NOT amend `export_invoice_bundle.rs`.
  - **F8 idempotency-key contract.** The recovered
    `InvoiceSubmissionResponse` carries the same
    `idempotency_key` the prior `InvoiceSubmissionAttempt`
    (and the prior `InvoiceCheckPerformed`) carries — the
    original issuance's key per the F8 carry-forward
    contract. Reuses the existing
    `latest_submission_attempt`'s payload key lookup pattern;
    no new public surface on `audit_query`.
  - **Session-24 handoff §"Suggested next session sub-split"**
    — named PR-21 = F48 closure (Option A) as the strongest
    pick. ADR-0034 takes Option A per the handoff's lean.
    Options B (bundle verifier — F38), D (serve.rs derive_state
    extension — F21 + F47), and E (automatic state-2 retry loop
    — F45) remain named-deferred with their existing triggers.
- **Source material:** ADR-0009 §5 (the Layer-2 design intent —
  full), ADR-0028 §3 + §"Surfaced conflict 3" (the
  `queryInvoiceData` verbatim-bytes-first posture PR-21
  preserves), ADR-0033 §1 + §6 + §9 (the F48 named trigger +
  Layer-2 informational-only contract PR-21 preserves),
  `crates/nav-transport/src/operations/query_invoice_data.rs`
  (the PR-15 query module PR-21 additively extends with one
  new parse helper), `apps/aberp/src/observe_receiver_confirmation.rs`
  (the existing `queryInvoiceData` orchestration shape PR-21's
  new orchestration module structurally parallels),
  `apps/aberp/src/retry_submission.rs`'s Phase 0 loud-warned
  summary on `Exists` (the operator-facing message PR-21
  amends to point at `recover-from-nav`),
  `apps/aberp/src/audit_payloads.rs::InvoiceSubmissionResponsePayload`
  (the payload PR-21 reuses for the recovered Response entry —
  no schema change), `apps/aberp/src/audit_query.rs` (the
  precondition walker PR-21 does NOT amend at the classifier
  level — the §6 pin tests stay valid),
  `docs/research/nav-and-billingo.md` §"NAV Online Számla v3.0
  operations" line 103 (the `queryInvoiceData` invoice-by-
  number query) + §"Idempotency model" lines 161–184 (the
  network-reset disambiguation pattern ADR-0009 §5 codifies).

## Context

PR-20 / ADR-0033 closed F44 at the **state-2 disambiguation
level**: a state-2 Pending retry consults NAV via Layer-2
`queryInvoiceCheck` BEFORE the `manageInvoice` re-POST, writes
an `InvoiceCheckPerformed` audit entry recording the existence
check's outcome, and skips the re-POST on `outcome == "exists"`
to avoid a duplicate submission. PR-20's operator-visible
summary on the `Exists` outcome named the **second half** of
the ADR-0009 §5 Layer-2 intent loud-as-gap (F48):

> retry-submission Layer-2 Exists: NAV already has invoice X
> (NAV number Y) — re-POST skipped (no duplicate submission)
> (audit chain verified across N entries;
> InvoiceCheckPerformed recorded with outcome=exists); invoice
> remains state-2 Pending locally because the prior
> submission's Response/Ack chain is absent — the
> chain-reconstruction surface is named-deferred per F48
> (ADR-0033 §9); inspect NAV's web UI to view the prior
> submission's transactionId, or run `aberp mark-abandoned`
> locally to terminate the chain by operator decision

The gap is: after PR-20's Layer-2 check confirms NAV has the
invoice, ABERP's local audit ledger STILL lacks the
`InvoiceSubmissionResponse` entry that would link the local
invoice to NAV's transactionId. The operator must either
inspect NAV's web UI by hand (out-of-band, unverifiable) or
mark the invoice abandoned locally (terminates the chain by
operator decision; loses the link to NAV's record). Neither
is the chain-reconstruction surface ADR-0009 §5's full intent
names.

ADR-0034 closes this gap by introducing the
`aberp recover-from-nav` operator command that:

1. Verifies the precondition (state-2 Pending invoice with a
   prior `InvoiceCheckPerformed(outcome=exists)` audit
   entry) — loud-fail otherwise per CLAUDE.md rule 12.
2. Calls `queryInvoiceData` against the NAV-facing invoice
   number to fetch NAV's record of the invoice.
3. Parses the `<auditData>/<transactionId>` field from NAV's
   response to recover the original submission's NAV-assigned
   transactionId.
4. Writes ONE recovered `InvoiceSubmissionResponse` audit
   entry carrying:
   - The recovered transactionId,
   - The verbatim `<QueryInvoiceDataResponse>` bytes
     (provenance evidence; what NAV said and when),
   - The same F8 idempotency_key as the preceding
     `InvoiceCheckPerformed` and `InvoiceSubmissionAttempt`
     entries.
5. Verifies the audit chain after commit; syncs the mirror.
6. Prints an operator-visible summary that names the next
   step: run `aberp poll-ack` to drive the recovered chain
   to its terminal state via NAV's authoritative
   `queryTransactionStatus`.

After step 4 the precondition walker classifies the invoice
as **state-3 AwaitingAck** (the existing classifier rule:
"Response exists, no terminal ack" — step 2 in the walker
runs ahead of step 3). The operator's next move is
`aberp poll-ack` exactly as for any state-3 invoice.

### Why reconstruct only `InvoiceSubmissionResponse`, not `InvoiceAckStatus`

ADR-0009 §5's intent ("reconstruct local state") could be read
as "reconstruct everything the original submission would have
written — Response AND Ack". ADR-0034 rejects this maximal
reading per CLAUDE.md rule 12 (fail loud, don't fabricate
facts ABERP cannot itself verify):

- `queryInvoiceData` returns the **invoice data** plus an
  `<auditData>` block carrying the original submission's
  `transactionId`. It does NOT return the
  ack-status enumeration value (`RECEIVED` / `PROCESSING` /
  `SAVED` / `ABORTED`) — that surface lives on
  `queryTransactionStatus`.
- An invoice that exists at NAV per `queryInvoiceCheck` is
  **highly likely** to be `SAVED` (NAV's invoice-by-number
  store typically holds processed invoices), but the
  authoritative answer comes from `queryTransactionStatus`.
  Fabricating an `InvoiceAckStatus(ack_status=SAVED)` entry
  during recovery would mask the difference between
  ABERP-derived inference and NAV-authoritative fact.
- The operator's next move (`aberp poll-ack`) hits
  `queryTransactionStatus` against the recovered
  transactionId and produces the authoritative
  `InvoiceAckStatus` entry per the existing PR-7-C-2 path.
  ABERP records exactly what NAV said.

The chain after `recover-from-nav` + `poll-ack` is therefore:

```
InvoiceDraftCreated
InvoiceSubmissionAttempt        (original — wire-broke)
InvoiceCheckPerformed(exists)   (PR-20 — Layer-2 disambiguation)
InvoiceSubmissionResponse       (PR-21 — recovered from NAV)
InvoiceAckStatus(SAVED|...)     (operator's next poll-ack run)
```

A NAV inspector reading the bundle sees the recovery trace
explicitly: the absence of an `InvoiceRetryRequested` entry
between the `Attempt` and the `Response` (which a normal
`retry-submission` would have written) plus the presence of
the `InvoiceCheckPerformed(exists)` entry IS the provenance
marker. The bundle reader's chain walk can therefore
distinguish recovered-from-NAV Response entries from
originally-witnessed Response entries by entry order alone —
no payload schema change required.

### Prerequisite-gate state at PR-21 time

- **ADR-0009 §5 Layer-2 idempotency (first half — `queryInvoiceCheck`
  call + state-2 skip-on-exists).** MET — closed by PR-20.
- **ADR-0009 §5 Layer-2 idempotency (second half — chain fetch
  + local-state reconstruction).** UNMET. PR-21 closes this
  gap at the operator-driven `Response`-reconstruction level.
- **ADR-0033 §6** — Layer-2 entries are informational-only;
  precondition walker UNCHANGED. PRESERVED by PR-21. The §6
  pin tests in PR-20 (`check_performed_exists_does_not_change_state_2_classification`,
  `check_performed_absent_does_not_change_state_2_classification`,
  `check_performed_does_not_change_state_3_classification`,
  `already_abandoned_wins_over_check_performed_exists`) all
  stay valid — PR-21's classifier amendment is **zero**.
- **ADR-0028 §"Surfaced conflict 3"** — `queryInvoiceData`
  verbatim-bytes-first for the receiver-confirmation field.
  PRESERVED by PR-21. The new `parse_audit_data_transaction_id`
  helper extracts a different field (`<auditData>/<transactionId>`,
  NAV's record of the original submission's transactionId,
  unrelated to receiver-confirmation). The
  `QueryInvoiceDataOutcome` struct shape is UNCHANGED;
  `query_invoice_data_outcome_shape_has_no_parsed_status_field`
  pin test stays valid.
- **ADR-0032 §1** — two-tx Attempt-before-call posture.
  UNCHANGED. PR-21's recovery writes a Response WITHOUT a
  preceding fresh Attempt — the original Attempt (from the
  wire-broke submission) is the audit-evidence of "ABERP
  tried to submit". The recovered Response says "NAV
  records that submission with transactionId X". Same
  Attempt + Response pairing the bundle reader expects;
  the only divergence is the timing (the recovery's
  Response is written long after the original Attempt).

### What surfaced during PR-21 design

Three conflicts among prior-PR conventions plus six
adversarial-review concerns surfaced during the design pass.

**Surfaced conflict 1: Does `recover-from-nav` reconstruct
`InvoiceAckStatus` too, or only `InvoiceSubmissionResponse`?**
Three readings:

- **Reading A — Reconstruct both: write `InvoiceSubmissionResponse`
  with the recovered transactionId AND `InvoiceAckStatus(SAVED)`
  inferred from the queryInvoiceCheck `Exists` + queryInvoiceData
  success.** Trade-off: the local chain is complete after one
  operator command; no follow-up `poll-ack` needed. Rejected
  per CLAUDE.md rule 12 — the inferred SAVED is not NAV-
  authoritative (queryInvoiceData success could conceivably
  return data for PROCESSING invoices; NAV-testbed verification
  has not surfaced the actual semantics). Fabricating SAVED
  would mask the inference-vs-fact distinction.
- **Reading B — Reconstruct only `InvoiceSubmissionResponse`;
  operator runs `poll-ack` next for authoritative ack
  status.** Trade-off: requires two operator commands;
  matches the operator-facing-twin pattern every other
  multi-step audit-recovery flow uses. Accepted — fails
  loud on the inference question; reuses the existing
  poll-ack path verbatim; no fabricated audit entries.
- **Reading C — Reconstruct nothing; surface the recovered
  transactionId in the operator-visible summary only, no
  audit entry.** Trade-off: the audit chain remains state-2
  Pending forever; the operator's recovery action leaves no
  audit trail. Rejected — the entire point of F48 is to
  produce the audit-bearing record of the recovery; a
  print-only surface would defeat the purpose.

**Decision: Reading B.** PR-21 writes one
`InvoiceSubmissionResponse` audit entry carrying the
recovered transactionId; the operator runs `aberp poll-ack`
next for authoritative ack status. The operator-visible
summary names the follow-up step explicitly.

**Surfaced conflict 2: Should the recovered
`InvoiceSubmissionResponse` be distinguishable from an
originally-witnessed one by payload schema?** Three readings:

- **Reading A — Reuse existing `InvoiceSubmissionResponse`
  EventKind + payload shape; rely on the preceding
  `InvoiceCheckPerformed(outcome=exists)` entry as the
  provenance marker.** Trade-off: no schema change; F12
  ritual does NOT fire; the bundle reader's chain walk
  distinguishes by entry order. Accepted — matches the
  ADR-0028 verbatim-evidence-first pattern (the Response
  entry's `response_xml` carries the verbatim NAV bytes,
  which are themselves a `<QueryInvoiceDataResponse>`
  envelope — structurally distinct from the
  `<ManageInvoiceResponse>` an originally-witnessed
  Response would carry).
- **Reading B — Add a typestate field
  (`recovered_from_nav: bool`) to `InvoiceSubmissionResponsePayload`.**
  Trade-off: schema change without a new EventKind (additive
  via `#[serde(default)]`); the bundle reader inspects the
  field directly without entry-order walk. Rejected — the
  preceding `InvoiceCheckPerformed(outcome=exists)` is a
  more general provenance marker (also useful for future F45
  automatic recovery, for F49 Layer-2-aware abandon-warn,
  and for any future inspector that walks the chain by
  payload kind). Reading A's chain-walk-by-order is
  structurally sufficient; the payload-field option is
  speculative abstraction per CLAUDE.md rule 2.
- **Reading C — Add a new EventKind variant
  (`InvoiceSubmissionResponseRecovered` —
  F12 ritual fires once).** Trade-off: bundle reader filters
  by kind without inspecting payload. Rejected — three sub-
  types of "ABERP recorded NAV's transactionId for invoice
  X" (originally-witnessed, recovered-from-Layer-2,
  hypothetical future-recovered-from-other-channel) are not
  structurally distinct events; same posture as ADR-0033
  §"Surfaced conflict 2 Reading A" rejected. F12 ritual
  cost is real (eleven landings to date) and should not
  fire for a soft semantic distinction.

**Decision: Reading A.** No EventKind variant, no payload
schema change, no F12 ritual firing. The
`InvoiceCheckPerformed(outcome=exists)` entry directly
preceding the recovered `InvoiceSubmissionResponse` is the
provenance marker. The bundle reader's chain walk in entry
order distinguishes the two cases.

**Surfaced conflict 3: Should `audit_query::stuck_precondition`
classify state-2 + Exists as a new stage / new NotStuck
reason?** Three readings:

- **Reading A — Add a new `StuckStage::AwaitingRecovery`
  variant; the precondition walker classifies state-2 +
  Exists as it; `retry-submission` rejects this stage and
  steers the operator at `recover-from-nav`; the §6 pin
  tests in PR-20 get updated to expect the new
  classification.** Trade-off: type-system-driven routing
  of operator commands per precondition; cleaner separation
  of "retry vs recover" intent. Rejected — pre-empts
  questions ADR-0033 §6 explicitly named as deliberate
  minimal-scope: "the state-2 → not-stuck transition is the
  F48-deferred recover-from-nav surface; PR-20 explicitly
  does not pre-empt F48's design." A new stage at
  classification-bearing scope is exactly the pre-emption
  ADR-0033 declined. The PR-20 §6 pin tests are
  load-bearing contract evidence; PR-21 ratifies them
  rather than amending them.
- **Reading B — Keep `audit_query::stuck_precondition`
  unchanged; `recover-from-nav` carries its own typed
  precondition checker (state-2 Pending AND most-recent
  `InvoiceCheckPerformed.outcome == "exists"`) that
  loud-fails on every non-recoverable shape; the §6 pin
  tests stay valid.** Trade-off: small duplication of
  "walk the ledger for the latest InvoiceCheckPerformed"
  logic inside `recover_from_nav.rs`; matches the
  operator-facing-twin posture (CLAUDE.md rule 2 — two
  callers, a third caller would prompt extraction).
  Accepted — preserves PR-20's §6 contract; surgical
  scope per CLAUDE.md rule 3.
- **Reading C — Make the precondition walker consult
  `InvoiceCheckPerformed` and return a NEW `NotStuck`
  reason `StateRecoveryPending`.** Trade-off: similar to
  Reading A but at the `NotStuck` axis instead of the
  Stuck-stage axis. Rejected for the same reason —
  pre-empts F48's design space that ADR-0033 §6
  deliberately left open.

**Decision: Reading B.** PR-21 does NOT amend
`audit_query::stuck_precondition`. The classifier remains
informational-only with respect to Layer-2 entries per
ADR-0033 §6. `recover-from-nav` carries its own
precondition checker; the precondition walker remains the
shared truth for state-2/state-3/terminal classification
and the §6 pin tests stay valid.

## Decision

### 1. New `aberp recover-from-nav` operator command

A new CLI subcommand `recover-from-nav` with the same five-
field shape as `observe-receiver-confirmation`:

| Flag | Type | Notes |
|---|---|---|
| `--invoice-id` | `String` | Prefixed `inv_<ULID>` form |
| `--tax-number` | `String` | Same parser as every NAV-touching command |
| `--db` | `PathBuf` | Default `./aberp.duckdb` |
| `--tenant` | `String` | Default `default` |
| `--endpoint` | `NavEnv` | Explicit per ADR-0020 §1 |

No `--reason` flag. The audit-evidence is the chain itself
(the preceding `InvoiceCheckPerformed(outcome=exists)` plus
the recovered `InvoiceSubmissionResponse`); a free-form
reason would not add inspector-visible value because the
recovery is not a choice between alternatives — it is the
mechanical reconstruction of state NAV already has. CLAUDE.md
rule 2: no speculative abstractions.

### 2. Pipeline

The orchestration pipeline (mirror of
`observe_receiver_confirmation::run` per the operator-facing-
twin posture):

1. Parse + validate CLI args (tenant; tax-number; endpoint).
2. Load `NavCredentials` from the OS keychain (loud-fail on
   missing).
3. Open tenant DuckDB; load the previously-issued invoice +
   idempotency_key from the billing store (scoped read tx;
   same shape as `submit_invoice::run` / `retry_submission::run`).
4. Resolve the typed `recover-from-nav` precondition (in-
   module helper):
   - Resolve `audit_query::stuck_precondition` — require
     `Stuck(StuckStage::Pending)`. Loud-fail with a typed
     error message on every other classification.
   - Walk the audit ledger for the most-recent
     `InvoiceCheckPerformed` entry whose payload's
     `invoice_id` matches; require it exists and its
     `outcome == "exists"`. Loud-fail otherwise (steers the
     operator to run `aberp retry-submission` first so
     PR-20's Phase 0 Layer-2 check produces the
     `InvoiceCheckPerformed(outcome=exists)` evidence).
5. Construct the NAV-facing invoice number string
   (`"{series_code}/{seq:05}"`) from the loaded invoice's
   series — same helper shape `retry_submission`'s
   `derive_nav_invoice_number` and
   `observe_receiver_confirmation::load_base_nav_invoice_number`
   use.
6. Build a tokio current-thread runtime and drive ONE
   `queryInvoiceData` call (per ADR-0028 §4's one-shot
   posture; receiver-confirmation is not the surface here,
   but the same one-shot principle applies — recover-from-nav
   does NOT loop). The call reuses the existing
   `query_invoice_data::call` per ADR-0028 §3 (verbatim
   request + response bytes).
7. Parse `<auditData>/<transactionId>` from the verbatim
   response bytes via the new
   `query_invoice_data::parse_audit_data_transaction_id`
   helper. Loud-fail if the field is missing — that surfaces
   either a NAV-side response-shape divergence (named trigger
   for an amendment ADR) or a NAV record without the audit
   block (operator surface).
8. Under one DuckDB transaction, append ONE
   `InvoiceSubmissionResponse` audit entry carrying the
   recovered transactionId + the verbatim
   `<QueryInvoiceDataResponse>` bytes + the F8 idempotency
   key from the precondition. Commit. Sync the audit-ledger
   mirror.
9. Verify the audit chain after commit (success-criterion
   gate).
10. Print the operator-visible summary naming the recovery
    result + the next step: `aberp poll-ack` to drive the
    recovered chain to its terminal state.

### 3. `parse_audit_data_transaction_id` helper

A new public function on
`crates/nav-transport/src/operations/query_invoice_data.rs`:

```rust
pub fn parse_audit_data_transaction_id(
    response_xml: &[u8],
) -> Result<String, NavTransportError>
```

Extracts the first `<transactionId>` element from a verbatim
`<QueryInvoiceDataResponse>` body. The NAV v3.0 spec places
this element inside the `<auditData>` block; the
`find_first_text` helper (already used by every other
operations module — see e.g.
`token_exchange::call`'s `encodedExchangeToken` extraction
and `manage_invoice::call`'s `transactionId` extraction) is
the canonical extractor.

Returns `QueryInvoiceDataResponseParse(...)` on:
- Element missing from the body.
- Element present but empty.
- XML parse failure (delegated to `find_first_text`'s
  Result shape).

The `call(...) -> QueryInvoiceDataOutcome` surface is
**unchanged**. PR-21 does NOT add a parsed field to
`QueryInvoiceDataOutcome`. The verbatim-bytes posture per
ADR-0028 §3 stays intact; PR-21's helper is invoked by the
orchestration layer ONLY when the recovery flow needs the
parsed transactionId. The
`query_invoice_data_outcome_shape_has_no_parsed_status_field`
pin test in PR-15 stays valid.

### 4. Reuse of `InvoiceSubmissionResponsePayload`

The recovered Response entry uses the existing payload
shape:

```rust
audit_payloads::InvoiceSubmissionResponsePayload::new(
    invoice_id,           // from the precondition
    idempotency_key,      // from the precondition (F8)
    recovered_txid,       // parsed from queryInvoiceData
    response_xml,         // verbatim <QueryInvoiceDataResponse> bytes
)
```

No new constructor. No payload schema change. The
`response_xml` field, which carries
`<ManageInvoiceResponse>` bytes on the originally-witnessed
path, carries `<QueryInvoiceDataResponse>` bytes on the
recovered path — both are NAV-emitted XML; both serve as
audit-bearing evidence. The bundle reader's chain walk in
entry order distinguishes the two cases by:

- The preceding entry kind on the recovered path is
  `InvoiceCheckPerformed`, not `InvoiceRetryRequested` or
  `InvoiceSubmissionAttempt`.
- The verbatim XML root element in the `response_xml` field
  differs (`QueryInvoiceDataResponse` vs
  `ManageInvoiceResponse`) — a future bundle-verifier
  (F38-named) can additively pin this distinction without
  ABERP's writer-side discipline changing.

### 5. `audit_query::stuck_precondition` UNCHANGED

Per §"Surfaced conflict 3 Reading B", PR-21 does NOT amend
the precondition walker. The §6 pin tests in PR-20
(`check_performed_exists_does_not_change_state_2_classification`,
`check_performed_absent_does_not_change_state_2_classification`,
`check_performed_does_not_change_state_3_classification`,
`already_abandoned_wins_over_check_performed_exists`) all
stay valid.

`recover_from_nav.rs` carries its own typed precondition
checker:

```rust
fn resolve_recovery_precondition(
    ledger: &Ledger,
    invoice_id: &str,
    issuance_idempotency_key: &IdempotencyKey,
) -> Result<RecoveryPrecondition>
```

The checker:

1. Calls `audit_query::stuck_precondition`. Requires
   `Stuck(StuckStage::Pending)`. Loud-fails with the typed
   message on every other outcome.
2. Walks the ledger for the most-recent
   `InvoiceCheckPerformed` entry whose payload's
   `invoice_id` matches. Requires it exists. Loud-fails
   with a message steering the operator to run
   `aberp retry-submission` first (so Phase 0 produces the
   `InvoiceCheckPerformed(outcome=exists)` evidence).
3. Requires the `outcome` field equals `"exists"`. Loud-fails
   with a typed message on `"absent"` (the prior retry
   already proceeded to re-POST; the invoice's state-2
   shape means the re-POST itself failed — the operator's
   next move is another `retry-submission` run, not
   `recover-from-nav`) or `"failure"` (the prior Layer-2
   check itself failed; the operator's next move is another
   `retry-submission` run to retry the Layer-2 check, not
   `recover-from-nav`).
4. Returns `RecoveryPrecondition { idempotency_key,
   nav_invoice_number_from_check }` so the orchestration
   has every field the recovery write needs without
   re-walking the ledger.

The F8 idempotency-key cross-check happens in step 1 (per
the same posture `retry_submission::resolve_stuck_or_loud_fail`
uses): the precondition's `idempotency_key` must match the
billing row's issuance key. Loud-fail otherwise per
CLAUDE.md rule 12 (ledger tamper detection).

### 6. `retry_submission` operator-visible summary amendment

`retry_submission.rs`'s state-2 + Exists branch (PR-20 / ADR-0033 §1) currently prints:

> ... invoice remains state-2 Pending locally because the
> prior submission's Response/Ack chain is absent — the
> chain-reconstruction surface is named-deferred per F48
> (ADR-0033 §9); inspect NAV's web UI to view the prior
> submission's transactionId, or run `aberp mark-abandoned`
> locally to terminate the chain by operator decision

PR-21 amends this to:

> ... invoice remains state-2 Pending locally because the
> prior submission's Response/Ack chain is absent — run
> `aberp recover-from-nav --invoice-id <id> --tax-number
> ... --endpoint {test|production}` to reconstruct the
> local InvoiceSubmissionResponse from NAV's
> queryInvoiceData (ADR-0034 / PR-21), then `aberp poll-ack`
> to drive the terminal state; or `aberp mark-abandoned`
> locally to terminate the chain by operator decision.

The substring `"the chain-reconstruction surface is named-
deferred per F48"` is removed; the `recover-from-nav`
pointer replaces it. The `mark-abandoned` alternative is
preserved (the operator may still choose to abandon
locally — `recover-from-nav` is the affirmative recovery
path, not the only path).

### 7. `mark-abandoned` UNCHANGED

ADR-0034 does NOT amend `mark-abandoned`. F49 (Layer-2-aware
mark-abandoned that warns before accepting a state-2
abandonment when NAV has the invoice) remains named-deferred
with its existing trigger ("first operator request OR first
incident where a state-2 + Exists invoice was abandoned
without operator awareness that NAV had a copy").

ADR-0034 surfaces the operator's alternatives — recover or
abandon — in the `retry-submission` summary; the
mark-abandoned command itself stays operator-decision-respecting
(no NAV consultation, no auto-warn). The audit ledger records
the operator's choice either way.

### 8. `drain-submission-queue` UNCHANGED

Drain remains strict per ADR-0032 §5 + ADR-0033 §
"Surfaced conflict 3 Reading B". State-2 invoices (whether
or not they carry an `InvoiceCheckPerformed` entry) are
excluded from drain. F45 (automatic state-2 retry loop)
retains its existing named-trigger.

### 9. Deferred scope

Two sub-surfaces are NAMED here and DEFERRED per CLAUDE.md
rule 2 + ADR-0021's just-in-time-ADR posture. Each has a
named trigger.

- **F49 — Layer-2-aware `mark-abandoned`.** Unchanged from
  ADR-0033 §9. Named trigger: first operator request OR
  first incident where a state-2 + Exists invoice was
  abandoned without operator awareness that NAV had a copy.
- **F50 — Operator-tunable `queryInvoiceCheck` /
  `queryInvoiceData` rate-limit cooldown.** Unchanged from
  ADR-0033 §9. Named trigger: first operator incident where
  rate-limit responses are observed.

### 10. F38 (bundle verifier) interaction

The recovered `InvoiceSubmissionResponse` entry carries a
`response_xml` body whose XML root element is
`<QueryInvoiceDataResponse>` rather than
`<ManageInvoiceResponse>`. A future bundle verifier
(F38-named) that pins root-element-by-EventKind would
either:

- Accept both root elements for the
  `InvoiceSubmissionResponse` kind (recommended — preserves
  the ADR-0034 §4 chain-walk-by-order posture).
- Use the preceding entry (Attempt vs CheckPerformed) to
  branch the expected root element (more rigid — re-asserts
  the provenance distinction at verifier-side).

PR-21 does NOT pre-empt F38's design. The named trigger for
F38 (per session-22 / PR-18 + session-24 / PR-20 handoff
text) covers this; the verifier ADR will decide the
posture when it lands.

## Consequences

**Positive**

- ADR-0009 §5's full Layer-2 idempotency intent is met
  end-to-end at the operator-driven level. The
  network-reset-disambiguation pattern named in
  `docs/research/nav-and-billingo.md` lines 177–181 is now
  fully implemented in ABERP for state-2 retries: Layer-2
  check (PR-20) followed by chain reconstruction (PR-21).
- The audit-evidence chain after `recover-from-nav` +
  `poll-ack` carries inspector-visible provenance: the
  `InvoiceCheckPerformed(outcome=exists)` entry preceding
  the recovered Response IS the marker that the Response
  came from queryInvoiceData rather than from a fresh
  manageInvoice POST. No payload schema change required;
  no F12 ritual firing.
- The `queryInvoiceData` operation gains one parsed field
  (`auditData.transactionId`) without violating
  ADR-0028's verbatim-bytes-first posture — the existing
  `call` / `QueryInvoiceDataOutcome` surface is unchanged;
  the parse is invoked by the orchestration layer on the
  recovery path only.
- The precondition walker (`audit_query::stuck_precondition`)
  remains the shared truth for state-2/state-3/terminal
  classification. PR-20's §6 pin tests stay valid; the
  Layer-2-informational-only contract is ratified rather
  than amended.
- The reconstruction is operator-driven, explicit, and
  fail-loud at every step. No automatic recovery (F45's
  surface is separate); no fabricated ack status; no
  silent state divergence. CLAUDE.md rule 12 honoured.

**Negative**

- The recovery requires TWO operator commands (`recover-from-nav`
  then `poll-ack`) to drive a state-2 + Exists invoice to
  its terminal state. A single-command recovery (Reading A
  in §"Surfaced conflict 1") would be faster but would
  fabricate inferred state. The two-command flow is the
  fail-loud cost.
- The reconstructed `InvoiceSubmissionResponse` carries
  `<QueryInvoiceDataResponse>` bytes in `response_xml`
  rather than `<ManageInvoiceResponse>` bytes. A future
  bundle verifier that pins root-element-by-EventKind must
  accept both shapes (or branch on the preceding entry).
  Per §10, the named-deferred F38 ADR handles this
  when it lands; PR-21 does not pre-empt.
- `audit_query.rs` is unchanged but `recover_from_nav.rs`
  duplicates a small amount of ledger-walking logic for
  the InvoiceCheckPerformed lookup (mirror of
  `observe_receiver_confirmation::extract_receiver_confirmation_inputs`).
  Two callers of "walk the ledger for the latest
  Layer-2 entry" exist after PR-21 (the precondition checker
  here and, conceivably, a future F49 Layer-2-aware
  mark-abandoned); a third caller would prompt
  extraction to a shared helper in `audit_query.rs`.
  Operator-facing-twin posture per CLAUDE.md rule 2.
- `queryInvoiceData`'s wire shape (the `<auditData>/<transactionId>`
  element name + position) is pinned by structural
  inference from the NAV v3.0 spec + the structural-
  parallel posture against `manageInvoice`'s and
  `manageAnnulment`'s `<transactionId>` extraction. NAV-
  testbed verification is the named trigger for amendment
  if the actual response shape differs from the modelled
  one. The strict-parse loud-fail posture (mirror of
  ADR-0033 §"Adversarial review" #7) catches the
  divergence without silent miscoercion.

**Locked in**

- The recovered `InvoiceSubmissionResponse` entry, once
  written, is immutable per ADR-0008 (the hash chain locks
  it in). A future ADR that changes the Response payload
  schema (e.g., adds a `recovered_from_nav: bool` typestate
  field — Reading B in §"Surfaced conflict 2") must keep
  the existing entries valid for historical reading; the
  current entries' absence of the field deserialises as
  `false` (the originally-witnessed default) via serde's
  `#[serde(default)]`. The amendment is therefore additive
  and backward-compatible if/when it lands.
- The `recover-from-nav` command's chain-reconstruction
  semantic is locked to "one recovered Response per
  invocation, derived from one queryInvoiceData call". A
  future operational pattern that wants to batch-recover
  multiple invoices (e.g., after a multi-day NAV outage
  that left dozens of state-2 + Exists invoices) would
  either compose multiple invocations (existing path) or
  file a new ADR for a `bulk-recover-from-nav` surface.
- The audit-evidence bundle's chain walk by entry order
  becomes load-bearing for distinguishing recovered-from-
  NAV Response entries from originally-witnessed ones.
  Future entries between an `InvoiceCheckPerformed(exists)`
  and the subsequent `InvoiceSubmissionResponse` would
  break the provenance signal; ADR-0034 names this as a
  contract on the orchestration writer-side (recover-from-
  nav writes nothing between the two).

## Adversarial review

A hostile NAV inspector and a hostile-engineer review, in
alternation.

1. **"You're reconstructing local state from NAV's record.
   What stops ABERP from getting NAV's record wrong — say,
   parsing the wrong transactionId because the
   `<auditData>` block format differs from your model?"**
   `parse_audit_data_transaction_id` is strict per
   CLAUDE.md rule 12: it extracts the first
   `<transactionId>` element via `find_first_text` and
   loud-fails if absent or empty. The verbatim
   `<QueryInvoiceDataResponse>` bytes are persisted on the
   recovered Response entry BEFORE the parse runs — a
   parse-side bug therefore cannot drop the audit evidence;
   the inspector reading the bundle has access to NAV's
   verbatim bytes and can verify the recovery by hand.
   NAV-testbed verification is the named trigger for an
   amendment ADR if the actual element position or name
   differs from the modelled one.

2. **"What if `queryInvoiceData` returns data for an
   invoice that's still PROCESSING (not yet SAVED at
   NAV)?"** ADR-0034 deliberately does NOT fabricate an
   `InvoiceAckStatus` entry from `queryInvoiceData` success
   for exactly this reason (§"Surfaced conflict 1
   Reading A" rejected). The recovered
   `InvoiceSubmissionResponse` carries the transactionId
   only; the operator's next step (`aberp poll-ack`) hits
   `queryTransactionStatus` against that transactionId
   and produces the authoritative `InvoiceAckStatus`. If
   NAV's poll returns PROCESSING, the operator's chain
   walks PROCESSING → SAVED via the existing bounded poll
   loop (PR-7-C-2). No fabricated SAVED entries are
   written.

3. **"What if the operator runs `recover-from-nav`
   against an invoice whose last `InvoiceCheckPerformed`
   was `outcome=absent`?"** The typed precondition checker
   loud-fails: "the most-recent InvoiceCheckPerformed for
   this invoice has outcome=absent — NAV does not have the
   invoice; run `aberp retry-submission` to re-POST under
   the existing Layer-2 disambiguation flow (the
   precondition for recover-from-nav is outcome=exists)."
   The operator-visible message names the right next step;
   the audit ledger does not accumulate a half-failed
   recovery entry.

4. **"What if the operator runs `recover-from-nav` against
   an invoice that has no prior `InvoiceCheckPerformed`
   entry at all?"** Same path: loud-fail with a message
   steering the operator to run `aberp retry-submission`
   first so Phase 0 produces the
   `InvoiceCheckPerformed(outcome=exists)` evidence. The
   recover-from-nav surface is downstream of Phase 0; it
   does not run a fresh Layer-2 check itself (that
   responsibility lives on retry-submission per the
   command-decomposition posture of CLAUDE.md rule 3).

5. **"You're trusting that the most-recent
   `InvoiceCheckPerformed(outcome=exists)` reflects NAV's
   current state. What if NAV deleted the invoice between
   the Layer-2 check and the recovery run?"** Such a
   scenario is operationally rare (NAV does not delete
   invoices once SAVED — that would require a separate
   annulment + receiver-confirmation flow), but if it
   did occur, the `recover-from-nav` orchestration's
   own `queryInvoiceData` call would surface the
   divergence: NAV returns an `ERROR` funcCode +
   `INVOICE_NOT_FOUND`-class application error (or
   equivalent NAV-side code per the v3.0 spec). The
   `query_invoice_data::call` path classifies this as
   `QueryInvoiceDataNonRetryable`; the orchestration
   loud-fails with the typed message naming the divergence;
   no recovered Response entry is written. The operator's
   next move is investigation (NAV web UI / accountant
   consultation), not a retry of recover-from-nav.

6. **"What's stopping an operator from running
   `recover-from-nav` in a tight loop on the same invoice
   and getting multiple recovered Response entries with
   the same transactionId?"** Once the first
   `recover-from-nav` run succeeds, the recovered
   `InvoiceSubmissionResponse` entry is in the audit
   ledger. The precondition walker (`audit_query::stuck_precondition`)
   then classifies the invoice as state-3 AwaitingAck (step
   2 in the classifier — Response wins). A second
   `recover-from-nav` run's precondition checker requires
   `Stuck(StuckStage::Pending)`, which fails on the
   state-3 invoice; the second run loud-fails with a typed
   message steering the operator to run `aberp poll-ack`.
   No duplicate Response entries can accumulate via
   recover-from-nav. (A future operational anomaly that
   produces them would be flagged by the chain-verify
   gate the bundle reader runs per ADR-0029 §6.)

## Alternatives considered

- **Reconstruct `InvoiceAckStatus(SAVED)` alongside
  `InvoiceSubmissionResponse` in a single
  recover-from-nav invocation.** Rejected per §"Surfaced
  conflict 1 Reading A". CLAUDE.md rule 12 — don't
  fabricate facts ABERP cannot itself verify; the
  operator's follow-up `poll-ack` produces the
  authoritative ack status.
- **Make `recover-from-nav` an automatic follow-up to
  `retry-submission`'s state-2 + Exists branch.** Rejected
  per CLAUDE.md rule 3 (surgical scope) + the session-24
  handoff's lean for explicit operator invocation. Each
  recovery is an operator decision; the automatic-loop
  equivalent (F45) is the named-deferred surface.
- **Add a `recovered_from_nav: bool` field to
  `InvoiceSubmissionResponsePayload`.** Rejected per
  §"Surfaced conflict 2 Reading B". The preceding
  `InvoiceCheckPerformed(outcome=exists)` entry is a more
  general provenance marker than a single-purpose
  payload field; the chain-walk-by-order approach is
  structurally sufficient.
- **Add a new EventKind `InvoiceSubmissionResponseRecovered`
  (F12 ritual fires once).** Rejected per §"Surfaced
  conflict 2 Reading C". F12 ritual cost is real (eleven
  landings to date); the recovered Response is not
  structurally distinct from an originally-witnessed
  Response at the audit-evidence level.
- **Amend `audit_query::stuck_precondition` to classify
  state-2 + Exists as a new stage.** Rejected per
  §"Surfaced conflict 3 Reading A / Reading C". Pre-empts
  the design space ADR-0033 §6 deliberately left open;
  the §6 pin tests are load-bearing contract evidence.
- **Use `queryInvoiceChainDigest` instead of
  `queryInvoiceData` for the chain fetch.** Rejected —
  `queryInvoiceChainDigest` is paginated and shaped for
  multi-invoice chain traversal (base + every
  amendment/storno). The recover-from-nav surface needs
  ONE invoice's prior transactionId; `queryInvoiceData`
  is the right granularity. A future
  reconciliation-side ADR (per ADR-0010 §Deferred) may
  introduce the digest path for batch operations; PR-21
  does not pre-empt.
- **Add `--reason` flag for the operator's recovery
  justification.** Rejected per CLAUDE.md rule 2 (no
  speculative abstractions). The recovery is mechanical —
  there is no choice between alternative recoveries that
  a reason text would disambiguate. The audit-evidence
  chain (InvoiceCheckPerformed + recovered Response) is
  itself the justification.

## Open questions

The full list of cross-cutting open questions is consolidated
in `docs/research/nav-and-billingo.md`; the items below
specifically block work that ADR-0034 touches:

- **NAV-testbed verification of `queryInvoiceData`'s
  `<auditData>/<transactionId>` shape.** The modelled
  position (`<auditData>` block containing `<transactionId>`)
  is drawn from the v3.0 spec + the structural-parallel
  posture against `manageInvoice`'s and `manageAnnulment`'s
  response-side `<transactionId>` extraction. NAV-testbed
  verification is the named trigger for amendment if the
  actual element position differs.
- **`queryInvoiceData` PROCESSING-state semantics.**
  Whether NAV returns full invoice data for invoices in
  PROCESSING state (vs only SAVED) is unverified. Today
  ADR-0034 sidesteps the question by not fabricating an
  `InvoiceAckStatus` — the operator's `poll-ack` produces
  the authoritative status. A future operational pattern
  may want to surface this inference explicitly; out of
  PR-21 scope.
- **F38 (bundle verifier) — chain-walk-by-order
  contract.** ADR-0034 §4's "the preceding entry kind
  distinguishes recovered from originally-witnessed
  Response entries" relies on the orchestration writer-
  side discipline (recover-from-nav writes nothing
  between the `InvoiceCheckPerformed(exists)` and the
  recovered Response). A future bundle verifier should
  pin this contract; ADR-0034 names it but does not
  enforce it at writer-side via a new EventKind or
  payload field.

## Follow-on ADRs unblocked by this decision

- **ADR — Automatic state-2 retry loop (F45 closure).**
  Now Layer-2-aware AND chain-reconstruction-aware. The
  automatic loop's per-invoice driver could chain
  Phase 0 → Phase 1+2 (Absent path) → or →
  recover-from-nav (Exists path) → poll-ack (terminal
  state). The automatic-vs-operator-driven distinction
  remains the load-bearing design question.
- **ADR — Layer-2-aware `mark-abandoned` (F49
  closure).** Now has a clear alternative recovery path
  to point operators at (recover-from-nav). The Layer-2-
  aware mark-abandoned would warn: "NAV has this invoice;
  consider `aberp recover-from-nav` to record the
  transactionId locally before abandoning."
- **ADR — Bundle verifier tool (F38 closure).** Now
  must accept two root-element shapes for
  `InvoiceSubmissionResponse` entries
  (`<ManageInvoiceResponse>` and
  `<QueryInvoiceDataResponse>`) OR branch on the
  preceding entry kind to pin the expected root.
- **ADR — NAV historical / reconciliation read path
  (per ADR-0010 §Deferred).** The
  `queryInvoiceDigest` / `queryInvoiceChainDigest` /
  `queryTransactionList` surface for batch
  reconciliation. Reuses the
  `parse_audit_data_transaction_id` posture
  ADR-0034 establishes (additive parse on a
  verbatim-bytes operation module).
- **ADR — Operator-tunable threshold config (F42 +
  F46 + F50 joint closure).** Unchanged from
  ADR-0033's named-deferred shape.
