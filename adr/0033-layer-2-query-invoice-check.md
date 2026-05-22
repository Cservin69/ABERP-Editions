# ADR-0033 — Layer-2 `queryInvoiceCheck` reconciliation — state-2 Pending retries consult NAV BEFORE re-POSTing so a transport-mid-flight loss that DID reach NAV does not produce a duplicate submission; a new `InvoiceCheckPerformed` `EventKind` records the existence-check result (typed `outcome` discriminator with three values: `"exists"`, `"absent"`, `"failure"`); the `nav-transport` crate gains a `queryInvoiceCheck` operation under `operations/query_invoice_check.rs` (one new module, five new public surfaces; same `build_request` + `send_built_request` split as ADR-0032 §3 — no `call` wrapper because there are no pre-existing callers); `retry-submission`'s state-2 path becomes "query NAV first, then proceed iff NAV does NOT have the invoice"; `retry-submission`'s state-3 path stays unchanged (NAV's Layer-1 `INVOICE_NUMBER_NOT_UNIQUE` guard already covers state-3); `drain-submission-queue` stays unchanged (the fourth-predicate clause from ADR-0032 §5 excludes Attempted invoices from drain — Layer-2 would never disambiguate anything new there); the `submission_queue::classify_attempt_failure` helper extends with the five new `QueryInvoiceCheck*` `NavTransportError` arms; `mark-abandoned` stays unchanged for PR-20 (a future Layer-2-aware mark-abandoned named-deferred); closes F44 at the state-2 disambiguation level; the post-positive-check NAV-side state recovery (fetching the chain and writing the missing Response + AckStatus entries) remains deferred to its own ADR as F48

- **Status:** Accepted
- **Date:** 2026-05-22
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — first PR after
  ADR-0032 / PR-19 to introduce Layer-2 NAV-side reconciliation
  per ADR-0009 §5's named-deferred surface. Closes F44 at the
  state-2 disambiguation level (the duplicate-submission residual
  PR-19's adversarial review #2 named-warned). The post-positive-
  check NAV-side state recovery (a `recover-from-nav` operator
  command that fetches the chain via `queryInvoiceData` and
  reconstructs the missing local `InvoiceSubmissionResponse` +
  `InvoiceAckStatus` entries per ADR-0009 §5's belt-and-braces
  intent) remains deferred to its own ADR as F48.
  Audit-ledger crate gains one new `EventKind` variant
  (`InvoiceCheckPerformed`) and one new on-disk string
  (`invoice.check_performed`); the F12 four-coordinated-edit
  ritual fires once. The binary's `apps/aberp/src/audit_payloads.rs`
  gains one new payload type (`InvoiceCheckPerformedPayload`).
  The `crates/nav-transport` crate gains one new operations module
  (`query_invoice_check.rs`) carrying two new public functions
  (`build_request` + `send_built_request`), one new public struct
  (`SendBuiltRequestOutcome`), one new public outcome enum
  (`QueryInvoiceCheckOutcome` — `Exists` / `Absent`), and the
  matching SOAP renderer in `soap::render_query_invoice_check_request`
  + five new `NavTransportError` variants. The binary's
  `apps/aberp/src/retry_submission.rs` shifts to a three-phase
  posture for state-2 (TX0 = InvoiceCheckPerformed; conditional
  early-exit on `Exists`; TX1 = RetryRequested + Attempt per the
  pre-PR-20 shape iff `Absent`; TX2 = Response or AttemptFailed
  per the pre-PR-20 shape iff TX1 fired); state-3 retains the
  PR-19 two-tx posture verbatim. The binary's
  `apps/aberp/src/submission_queue.rs::classify_attempt_failure`
  helper extends with five new arms for the new
  `NavTransportError::QueryInvoiceCheck*` variants. The binary's
  `apps/aberp/src/audit_query.rs` is **unchanged** — an
  `InvoiceCheckPerformed` entry is informational only and does
  NOT change `StuckStage` classification per ADR-0033 §6 (the
  state-2 → not-stuck transition is a recover-from-nav surface,
  F48-deferred). The binary's
  `apps/aberp/src/export_invoice_bundle.rs` exhaustive
  `extract_nav_xml` match gains one new arm for the new variant.
  Does **not** supersede ADR-0009, ADR-0032, ADR-0031, ADR-0030,
  or any prior ADR; all remain in force.
- **Related:**
  - **ADR-0009 §5 Layer-2 idempotency** — the parent posture:
    "If the process crashed between `manageInvoice` returning and
    the `transactionId` being persisted (no Layer-1 record yet),
    the retry path **first calls `queryInvoiceCheck`** against the
    invoice number. If NAV already has it, fetch the chain via
    `queryInvoiceData` and reconstruct local state. If NAV does
    not have it, submit fresh." ADR-0033 realises the first
    half of this intent (the `queryInvoiceCheck` call + the
    "submit fresh iff NAV does not have it" branch). The second
    half (chain fetch + local-state reconstruction) is named-
    deferred to F48 — the operator-visible message names the
    gap loud per CLAUDE.md rule 12.
  - **ADR-0009 §5** (operator-unblock surface) — the existing
    `retry-submission` / `mark-abandoned` command shape per the
    pre-PR-20 posture. ADR-0033 amends the state-2 path of
    `retry-submission` to consult Layer-2 first; the state-3
    path remains the PR-19 two-tx shape. `mark-abandoned` is
    unchanged in PR-20.
  - **ADR-0032 §"Adversarial review" #2** — the duplicate-
    submission residual: "State-2 retry might produce a duplicate
    submission to NAV. Why is this acceptable?" ADR-0032 accepted
    the residual loud-warned until Layer-2 lands. ADR-0033 closes
    the gap by interposing the Layer-2 disambiguation step.
  - **ADR-0032 §"Open questions"** — `Layer-2 queryInvoiceCheck
    operation in nav-transport` was named here; F44 trigger fires
    with PR-20.
  - **ADR-0032 §4** — state-2 `StuckStage::Pending` precondition.
    Unchanged by ADR-0033; the precondition walker still classifies
    an Attempt-without-Response as state-2 Pending, and the
    `retry-submission` orchestration is what consults Layer-2
    inside that branch.
  - **ADR-0032 §5** — `submission_queue` fourth-predicate clause
    (exclude Attempted invoices from drain). Unchanged by
    ADR-0033; drain still handles only pure-Draft invoices.
    Drain is the wrong surface for Layer-2 because by predicate
    construction it never sees a state-2 invoice (which is where
    Layer-2 disambiguation is load-bearing).
  - **ADR-0028 §3** — the `query_invoice_data.rs` operations
    module that PR-15 added. ADR-0033's `query_invoice_check.rs`
    follows the structural-parallel posture (same `<user>` block,
    same non-`manageInvoice` request-signature shape, same
    `<invoiceNumberQuery>` body wrapper) but uses
    `<queryInvoiceCheckRequest>` as the root + parses a boolean
    `<invoiceCheckResult>` from the OK response per the NAV v3.0
    spec.
  - **ADR-0009 §8** — audit-evidence bundle. The new
    `InvoiceCheckPerformed` entry's verbatim request + response
    bytes flow into the bundle's `nav/` directory via the
    exhaustive-match arm added to `extract_nav_xml`. The
    response bytes are the load-bearing inspector evidence
    ("NAV said TRUE/FALSE for invoice X at time Y"); the
    request bytes live in-payload via `chain.jsonl` (same
    posture every prior NAV-bearing variant uses).
  - **F12 four-coordinated-edit ritual.** Eleventh landing.
    `InvoiceCheckPerformed` is the new variant; the four
    coordinated edits are the variant body in `EventKind`, the
    `as_str` arm, the `from_storage_str` arm, and the
    `round_trip_for_every_variant` hand-listed test array. Same
    posture as PR-8 / PR-10 / PR-11 / PR-12 / PR-13 / PR-14 /
    PR-15 / PR-19's prior firings.
  - **Session-23 handoff §"Suggested next session sub-split"**
    — named PR-20 = F44 closure as the strongest pick (rationale:
    closes PR-19's state-2 duplicate-submission residual; lands
    the `queryInvoiceCheck` operation in nav-transport as a
    foundation for future reconciliation work; re-asserts the
    F12 four-edit ritual mechanically). Session 23 also named
    Option B (bundle verifier — F38) and Option D (serve.rs
    state-2 visibility — F21 + F47) as alternative PR-20
    candidates; ADR-0033 takes Option A.
- **Source material:** ADR-0009 §5 (the Layer-2 design intent),
  ADR-0032 §"Open questions" + §"Adversarial review" #2 (the
  duplicate-submission residual + F44 named-trigger),
  ADR-0028 §3 (the `query_invoice_data.rs` operations module
  structural template), `docs/research/nav-and-billingo.md`
  §"NAV Online Számla v3.0 operations" line 105 (the
  `queryInvoiceCheck` boolean existence check) +
  §"Network-reset disambiguation" lines 177–181 (the pattern
  observed in angro-kft/nav-connector), `crates/nav-transport/
  src/operations/query_invoice_data.rs` (the structural-
  parallel template PR-20's new module follows),
  `crates/nav-transport/src/operations/manage_invoice.rs`
  (the `build_request` + `send_built_request` split posture
  PR-20 mirrors), `apps/aberp/src/retry_submission.rs` (the
  state-2 path PR-20 amends), `apps/aberp/src/audit_query.rs`
  (the precondition walker PR-20 does NOT amend — Layer-2
  is informational, not classification-bearing),
  `apps/aberp/src/submission_queue.rs::classify_attempt_failure`
  (the classifier PR-20 extends with five new arms).

## Context

PR-19 / ADR-0032 closed F40 at the issuing-path level: every
NAV submission attempt now produces an audit row, success or
failure. The two-tx Attempt-before-call posture means a wire
that breaks between the `manageInvoice` POST and NAV's response
leaves an `InvoiceSubmissionAttempt` row in the audit ledger
with no matching `InvoiceSubmissionResponse` — state-2 Pending
per ADR-0032 §4. The operator-recoverable command for state-2
is the existing `retry-submission`, extended in PR-19 to accept
state-2 (the precondition walker now classifies an Attempt-
without-Response as `Stuck(StuckStage::Pending)`).

The residual that PR-19's adversarial review #2 accepted loud-
warned: a state-2 retry may produce a **duplicate submission to
NAV** if the prior Attempt actually reached NAV before the wire
broke. Layer-1 idempotency (the `IssueInvoiceCommand` ULID) does
NOT protect against this — Layer-1 only protects against
duplicate ABERP-side issuance commands; if the operator runs
`retry-submission` on an invoice whose prior Attempt landed at
NAV but whose response was lost in transit, the retry's fresh
`manageInvoice` POST is a structurally distinct command from
NAV's perspective. NAV's invoice-number-uniqueness guard
(`INVOICE_NUMBER_NOT_UNIQUE`) IS the NAV-side dedup mechanism,
but it surfaces as a non-retryable error AFTER the duplicate
arrives — too late to avoid the duplicate-submission audit
trail on NAV's side.

ADR-0009 §5 names the disambiguation surface: **Layer-2
`queryInvoiceCheck`** — a NAV-side boolean existence check
against the invoice number, performed BEFORE the retry's
re-POST. If NAV already has the invoice, the retry skips the
re-POST (no duplicate). If NAV does not, the retry proceeds
with the standard prepare → TX1 → wire → TX2 sequence.

`docs/research/nav-and-billingo.md` independently confirms the
pattern: "on connect-reset after the request has reached the
server but before the response returned, immediately call
`queryInvoiceCheck` / `queryInvoiceDigest` for the invoice
number to determine whether the prior submission landed.
(Pattern observed in angro-kft/nav-connector.)" The Hungarian
open-source NAV-client community converged on the same shape
ADR-0009 §5 names.

PR-20 closes F44 at the state-2 disambiguation level by adding
the `queryInvoiceCheck` operation to `nav-transport`, the
`InvoiceCheckPerformed` audit-ledger variant, and the
state-2 path amendment in `retry-submission`. The
**post-positive-check NAV-side state recovery** — fetching the
chain via `queryInvoiceData` and reconstructing the missing
local `InvoiceSubmissionResponse` + `InvoiceAckStatus` entries
per ADR-0009 §5's full "fetch the chain via `queryInvoiceData`
and reconstruct local state" intent — is named-deferred to F48
(its own ADR). Today the operator-visible message names the
gap loud: "NAV already has invoice X; ABERP did not write the
prior submission's Response/Ack to the local ledger; the
chain-reconstruction surface is named-deferred per F48 — until
it lands, the operator inspects NAV's web UI and/or runs
`mark-abandoned` locally to terminate the chain by operator
decision."

### Prerequisite-gate state at PR-20 time

- **ADR-0009 §5 Layer-2 idempotency (first half: `queryInvoiceCheck`
  call + state-2 skip-on-exists).** UNMET. PR-20 closes this gap.
- **ADR-0009 §5 Layer-2 idempotency (second half: chain fetch +
  local-state reconstruction).** UNMET. PR-20 names-defers as
  F48; not in scope.
- **ADR-0032 §4** — state-2 Pending precondition walker.
  UNCHANGED by PR-20. The Layer-2 consultation is internal to
  `retry-submission`'s state-2 branch; the precondition walker
  still classifies an Attempt-without-Response as state-2
  Pending regardless of any `InvoiceCheckPerformed` entry.
- **ADR-0032 §5** — `submission_queue` fourth-predicate clause.
  UNCHANGED. Drain still handles only pure-Draft invoices.
- **ADR-0008 §"Storage"** — one-tx-per-state-change posture.
  Preserved per the same rationale as ADR-0032's two-tx
  posture: the `InvoiceCheckPerformed` entry is its own
  state change (ABERP-side decision-to-check + NAV-side
  decision-to-answer) and atomically pairs the check's
  outcome with the verbatim wire bytes in one tx. The
  retry's subsequent TX1 (RetryRequested + Attempt) and TX2
  (Response or AttemptFailed) remain in their own txs per
  ADR-0032 §1.
- **ADR-0028 §3** — `queryInvoiceData` operations module.
  Used as the structural template; not amended. PR-20's
  new `query_invoice_check.rs` parallels its shape but
  parses a boolean instead of returning verbatim bytes only.

### What surfaced during PR-20 design

Three conflicts among prior-PR conventions plus seven
adversarial-review concerns surfaced during the design pass.

**Surfaced conflict 1: What does the retry do if
`queryInvoiceCheck` itself fails?** Three readings:

- **Reading A — Soft fail: abort the retry; do not re-POST.**
  Trade-off: if queryInvoiceCheck fails with a transport error,
  the manageInvoice POST would likely fail too — the operator's
  re-run-later instinct is correct. The retry leaves the
  invoice in state-2 Pending; an `InvoiceCheckPerformed` entry
  with `outcome = "failure"` is written; the operator-visible
  message names the disambiguation gap and asks the operator
  to re-run later. No duplicate-submission risk. Accepted.
- **Reading B — Hard fail: abort the retry; surface only via
  tracing + non-zero exit.** Trade-off: no audit entry written;
  the operator loses evidence of "ABERP tried to disambiguate
  but NAV-side check failed." Rejected per CLAUDE.md rule 12
  (fail loud, leave evidence).
- **Reading C — Proceed: re-POST anyway if queryInvoiceCheck
  fails.** Trade-off: matches ADR-0032 §"Adversarial review"
  #2's "loud-acceptance preferable to silent-refusal" posture
  (the operator chose to retry; absence of disambiguation
  should not block them). Rejected — the duplicate-submission
  residual is exactly what Layer-2 exists to prevent; a
  Layer-2 failure that proceeds defeats the purpose. The
  operator's chosen recovery path (re-run later when NAV is
  reachable) is the safe direction.

**Decision: Reading A.** PR-20's `retry-submission` state-2
path writes the `InvoiceCheckPerformed` entry with
`outcome = "failure"` (and the typed `failure_class` +
`failure_code` + `failure_message` per the same enumeration
`InvoiceSubmissionAttemptFailedPayload` uses), then loud-fails
the command. The invoice remains in state-2 Pending; the
operator re-runs `retry-submission` later. Subsequent retries
walk the same Layer-2 path; multiple `InvoiceCheckPerformed`
entries accumulate as audit evidence per the same posture
`InvoiceSubmissionAttempt` + `InvoiceSubmissionAttemptFailed`
chains accumulate today.

**Surfaced conflict 2: Should the new EventKind variant
discriminate by outcome (one variant per
`exists`/`absent`/`failure`) or carry one variant with a
typed outcome field?** Three readings:

- **Reading A — One variant per outcome
  (`InvoiceCheckExistsConfirmed`,
  `InvoiceCheckAbsenceConfirmed`,
  `InvoiceCheckFailed`).** Trade-off: F12 ritual fires three
  times; bundle reader filters by kind without inspecting
  payload. Rejected — three sub-types of "ABERP performed
  a Layer-2 existence check" are not structurally distinct
  events; they share the same precondition (state-2 retry
  invoked an existence check) and the same downstream
  control flow (the retry decides skip-re-POST vs proceed
  based on the outcome). Same shape as ADR-0032 §"Surfaced
  conflict 2 Reading A" (which rejected per-failure-class
  variants for `InvoiceSubmissionAttemptFailed`).
- **Reading B — One variant with a typed `outcome` string
  field (`"exists"` / `"absent"` / `"failure"`).** Trade-off:
  F12 ritual fires once; bundle reader dispatches on
  payload field. Accepted — matches ADR-0032 §"Surfaced
  conflict 2 Reading B"'s posture for `error_class` on
  `InvoiceSubmissionAttemptFailedPayload`. The
  payload-field dispatch is sufficient for inspector
  triage.
- **Reading C — Reuse an existing EventKind variant.**
  No existing variant fits the semantic (query-side
  reconciliation distinct from `InvoiceSubmissionAttempt` /
  `InvoiceSubmissionResponse`). Rejected.

**Decision: Reading B.** PR-20 adds one new EventKind variant
`InvoiceCheckPerformed` with a typed `outcome` field on the
payload. The values are enumerated below in §2 as a string
field (same posture as `InvoiceSubmissionAttemptFailedPayload.error_class`).

**Surfaced conflict 3: Does `drain-submission-queue`
consult `queryInvoiceCheck` for state-2 invoices, or stay
strict?** Three readings:

- **Reading A — Relax the fourth-predicate clause: drain
  consults queryInvoiceCheck for state-2 invoices and
  re-POSTs only if NAV does not have them.** Trade-off:
  fully automatic recovery from transport-mid-flight loss;
  matches the F45-named automatic-retry-loop intent.
  Rejected — drain is intentionally automatic per
  ADR-0031 §3 + ADR-0032 §5; state-2 retry requires
  operator acknowledgement of the duplicate-submission
  residual (now narrowed by Layer-2, but still present
  in the `outcome = "failure"` case). Mixing automatic
  drain with Layer-2 conflates F44 (this ADR) with F45
  (automatic state-2 retry loop); F45 has its own
  named-trigger and deserves its own decision.
- **Reading B — Keep the fourth-predicate clause strict;
  drain handles only pure-Draft invoices.** Trade-off:
  state-2 recovery stays operator-driven; F45's automatic
  loop landing in a future PR amends the drain (or adds
  a new daemon) without backfilling the operator-decision
  layer. Accepted — matches ADR-0032 §5's posture verbatim;
  surgical scope per CLAUDE.md rule 3.
- **Reading C — Add a new operator-driven drain variant
  (`drain-state-2`) distinct from the existing drain.**
  Rejected — doubles the operator surface area for a
  difference (Layer-2 consultation) that the existing
  `retry-submission` command already handles per-invoice.
  Drain's value is FIFO automation across many pure-Draft
  invoices; the operator-driven Layer-2 path is the wrong
  shape to bolt on.

**Decision: Reading B.** PR-20 does NOT amend
`drain-submission-queue`. The drain's fourth-predicate
clause (ADR-0032 §5) stays as-is. State-2 recovery remains
operator-driven via `retry-submission`. F45 (automatic
state-2 retry loop) keeps its existing named-trigger and
will decide drain's posture when it fires.

## Decision

### 1. Three-phase posture for state-2 `retry-submission`

The state-2 branch of `retry-submission` (PR-19's shape) gains
a Layer-2 disambiguation step before the existing two-tx
posture. The new shape:

- **Phase 0 — Layer-2 disambiguation.** Open transport,
  tokenExchange-skipping (queryInvoiceCheck is a NAV query
  operation, no exchange token needed — same as
  queryInvoiceData per ADR-0028 §3 + ADR-0009 §4). Build the
  `<QueryInvoiceCheckRequest>` envelope via
  `soap::render_query_invoice_check_request`. POST to
  `<endpoint>/queryInvoiceCheck`. Parse the
  `<invoiceCheckResult>` boolean from the OK response. Three
  outcomes:
  - **`Exists` (NAV has the invoice).** Skip Phase 1 + Phase
    2. Write the `InvoiceCheckPerformed` audit entry with
    `outcome = "exists"`. Sync mirror. The operator-visible
    summary names the chain-reconstruction gap loud per F48;
    the typestate is NOT transitioned (Stuck stays Stuck;
    the operator's next move is either `mark-abandoned` or
    waiting for F48 / a recover-from-nav surface).
  - **`Absent` (NAV does not have the invoice).** Write the
    `InvoiceCheckPerformed` audit entry with
    `outcome = "absent"`. Sync mirror. Proceed to Phase 1
    + Phase 2 per the pre-PR-20 / ADR-0032 §1 shape.
  - **`Failure` (queryInvoiceCheck failed at any layer:
    transport / HTTP status / response parse / NAV-side
    application error).** Write the `InvoiceCheckPerformed`
    audit entry with `outcome = "failure"` (carrying the
    typed `failure_class` / `failure_code` / `failure_message`
    per the same enumeration `InvoiceSubmissionAttemptFailedPayload`
    uses). Sync mirror. Loud-fail the command — do NOT
    proceed to Phase 1. Operator re-runs later.
- **Phase 1 — TX1 (RetryRequested + Attempt-before-call).**
  Unchanged from PR-19. Fires iff Phase 0 returned `Absent`.
  Opens transport, tokenExchange, builds the
  `<ManageInvoiceRequest>` envelope, writes
  `InvoiceRetryRequested` + `InvoiceSubmissionAttempt` in
  one tx, commits, syncs mirror.
- **Phase 2 — Wire send + TX2.** Unchanged from PR-19. POST
  the prepared envelope; on success write
  `InvoiceSubmissionResponse`; on failure write
  `InvoiceSubmissionAttemptFailed`. Commit. Sync mirror.

The state-3 (`StuckStage::AwaitingAck`) branch retains the
PR-19 two-tx posture verbatim. State-3 has a prior
`InvoiceSubmissionResponse` (so NAV's Layer-1
`INVOICE_NUMBER_NOT_UNIQUE` guard would catch a duplicate
re-POST at NAV's side); ADR-0033 does NOT extend Layer-2 to
state-3 because the duplicate residual is already covered by
NAV's own dedup. A future F (named-deferred) may add Layer-2
to state-3 as belt-and-braces; PR-20's surgical scope keeps
state-3 untouched.

`submit-invoice` and `drain-submission-queue` are unchanged.
Neither operates on state-2 invoices: `submit-invoice` is the
fresh-issuance path (never a retry), and drain's predicate
excludes Attempted invoices (which by construction is the
state-2 universe).

### 2. New `EventKind` variant + payload

A new `EventKind` variant `EventKind::InvoiceCheckPerformed`
is added to `crates/audit-ledger/src/entry/event_kind.rs`. The
on-disk string is `invoice.check_performed`. The F12 four-
coordinated-edit ritual fires for the eleventh time across
PR-6.1 / PR-7-B-3 / PR-8 / PR-10 / PR-11 / PR-12 / PR-13 /
PR-14 / PR-15 / PR-19 / PR-20 (variant body + `as_str` arm +
`from_storage_str` arm + `round_trip_for_every_variant`
hand-listed array).

The matching payload type
`audit_payloads::InvoiceCheckPerformedPayload` lives in
`apps/aberp/src/audit_payloads.rs` with this shape:

```rust
pub struct InvoiceCheckPerformedPayload {
    /// Prefixed `inv_<ULID>` form — same shape as every other
    /// invoice-bearing payload.
    pub invoice_id: String,
    /// F8 idempotency key carry-forward — same canonical form
    /// as every other NAV-related entry for this invoice.
    pub idempotency_key: String,
    /// `"test"` or `"production"` — same shape as
    /// `InvoiceSubmissionAttemptPayload.endpoint` and
    /// `InvoiceSubmissionAttemptFailedPayload.endpoint`.
    pub endpoint: String,
    /// The NAV-facing invoice number string that was queried
    /// (e.g., `"INV-default/00042"`). The bundle reader sees
    /// the exact identifier that hit NAV's queryInvoiceCheck
    /// endpoint without re-deriving from series.code + seq.
    pub nav_invoice_number: String,
    /// Outcome discriminator. Enumerated values:
    ///   `"exists"`  — NAV returned `<invoiceCheckResult>true</>`.
    ///                 Retry SKIPPED the manageInvoice re-POST.
    ///   `"absent"`  — NAV returned `<invoiceCheckResult>false</>`.
    ///                 Retry PROCEEDED to the manageInvoice
    ///                 re-POST per ADR-0032 §1's two-tx posture.
    ///   `"failure"` — queryInvoiceCheck failed at any layer
    ///                 (transport / http_status / response_parse
    ///                 / application). Retry ABORTED (CLAUDE.md
    ///                 rule 12 — Layer-2 disambiguation gap is
    ///                 named loud; operator re-runs later).
    pub outcome: String,
    /// Verbatim `<QueryInvoiceCheckRequest>` envelope bytes
    /// for the audit-evidence bundle (ADR-0009 §8). Persisted
    /// for every outcome — even on `"failure"` the request
    /// bytes show what ABERP attempted.
    pub request_xml: Vec<u8>,
    /// Verbatim NAV response bytes. `Some(...)` for `"exists"`
    /// and `"absent"` outcomes (NAV returned a body even if it
    /// was an error body); `Some(...)` for `"failure"` outcomes
    /// where a body was received before the failure fired
    /// (e.g., http_status / application classes — NAV's body
    /// carries the `<funcCode>` / `<errorCode>` / `<message>`
    /// triple); `None` for `"failure"` outcomes where no body
    /// was received (transport / envelope / credential /
    /// client_build classes).
    pub response_xml: Option<Vec<u8>>,
    /// `Some(...)` IFF `outcome == "failure"`. One of the same
    /// seven classes the `InvoiceSubmissionAttemptFailedPayload.error_class`
    /// field enumerates (`"transport"` / `"http_status"` /
    /// `"application"` / `"retryable_application"` /
    /// `"envelope"` / `"credential"` / `"client_build"`). `None`
    /// for `"exists"` and `"absent"` outcomes.
    pub failure_class: Option<String>,
    /// `Some(...)` for `failure_class == "application"`
    /// (NAV code) or `"retryable_application"` (NAV code) or
    /// `"http_status"` (HTTP status as decimal string); `None`
    /// otherwise.
    pub failure_code: Option<String>,
    /// `Some(...)` IFF `outcome == "failure"`. The operator-
    /// visible error message — the `NavTransportError::Display`
    /// rendering of the failure. Never includes secret material
    /// per ADR-0020 §3.
    pub failure_message: Option<String>,
}
```

The `outcome` enumeration is a string field (not a sub-enum
in the schema) for the same reason ADR-0032's `error_class`
is a string: the audit-ledger's payload schema is JSON without
enum constraints; Rust-side discipline lives in the constructor
helpers.

### 3. nav-transport `queryInvoiceCheck` operation

A new module `crates/nav-transport/src/operations/query_invoice_check.rs`
follows the ADR-0032 §3 split shape: two public functions
`build_request` + `send_built_request` plus a typed outcome
enum `QueryInvoiceCheckOutcome`. **No backward-compat `call`
wrapper** because this is a brand-new operation with no
pre-existing callers (unlike ADR-0032's `manage_invoice::call`
which had to keep its existing signature for the test-fixture
callers). The callers that need Layer-2 (PR-20's
`retry-submission` state-2 branch) compose `build_request` +
`send_built_request` directly.

Function signatures:

- `query_invoice_check::build_request(credentials, tax_number_8,
  nav_invoice_number, invoice_direction) -> Result<Vec<u8>, NavTransportError>`
  — renders the `<QueryInvoiceCheckRequest>` envelope bytes
  via a new `soap::render_query_invoice_check_request` helper.
  Same non-`manageInvoice` request-signature shape as
  `queryInvoiceData` / `queryTransactionStatus`. No
  `exchange_token` parameter (query operations authenticate via
  the per-request `<user>` block alone per ADR-0009 §4).
- `query_invoice_check::send_built_request(transport,
  request_xml) -> Result<SendBuiltRequestOutcome, NavTransportError>`
  — POSTs the pre-rendered envelope to
  `<endpoint>/queryInvoiceCheck`, captures the response
  verbatim, parses, classifies errors. Returns
  `SendBuiltRequestOutcome { check_result: bool, response_xml: Vec<u8> }`.
- `QueryInvoiceCheckOutcome` is the high-level enum the binary
  consumes: `Exists` / `Absent`. The orchestration maps from
  `SendBuiltRequestOutcome.check_result` to the enum at the
  call site.

The wire body of `<QueryInvoiceCheckRequest>` follows
`queryInvoiceData`'s `<invoiceNumberQuery>` wrapper shape per
the structural-parallel template (ADR-0028 §3):

```xml
<QueryInvoiceCheckRequest>
  <!-- common:header, common:user, software (shared via render_request) -->
  <invoiceNumberQuery>
    <invoiceNumber>INV-default/00042</invoiceNumber>
    <invoiceDirection>OUTBOUND</invoiceDirection>
    <batchIndex>1</batchIndex>
  </invoiceNumberQuery>
</QueryInvoiceCheckRequest>
```

The OK response carries `<invoiceCheckResult>true|false</invoiceCheckResult>`
which the parser extracts via the existing
`find_first_text(response_xml, "invoiceCheckResult")` helper.
The string value is matched: `"true"` → `Ok(true)`,
`"false"` → `Ok(false)`, anything else → loud-fail with
`QueryInvoiceCheckResponseParse`.

**Open question — NAV-testbed verification.** The exact
on-the-wire element name (`<invoiceCheckResult>` per
`docs/research/nav-and-billingo.md` + observed open-source
clients) and boolean encoding (`true`/`false` as text) are
pinned by the structural-parallel posture. NAV-testbed
verification is the named trigger for amendment if NAV's
actual response shape differs from the modelled one. Same
verbatim-bytes-first posture every prior NAV operation
uses: the response bytes flow into the audit ledger before
parse, so a parse-side bug cannot drop the evidence.

### 4. Five new `NavTransportError` variants

The `NavTransportError` enum grows by five variants for
queryInvoiceCheck — same shape as the four-variant cluster
queryInvoiceData / queryTransactionStatus / manageAnnulment
each contribute:

```rust
/// HTTP-layer failure on queryInvoiceCheck. PR-20 / ADR-0033 §3.
#[error("queryInvoiceCheck HTTP call failed: {0}")]
QueryInvoiceCheckHttp(#[source] reqwest::Error),

/// NAV returned a non-success HTTP status to queryInvoiceCheck.
#[error("queryInvoiceCheck returned non-success HTTP status: {status}")]
QueryInvoiceCheckHttpStatus { status: u16 },

/// The queryInvoiceCheck response body could not be parsed.
#[error("queryInvoiceCheck response parse failed: {0}")]
QueryInvoiceCheckResponseParse(String),

/// NAV non-retryable application error against queryInvoiceCheck.
#[error("queryInvoiceCheck non-retryable error: {code} — {message}")]
QueryInvoiceCheckNonRetryable { code: String, message: String },

/// NAV retryable application error against queryInvoiceCheck.
#[error("queryInvoiceCheck retryable error: {code} — {message}")]
QueryInvoiceCheckRetryable { code: String, message: String },
```

### 5. `submission_queue::classify_attempt_failure` extension

The classifier extends with five new arms mapping the new
`NavTransportError::QueryInvoiceCheck*` variants to the same
seven-class enumeration used by
`InvoiceSubmissionAttemptFailedPayload.error_class` and now also
`InvoiceCheckPerformedPayload.failure_class`:

```rust
NavTransportError::QueryInvoiceCheckHttp(_) => ("transport", None),
NavTransportError::QueryInvoiceCheckHttpStatus { status } => {
    ("http_status", Some(status.to_string()))
}
NavTransportError::QueryInvoiceCheckResponseParse(_) => ("application", None),
NavTransportError::QueryInvoiceCheckNonRetryable { code, .. } => {
    ("application", Some(code.clone()))
}
NavTransportError::QueryInvoiceCheckRetryable { code, .. } => {
    ("retryable_application", Some(code.clone()))
}
```

These arms slot into the existing classifier alongside the
analogous queryInvoiceData / queryTransactionStatus /
manageAnnulment arms — same five-line cluster per operation.
The classifier remains total per CLAUDE.md rule 5 (every
`NavTransportError` variant has an explicit arm; no `_`
default fallback).

### 6. `audit_query.rs` precondition walker UNCHANGED

ADR-0033 explicitly does NOT amend
`audit_query.rs::stuck_precondition`. The presence of an
`InvoiceCheckPerformed` entry — regardless of `outcome` —
does NOT change the precondition walker's classification:

- An invoice with `Attempt` + `InvoiceCheckPerformed(outcome=exists)`
  but no `Response` is still **state-2 Pending**. The
  precondition walker does not consult NAV-side facts; it
  walks ABERP-side ledger entries only. Per ADR-0033 §1, the
  retry-submission orchestration is what skips re-POST on
  `Exists`; the precondition walker classifies, the
  orchestration decides.
- An invoice with `Attempt` + `InvoiceCheckPerformed(outcome=absent)`
  + `Response` is **state-3 AwaitingAck** (via step 2 of the
  classifier — Response wins).
- An invoice with `Attempt` + `InvoiceCheckPerformed(outcome=failure)`
  + no Response is still **state-2 Pending**.

The classifier's match-arm ordering and stage values are
unchanged. The state-2 → not-stuck transition (when NAV has
the invoice but ABERP did not record the Response/Ack) is
the F48-deferred recover-from-nav surface; until F48 lands,
operator-visible state-2 with prior `Exists` checks accumulate
in the audit chain as evidence that ABERP knows NAV has the
invoice and the operator chose to skip re-POST.

This is the deliberate minimal scope. A future F48 closure
PR may extend the precondition walker to consult the latest
`InvoiceCheckPerformed` entry and classify state-2 +
`Exists` as `NotStuck(StateRecoveryPending)` or similar.
PR-20 does not pre-empt that decision.

### 7. `drain-submission-queue` UNCHANGED

Per §"Surfaced conflict 3 Reading B", drain does not
consult queryInvoiceCheck. The fourth-predicate clause
(ADR-0032 §5) excludes Attempted invoices from drain;
state-2 recovery stays operator-driven via
`retry-submission`. F45 (automatic state-2 retry loop)
retains its existing named-trigger.

### 8. `mark-abandoned` UNCHANGED for PR-20

Per ADR-0032 §"Adversarial review" #8, `mark-abandoned`
already accepts state-2 invoices. PR-20 does not amend
its behaviour: the operator's decision to mark an invoice
abandoned is independent of NAV-side existence. If NAV
has the invoice but the operator marks it abandoned
locally, the audit ledger shows the divergence loud (an
`InvoiceCheckPerformed(outcome=exists)` followed by
`InvoiceMarkedAbandoned` is operator-visible evidence
that the operator chose to terminate the chain by
decision despite NAV's record).

A future F (named-deferred — F49) may add Layer-2-aware
mark-abandoned that consults queryInvoiceCheck before
accepting the decision, warning the operator if NAV has
the invoice. PR-20 does not pre-empt; the surgical scope
stays bounded.

### 9. Deferred scope

Two sub-surfaces are NAMED here and DEFERRED per
CLAUDE.md rule 2 + ADR-0021's just-in-time-ADR posture.
Each has a named trigger.

- **F48 — Post-positive-check NAV-side state recovery
  (recover-from-nav).** ADR-0009 §5 names the full Layer-2
  intent: "If NAV already has it, fetch the chain via
  `queryInvoiceData` and reconstruct local state." PR-20
  closes the first half (the existence check); the chain
  fetch + local-state reconstruction (writing the missing
  `InvoiceSubmissionResponse` + `InvoiceAckStatus` entries
  derived from NAV's `queryInvoiceData` response) is named-
  deferred. Today the operator-visible message after an
  `outcome = "exists"` retry names the gap loud and asks
  the operator to inspect NAV's web UI and/or run
  `mark-abandoned` locally. Named trigger: first PR that
  introduces a `recover-from-nav` operator command OR
  first PR that wires the NAV historical/reconciliation
  read path per the ADR-0010 §Deferred named-deferred
  surface.

- **F49 — Layer-2-aware `mark-abandoned`.** Today
  `mark-abandoned` does not consult NAV. A future operational
  pattern may want the command to warn the operator before
  accepting a state-2 abandonment when NAV has the invoice
  (i.e., consult `queryInvoiceCheck` first and prompt the
  operator to confirm the divergence). Named trigger: first
  operator request OR first incident where a state-2 +
  `Exists` invoice was abandoned without operator awareness
  that NAV had a copy.

## Consequences

**Positive**

- The state-2 retry's duplicate-submission residual that
  PR-19's adversarial review #2 named-warned is **closed**
  for the `Absent` and `Failure` outcomes (no re-POST happens
  in either case until the operator re-runs after NAV is
  reachable). The `Exists` outcome explicitly skips the
  re-POST, so the duplicate-submission risk is structurally
  eliminated at the orchestration level.
- The audit-evidence bundle (ADR-0009 §8) now carries
  evidence for every Layer-2 check, success or failure. A
  NAV inspector reading the bundle sees "ABERP knew NAV had
  invoice X at time T; ABERP did not re-POST" as a coherent
  trace alongside the `Attempt` + (absent) `Response` pair.
- The `queryInvoiceCheck` operation lands in `nav-transport`
  as a one-time addition that future Layer-2 work (F45
  automatic retry, F48 chain-reconstruction, F49 abandon-
  awareness) can reuse without re-deriving the wire shape.
- The `build_request` + `send_built_request` split is the
  third operation to use the ADR-0032 §3 posture (after
  `manage_invoice`'s split and the implicit single-call
  shape of every other query operation). The pattern is now
  established and contributors landing future Attempt-before-
  call operations have two reference points.
- The F12 four-edit ritual fires for the eleventh time. The
  ritual continues to perform its job — the trap is caught
  at test time, not at runtime.

**Negative**

- One extra HTTP round-trip per state-2 retry. For
  transport-mid-flight loss patterns (the rare-but-real
  scenario PR-19 named), the operator's command latency
  doubles (queryInvoiceCheck + manageInvoice instead of
  manageInvoice alone). Per the project owner framing, NAV
  latency is the dominant per-invoice cost regardless; one
  extra round-trip is sub-second on the happy path.
- One extra audit-ledger transaction per state-2 retry
  (the `InvoiceCheckPerformed` write). The TX0 commit
  + mirror sync adds one sub-millisecond cost layer atop
  ADR-0032's two-tx posture. State-2 retries now have THREE
  sync_mirror calls per run (Phase 0 + Phase 1 + Phase 2);
  state-2 retries that abort on Phase 0 (`outcome = "failure"`)
  have ONE sync_mirror call; state-2 retries that skip
  Phase 1+2 on `Exists` have ONE sync_mirror call.
- The chain-reconstruction surface (F48) remains deferred.
  An operator who sees `outcome = "exists"` cannot recover
  the local Response/Ack chain via ABERP commands today;
  the audit ledger shows the divergence loud but the
  recovery path is manual. CLAUDE.md rule 12 — the gap is
  named, not hidden.
- The `NavTransportError` enum grows by five variants. The
  error surface is now O(operations × failure-classes); a
  future refactor that consolidates the per-operation Http
  / HttpStatus / NonRetryable / Retryable / ResponseParse
  pattern into a single shape would touch every existing
  arm. Not in scope for PR-20.
- The `audit_query::stuck_precondition` is intentionally
  unchanged. An invoice with `Attempt` +
  `InvoiceCheckPerformed(outcome=exists)` classifies as
  state-2 Pending, the same as before any Layer-2 check.
  This is technically over-conservative — NAV definitively
  has the invoice — but the alternative (introduce a new
  `StateRecoveryPending` stage) pre-empts F48's design
  decisions. Surgical scope per CLAUDE.md rule 3.

**Locked in**

- Once an `InvoiceCheckPerformed` entry is written to the
  audit ledger, it is immutable per ADR-0008 (the hash
  chain locks it in). A future ADR that changes the
  `outcome` enumeration (e.g., splits `"failure"` into
  per-class outcome strings) must add the new values as
  additional valid strings; existing entries remain valid
  for historical reading.
- The three-phase posture for state-2 commits to a per-
  retry ordering of "Check first, then re-POST iff Absent."
  A future refactor that wants to batch multiple
  state-2 retries (e.g., a hypothetical `bulk-retry`
  command) would need to keep the per-invoice three-phase
  posture (one Layer-2 check per invoice) or file a new
  ADR explicitly amending ADR-0033 §1.
- The `queryInvoiceCheck` operation's wire shape is pinned
  by `soap::render_query_invoice_check_request` against
  the structural-parallel posture (queryInvoiceData's
  `<invoiceNumberQuery>` wrapper). A NAV-testbed
  verification that surfaces a different actual shape is
  the named trigger for an amendment ADR; until then the
  modelled shape is the contract.

## Adversarial review

A hostile NAV inspector and a hostile-engineer review, in
alternation.

1. **"You added a NAV round-trip to every state-2 retry.
   What if NAV is itself slow on queryInvoiceCheck?"**
   ADR-0009 §"Adversarial review" #3 already accepted this
   posture for Layer-2: "The check is short and synchronous.
   If it times out we treat the prior submission as suspect
   and the invoice goes to `SubmissionStuck` pending
   operator action — no automatic retry, no automatic
   resubmit. Operator decides." PR-20 implements exactly
   this: a queryInvoiceCheck timeout (or any
   transport-class failure) classifies as
   `outcome = "failure"`, aborts the retry, and leaves the
   invoice in state-2 Pending. The operator decides whether
   to re-run later. No silent re-POST.

2. **"What if queryInvoiceCheck returns a NAV-side
   application error like `INVALID_SECURITY_USER`?"** Same
   path as #1: classified as `outcome = "failure"` with
   `failure_class = "application"` + the NAV code in
   `failure_code`. The retry aborts; the operator triages
   the credential / signature failure exactly as they would
   for an analogous `manageInvoice` failure. The
   `InvoiceCheckPerformed` audit entry records the failure
   for inspector triage.

3. **"What if queryInvoiceCheck returns `true` but the
   prior submission's transactionId is unrecoverable?"**
   This is the F48-named gap. PR-20's
   `outcome = "exists"` retry writes the audit entry
   recording "NAV has invoice X" but does NOT
   reconstruct the local Response/Ack chain. The
   operator-visible summary names the gap loud and points
   the operator at `mark-abandoned` (or waiting for F48).
   The audit ledger's divergence-loudness is preserved;
   the gap is explicit, not silent.

4. **"What stops the operator from running retry-submission
   in a tight loop on a state-2 + Exists invoice?"**
   Nothing structural — but each subsequent retry walks the
   same Phase 0, writes a fresh `InvoiceCheckPerformed`
   entry with `outcome = "exists"`, and exits without
   re-POSTing. No duplicate-submission risk; each retry
   adds one HTTP call to NAV's queryInvoiceCheck endpoint
   and one audit entry to the ledger. The audit ledger's
   growth is bounded by operator behaviour, not by
   structural risk. A future operational pattern that wants
   to refuse repeat checks could add a "don't re-check if
   the last InvoiceCheckPerformed within N seconds returned
   Exists" guard; out of PR-20 scope.

5. **"What about state-3 retries? They have a prior
   transactionId — couldn't NAV reject the re-POST with
   INVOICE_NUMBER_NOT_UNIQUE and lose the operator's
   evidence of having tried?"** State-3 retries still
   write the `InvoiceSubmissionAttempt` audit entry before
   the wire send per ADR-0032 §1; if NAV rejects with
   `INVOICE_NUMBER_NOT_UNIQUE`, the TX2 writes
   `InvoiceSubmissionAttemptFailed` with
   `error_class = "application"` +
   `error_code = "INVOICE_NUMBER_NOT_UNIQUE"`. The
   operator's evidence chain is intact. ADR-0033 does NOT
   extend Layer-2 to state-3 because NAV's Layer-1 dedup
   is already the disambiguation surface for state-3
   (NAV says "I already have this"); a future F may add
   belt-and-braces Layer-2 to state-3 if the audit
   pattern surfaces a reason.

6. **"What if the operator runs `mark-abandoned` against a
   state-2 + Exists invoice — isn't that a divergence with
   NAV?"** Acknowledged. PR-20 does NOT block this — see
   §8 above. The audit ledger records the
   `InvoiceCheckPerformed(outcome=exists)` entry followed
   by the `InvoiceMarkedAbandoned` entry, which is the
   operator-visible divergence trace. A NAV inspector
   reading the chain sees "ABERP knew NAV had the invoice
   but the operator chose to abandon locally." The
   operator's accountability for the decision is preserved
   in the audit ledger. F49 names a future Layer-2-aware
   `mark-abandoned` that would warn the operator before
   accepting; PR-20's surgical scope keeps mark-abandoned
   unchanged.

7. **"You're parsing a boolean from XML text by string
   comparison. What if NAV's response uses `1`/`0` or
   `TRUE`/`FALSE` or some other variant?"** The parser is
   strict per CLAUDE.md rule 12: `"true"` → `Ok(true)`,
   `"false"` → `Ok(false)`, anything else →
   `QueryInvoiceCheckResponseParse` loud-fail. Silent
   coercion would mask schema drift. NAV-testbed
   verification is the named trigger for amendment if the
   actual encoding differs from the modelled one — the
   amendment is mechanical (add new accepted values to the
   parser's match arm).

## Alternatives considered

- **Do not consult Layer-2; accept the duplicate-submission
  residual indefinitely.** Rejected — explicitly named in
  ADR-0009 §5 + ADR-0032 §"Open questions" as the gap
  Layer-2 closes. The residual stands until Layer-2 lands;
  PR-20 lands it.

- **Implement Layer-2 + the chain-reconstruction surface
  (F48) in one PR.** Rejected per CLAUDE.md rule 3
  (surgical changes) + rule 2 (no speculative
  abstractions). The chain-reconstruction surface needs
  its own design pass (queryInvoiceData + audit-entry
  reconstruction + handling NAV's response shape variants
  for retrieved invoices); bolting it onto PR-20 would
  produce a PR-of-PRs. F48 is the named-deferred surface
  with a clear trigger.

- **Add a CLI flag `--skip-layer-2` to retry-submission
  for operators who want to force re-POST despite a
  failed Layer-2 check.** Rejected — speculative
  abstraction per CLAUDE.md rule 2. The
  `outcome = "failure"` path already gives operators the
  re-run-later option; a force-flag would re-introduce
  the duplicate-submission risk Layer-2 exists to
  prevent. If a real operational pattern surfaces, the
  flag can be added then.

- **Drain consults queryInvoiceCheck for state-2 invoices
  (relax the fourth-predicate clause).** Rejected per
  §"Surfaced conflict 3 Reading B" above. Drain is
  automatic; state-2 is operator-driven; conflating the
  two pre-empts F45's design decisions.

- **One new EventKind variant per outcome
  (`InvoiceCheckExistsConfirmed`, etc.).** Rejected per
  §"Surfaced conflict 2 Reading A" above. F12 ritual
  fires three times for marginal classification benefit.

- **Reuse `InvoiceSubmissionResponse` with a typed
  `kind` field to record queryInvoiceCheck outcomes.**
  Rejected — breaks the existing payload schema; bundle
  readers that filter by `EventKind::InvoiceSubmissionResponse`
  would silently misclassify a query as a submission.
  Same posture as ADR-0032 §"Surfaced conflict 2 Reading
  C" rejected.

- **Extend Layer-2 to state-3 retries too (belt-and-
  braces against transport-mid-flight loss on retry
  Response).** Rejected — surgical scope. NAV's Layer-1
  `INVOICE_NUMBER_NOT_UNIQUE` is the existing state-3
  dedup; extending Layer-2 to state-3 doubles the per-
  retry HTTP cost for a vanishingly-small residual. A
  future F-name and named-trigger may revisit this if
  operational evidence surfaces.

## Open questions

The full list of cross-cutting open questions is consolidated
in `docs/research/nav-and-billingo.md`; the items below
specifically block work that ADR-0033 touches:

- **NAV-testbed verification of `queryInvoiceCheck`'s
  request/response shape.** The modelled `<invoiceNumberQuery>`
  body wrapper + `<invoiceCheckResult>` boolean response is
  drawn from `docs/research/nav-and-billingo.md` + the
  structural-parallel posture; NAV-testbed verification is
  the named trigger for amendment if the actual shape
  differs.

- **Chain-reconstruction surface (F48).** The full Layer-2
  intent per ADR-0009 §5 includes fetching the chain via
  `queryInvoiceData` after a positive existence check and
  reconstructing the local Response/Ack entries. Today
  the operator absorbs the divergence; F48 is the
  named-deferred surface.

- **`queryInvoiceCheck` rate limits at NAV.** Whether NAV
  imposes per-operator rate limits on the existence check
  endpoint is unknown. A retry-loop operator pattern could
  trip such a limit. F50 (named-deferred): operator-tunable
  retry-cooldown that throttles repeat queryInvoiceCheck
  calls per invoice. Named trigger: first operator
  incident where rate-limit responses are observed.

- **Layer-2-aware `mark-abandoned` (F49).** Whether the
  command should warn the operator before accepting a
  state-2 abandonment when NAV has the invoice. Today
  it does not; the audit ledger records the divergence
  loud; future operational pattern may want the warning.

## Follow-on ADRs unblocked by this decision

- **ADR — Chain-reconstruction surface (`recover-from-nav`
  operator command per F48).** First PR that introduces
  the NAV-side state recovery path after a positive
  `queryInvoiceCheck`. Amends `audit_query::stuck_precondition`
  to consult the latest `InvoiceCheckPerformed` entry
  (state-2 + Exists → new `NotStuck` reason or new
  Stuck stage); adds `queryInvoiceData` orchestration
  to fetch the chain; writes the missing
  `InvoiceSubmissionResponse` + `InvoiceAckStatus`
  entries.

- **ADR — Automatic state-2 retry loop (F45 closure).**
  Inherits PR-20's Layer-2 disambiguation. The
  automatic loop's per-invoice driver consults
  queryInvoiceCheck before re-POSTing exactly as
  `retry-submission` does post-PR-20.

- **ADR — Operator-tunable threshold config (F42 + F46
  + F50 joint closure).** First PR introducing the
  operator config file surface; lifts F42 (drain alert
  thresholds), F46 (attempt-failed alert thresholds),
  and now F50 (queryInvoiceCheck retry-cooldown)
  together.

- **ADR — Layer-2-aware `mark-abandoned` (F49 closure).**
  First operator request or incident where state-2 +
  Exists abandonment surfaces as a problem.
