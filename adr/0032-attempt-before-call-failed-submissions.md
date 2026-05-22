# ADR-0032 — Attempt-before-call posture and failed-submission audit trail — `InvoiceSubmissionAttempt` is written to the audit ledger BEFORE the `manageInvoice` POST goes on the wire (in its own transaction); a new `InvoiceSubmissionAttemptFailed` `EventKind` records the failure half of the Attempt/Response pair (transport-layer, application-layer, envelope-write, and credential-error classes all surface as one variant with a typed error_class discriminator); `retry-submission`'s precondition walker grows a new `Pending` stage that classifies an invoice with an Attempt but no Response as stuck-recoverable; the offline-submission-queue worker excludes Attempted invoices from its FIFO walk (drain only handles pure-Draft invoices; Attempted invoices flow through `retry-submission`); and the `nav-transport` crate splits `manage_invoice::call` into `build_request` (envelope rendering, no wire) + `send_built_request` (POST + parse) helpers, with the existing `call` retained as a thin wrapper for backward compatibility; closes F40 at the issuing-path level; the Layer-2 `queryInvoiceCheck` idempotency surface remains deferred to its own ADR

- **Status:** Accepted
- **Date:** 2026-05-22
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — first PR after
  ADR-0031 / PR-18 to amend the binary's submit-path
  transaction shape. Closes F40 at the issuing-path level;
  Layer-2 `queryInvoiceCheck` (named in ADR-0009 §5 as the
  belt-and-braces NAV-side reconciliation surface) remains
  deferred to its own ADR. Audit-ledger crate gains one new
  `EventKind` variant (`InvoiceSubmissionAttemptFailed`)
  and one new on-disk string (`invoice.submission_attempt_failed`);
  the F12 four-coordinated-edit ritual fires once. The
  binary's `apps/aberp/src/audit_payloads.rs` gains one new
  payload type (`InvoiceSubmissionAttemptFailedPayload`).
  The binary's `apps/aberp/src/submit_invoice.rs`,
  `apps/aberp/src/retry_submission.rs`, and
  `apps/aberp/src/drain_submission_queue.rs` shift to a
  two-tx posture (TX1 writes Attempt before the wire; TX2
  writes Response on success or AttemptFailed on failure).
  The binary's `apps/aberp/src/audit_query.rs` grows a new
  `StuckStage::Pending` variant and the precondition walker
  classifies state-2 (Attempt-without-Response) as stuck-
  recoverable. The binary's `apps/aberp/src/submission_queue.rs`
  excludes Attempted invoices from the drain's FIFO walk
  (third predicate clause). The `crates/nav-transport`
  crate gains two new public functions in
  `operations::manage_invoice`: `build_request` (envelope
  rendering only, no wire) and `send_built_request` (POST
  + response parse against a pre-built envelope). The
  existing `manage_invoice::call` is retained verbatim as
  a backward-compatibility wrapper around the two new
  helpers. Load-bearing deltas: §1 (transaction shape —
  Attempt-before-call in its own tx, Response/AttemptFailed
  in a second tx), §2 (new EventKind +
  `InvoiceSubmissionAttemptFailedPayload` shape), §3
  (nav-transport split: `build_request` + `send_built_request`
  + retained `call` wrapper), §4 (retry-submission state-2
  acceptance + StuckStage::Pending), §5 (submission_queue
  third-predicate clause — exclude Attempted invoices from
  drain), §6 (drain + submit-invoice + retry-submission tx-
  shape rewrite), §7 (deferred scope — Layer-2
  queryInvoiceCheck reconciliation, automatic state-2 retry
  loop, operator-tunable attempt-failed alert thresholds).
  Does **not** supersede ADR-0009, ADR-0031, ADR-0030, or
  any prior ADR; all remain in force.
- **Related:**
  - **ADR-0009 §8** — the parent posture: "`invoice.submission_attempt`
    — Fires before the response is received so a crash between
    POST and response still leaves the audit trail intact." The
    existing PR-7-B-3 / PR-18 shape wrote Attempt + Response in
    one post-NAV-success transaction; a failed `manageInvoice`
    call (transport-layer or application-layer) left NO audit
    trail, contradicting the §8 design intent. ADR-0032 realises
    the §8 design intent at the transaction-shape level.
  - **ADR-0009 §5** — the operator-unblock surface
    (`retry-submission` / `mark-abandoned`) for stuck invoices.
    Before ADR-0032 the precondition surface was state-3 only
    (Response exists, no terminal ack). ADR-0032 extends the
    surface to state-2 (Attempt exists, no Response, no
    AttemptFailed required) so a transport-mid-flight loss is
    operator-recoverable without needing Layer-2
    `queryInvoiceCheck` to disambiguate first. The two stuck
    stages share the same retry command shape; the precondition
    walker tells the operator which stage they're in via the
    operator-visible summary.
  - **ADR-0009 §5 Layer-2 idempotency** — `queryInvoiceCheck`
    against the invoice number to disambiguate "NAV already
    has this submission" from "the wire broke before NAV saw
    it." ADR-0032 does NOT introduce `queryInvoiceCheck`;
    a state-2 retry today may produce a duplicate submission
    on the NAV side if the prior Attempt actually reached NAV
    (Layer-1 idempotency — the IssueInvoiceCommand ULID — does
    not protect against this; Layer-1 only protects against
    duplicate ABERP-side issuance commands). The
    `queryInvoiceCheck` surface remains deferred to its own ADR
    per ADR-0009 §5 + §"Open questions"; ADR-0032 surfaces
    the residual loud in the operator-visible message when a
    state-2 retry is invoked (CLAUDE.md rule 12 — do not hide
    the risk).
  - **ADR-0031 §3** — the `drain-submission-queue` worker's
    per-invoice pipeline. ADR-0032 amends the drain's
    transaction shape (TX1 + TX2 per invoice instead of one
    combined tx) so the drain's per-invoice audit posture
    matches `submit-invoice`'s post-ADR-0032 shape. The
    drain's pending-classification rule
    (`submission_queue::pending_from_ledger`) grows a third
    predicate clause: exclude invoices that already have an
    `InvoiceSubmissionAttempt` entry (drain handles only
    pure-Draft invoices; Attempted invoices flow through
    `retry-submission`).
  - **ADR-0008 §"Storage"** — transactional posture: "Entries
    are written in the same transaction as the state change
    they describe." ADR-0032's two-tx posture is consistent
    with this: TX1 atomically writes the Attempt entry; TX2
    atomically writes the Response (or AttemptFailed) entry.
    The state change between TX1 and TX2 is the NAV wire
    interaction itself — it is not an ABERP-side state change
    and is therefore not bound to either tx. The mirror-file
    sync (ADR-0030 §2) fires after each commit; a crash
    between TX1 commit and TX2 commit leaves a mirror that
    reflects only the Attempt — exactly the state the audit
    bundle should show for "we tried, the wire broke."
  - **ADR-0030 §2** — the audit-ledger mirror file. The
    two-tx posture adds one extra mirror sync per submission
    (one after TX1, one after TX2). The mirror's
    append-only invariant is unchanged; the per-call
    `sync_mirror` cost doubles per submission but the
    mirror's bytes are O(entries) which is unchanged.
  - **F12 — four-coordinated-edit ritual.** Tenth landing.
    `InvoiceSubmissionAttemptFailed` is the new variant; the
    four coordinated edits are the variant body in `EventKind`,
    the `as_str` arm, the `from_storage_str` arm, and the
    `round_trip_for_every_variant` hand-listed test array.
    Same posture as PR-8 / PR-10 / PR-11 / PR-12 / PR-13 /
    PR-14 / PR-15's prior firings.
  - **Session-22 handoff §"Suggested next session sub-split"**
    — named PR-19 = F40 closure as the strongest pick (rationale:
    the offline queue's correctness depends on it; transport-
    mid-flight loss residual stands per PR-18 adversarial review
    #6; lifts the largest deferred surface in the issuing path;
    lays the foundation for the eventual `queryInvoiceCheck`
    Layer-2 surface).
- **Source material:** ADR-0009 §8 (the parent posture), ADR-0009
  §5 (the operator-unblock surface), ADR-0031 §3 (the drain
  pipeline shape), session-22 handoff §"Suggested next session
  sub-split" (the pre-PR-19 reading list), `apps/aberp/src/
  submit_invoice.rs` (the existing single-tx submit surface
  PR-19 rewrites), `apps/aberp/src/retry_submission.rs` (the
  existing operator-unblock surface PR-19 extends), `apps/aberp/
  src/drain_submission_queue.rs` (the PR-18 drain pipeline
  PR-19 amends), `apps/aberp/src/audit_query.rs` (the
  precondition walker PR-19 extends), `apps/aberp/src/
  submission_queue.rs` (the predicate PR-19 amends),
  `crates/nav-transport/src/operations/manage_invoice.rs`
  (the call function PR-19 splits).

## Context

After ADR-0031 / PR-18 closed the offline-submission-queue at
the infrastructure level and the drain worker is end-to-end
operational, the loudest remaining ADR-0009 gap on the
issuing-side surface is §8's `invoice.submission_attempt`
"Fires before the response is received" promise. The
operator-visible problem: a NAV inspector reading the
audit-evidence bundle (ADR-0009 §8) sees that ABERP issued
an invoice (the `InvoiceDraftCreated` entry) and that ABERP
recorded a `manageInvoice` response (the
`InvoiceSubmissionResponse` entry), but the bundle is silent
on every case where the wire broke between POST and response,
the NAV adapter timed out before NAV replied, or NAV rejected
the request at the application layer. The audit ledger is
silent on every failed attempt — exactly the failure mode
ADR-0009 §8's "before the response is received" wording
exists to prevent.

The current shape — PR-7-B-3 / PR-18 — writes both Attempt
and Response in one DuckDB transaction after `manageInvoice`
returns success. The rationale named in `submit_invoice.rs`
("Why two audit appends instead of one") is correct as far as
it goes: splitting Attempt from Response gives a coherent
"we tried" vs "we succeeded" trace. The single-tx posture
violates the spirit of the split — if `manageInvoice` returns
an error, NEITHER entry is written. PR-18's adversarial review
#6 acknowledged this as residual.

PR-19 closes F40 at the issuing-path level by splitting the
single tx into two: TX1 (Attempt-before-call) is the
unconditional "we are about to POST X" record; TX2 is the
conditional "NAV said Y" (Response) or "the attempt failed
because Z" (AttemptFailed) record. The Layer-2
`queryInvoiceCheck` surface — the belt-and-braces
NAV-side reconciliation that would let a state-2 retry
disambiguate "NAV already has this submission" from "the
wire broke before NAV saw it" — remains deferred to its own
ADR per ADR-0009 §5.

### Prerequisite-gate state at PR-19 time

- **ADR-0009 §8** — Attempt-before-call design intent UNMET
  at the transaction-shape level. PR-19 closes this gap.
- **ADR-0009 §7** — Closed at the infrastructure level by
  ADR-0031 / PR-18. PR-19 amends the drain's tx shape but
  does not change the drain's queue-membership semantics
  (other than adding the third-predicate clause to exclude
  Attempted invoices from the drain — they now flow through
  retry-submission instead).
- **ADR-0009 §5** — Operator-unblock surface UNMET for state-2
  (Attempt-without-Response). The existing PR-8 surface
  handles state-3 only. PR-19 extends the precondition walker
  to accept state-2 with the same `retry-submission` command
  shape.
- **ADR-0009 §5 Layer-2 `queryInvoiceCheck`** — Still deferred.
  Named in ADR-0009 §5 as the disambiguation surface for the
  state-2 retry's duplicate-submission residual; PR-19 surfaces
  the residual loud in the operator-visible summary and does
  not introduce `queryInvoiceCheck` itself.
- **ADR-0008 §"Storage"** — One-tx posture preserved per
  ADR-0008's design intent (the Attempt entry is atomically
  paired with the act of having decided to POST X; the
  Response entry is atomically paired with the act of having
  received Y). The two ABERP-side state changes (decision-to-
  POST and received-response) are distinct moments in time
  separated by an external system call; per-state-change tx
  shape is the natural mapping.

### What surfaced during PR-19 design

Three conflicts among prior-PR conventions and one
adversarial-review concern that PR-19's resolution must
account for, plus eight adversarial-review concerns surfaced
during the design pass.

**Surfaced conflict 1: Where does the request_xml live across
the two transactions?** The Attempt entry must record the
request body verbatim, so the bytes must be in hand at TX1
commit time. The Response entry records the same request_xml
copy plus the response body; PR-7-B-3's payload shape doesn't
have `request_xml` on the Response side (it lives on the
Attempt side only). Three readings:

- **Reading A:** Both TX1 and TX2 carry the request bytes.
  Trade-off: 2x storage cost for the request envelope;
  symmetric audit-evidence shape (Attempt + Response are
  paired records of the same wire bytes). Rejected — the
  request bytes are immutable; storing them twice doubles
  the audit ledger's footprint for the most-frequent invoice
  lifecycle event without adding evidence.
- **Reading B:** TX1 carries the request bytes (Attempt);
  TX2 carries only the response bytes (Response). The
  audit-evidence bundle reconstructs the request from the
  Attempt entry, the response from the Response entry, and
  the pairing is via `invoice_id` + temporal ordering.
  Trade-off: the bundle reader needs to walk to a different
  entry to find the request bytes, and a Response entry on
  its own is missing context. Accepted — this is the
  existing PR-7-B-3 shape; PR-19 preserves it.
- **Reading C:** A new `InvoiceSubmissionWireEvent` EventKind
  unifies Attempt + Response into one variant with a typed
  discriminator field. Trade-off: rewrites the audit-evidence
  bundle reader; collapses the F12 four-edit ritual surface
  area; orthogonal to F40's intent. Rejected — invasive
  refactor unrelated to F40; per CLAUDE.md rule 3.

**Decision: Reading B.** PR-19 preserves the existing
PR-7-B-3 payload shape: Attempt carries `request_xml`,
Response carries `response_xml`. The new AttemptFailed
payload (§2) carries the error class + code + message + the
verbatim response body if one was received before the error
fired (e.g., for a non-success HTTP status the body is
present; for a transport-layer error the body is `None`).

**Surfaced conflict 2: How does the new EventKind shape
classify the failure?** A failed submission can fail at
multiple layers: transport (TLS / DNS / socket), HTTP status
(non-2xx response), application-layer (NAV-side error code
in the response body), envelope construction (rare; would
indicate a programmer error), or credential (missing
keychain entry). Three readings:

- **Reading A:** One new EventKind variant per failure class
  (`InvoiceSubmissionAttemptTransportFailed`,
  `InvoiceSubmissionAttemptApplicationFailed`,
  `InvoiceSubmissionAttemptCredentialFailed`, etc.).
  Trade-off: discriminator-by-kind is the maximum
  bundle-reader clarity; F12 ritual fires N times. Rejected
  — each class is a sub-class of "the wire call failed,"
  not a structurally distinct event. The bundle reader
  needs the class for diagnosis, not for routing.
- **Reading B:** One new EventKind variant
  (`InvoiceSubmissionAttemptFailed`) with a typed
  `error_class` field that discriminates among the
  sub-classes. Trade-off: F12 ritual fires once; bundle
  reader sees one kind for all failures and dispatches by
  payload field. Accepted — matches the granularity of
  the existing `InvoiceRetryRequested` / `InvoiceMarkedAbandoned`
  pair (operator-decision events that share a precondition
  shape).
- **Reading C:** Reuse `InvoiceSubmissionResponse` with a
  typed `outcome` field that discriminates between
  success-with-transaction-id and failure-with-error-class.
  Trade-off: breaks the existing payload schema; bundle
  readers that filter by kind would silently misclassify
  a failure as a success. Rejected — payload-schema
  breakage is the F12 trap PR-19 is supposed to avoid,
  not exercise.

**Decision: Reading B.** PR-19 adds one new EventKind
variant `InvoiceSubmissionAttemptFailed` with a typed
`error_class` field on the payload. The classes are
enumerated in §2 below as a string field (not a sub-enum
in the schema — the audit ledger's payload schema is JSON
without enum constraints; the Rust-side discipline is in
the constructor).

**Surfaced conflict 3: Does retry-submission's existing
precondition walker accept state-2 or does it stay
state-3-only?** Three readings:

- **Reading A:** A new operator-facing command (e.g.,
  `retry-pending`) handles state-2 distinctly from the
  existing `retry-submission` (state-3 only). Trade-off:
  forces operator decision at the command boundary;
  doubles the operator surface area. Rejected — the
  underlying recovery action is identical (retry the
  `manageInvoice` POST with the same bytes); only the
  precondition's evidence differs.
- **Reading B:** Extend `retry-submission` to accept state-2
  in addition to state-3. The precondition walker classifies
  which stage the invoice is in (Pending vs AwaitingAck);
  the orchestration writes the same RetryRequested + Attempt
  + Response/AttemptFailed shape regardless of stage.
  Trade-off: the existing operator-visible message shape
  changes slightly (the "prior_transaction_id" field is
  `None` for state-2); the `InvoiceRetryRequestedPayload`'s
  `prior_transaction_id` field becomes optional. Accepted —
  matches the operator's mental model ("the invoice is
  stuck, I retry"); the precondition walker's stage
  classification is internal to the audit-query helper.
- **Reading C:** The drain command (not retry-submission)
  handles state-2. Trade-off: drain becomes both
  pre-submission and post-Attempt; the FIFO ordering
  contract conflicts with operator-driven retry intent
  (drain is automatic; retry-submission requires operator
  reason text). Rejected — drain is intentionally automatic
  per ADR-0031 §3; state-2 retry requires operator
  acknowledgement of the duplicate-submission residual
  (Layer-2 `queryInvoiceCheck` is still deferred).

**Decision: Reading B.** PR-19 extends `retry-submission` to
accept state-2; the `StuckPrecondition` struct grows a
`stage: StuckStage` field; the `prior_transaction_id` field
becomes `Option<String>`; the `InvoiceRetryRequestedPayload`'s
`prior_transaction_id` field becomes `Option<String>` (carry-
forward of the precondition's shape via the F8 contract).
The operator-visible message names the stage explicitly
(CLAUDE.md rule 12 — name the residual loud).

## Decision

### 1. Two-tx submission posture (Attempt-before-call)

The single-tx submission posture (PR-7-B-3 / PR-18) splits
into a two-tx posture for every NAV `manageInvoice`-bearing
command (`submit-invoice`, `retry-submission`,
`drain-submission-queue`):

- **TX1 — Attempt-before-call.** Open one DuckDB transaction;
  append exactly one `InvoiceSubmissionAttempt` entry whose
  payload carries the verbatim `<ManageInvoiceRequest>` bytes
  (rendered via `crate::soap::render_manage_invoice_request`
  with the freshly-issued `tokenExchange` decrypted token);
  commit. For `retry-submission`, TX1 also writes the
  preceding `InvoiceRetryRequested` entry under the same tx
  per ADR-0009 §5 (the operator's decision and the resulting
  Attempt are atomically paired so a half-written
  retry-decision-without-evidence is impossible).
- **NAV call.** POST the pre-rendered envelope via
  `manage_invoice::send_built_request` (the split helper
  introduced in §3 below). Parse the response. Classify
  errors per the existing `NavTransportError` enum.
- **TX2 — Response or AttemptFailed.** Open a second DuckDB
  transaction.
  - On success: append exactly one
    `InvoiceSubmissionResponse` entry whose payload carries
    the verbatim `<ManageInvoiceResponse>` bytes + the
    parsed `transaction_id`.
  - On failure: append exactly one
    `InvoiceSubmissionAttemptFailed` entry whose payload
    carries the typed error class + code + message + the
    verbatim response bytes (when one was received before
    the error fired) per §2 below.
  - Commit.

The mirror-file sync (ADR-0030 §2) fires after each commit;
a process crash between TX1 commit and TX2 commit leaves
the mirror reflecting only the Attempt entry. The recovery
state — Attempt with no Response and no AttemptFailed — is
state-2 Pending per §4 below; `retry-submission` handles it.

### 2. New EventKind + payload

A new `EventKind` variant
`EventKind::InvoiceSubmissionAttemptFailed` is added to
`crates/audit-ledger/src/entry/event_kind.rs`. The on-disk
string is `invoice.submission_attempt_failed`. The F12
four-coordinated-edit ritual fires for the tenth time across
PR-6.1 / PR-7-B-3 / PR-8 / PR-10 / PR-11 / PR-12 / PR-13 /
PR-14 / PR-15 / PR-19 (variant body + `as_str` arm +
`from_storage_str` arm + `round_trip_for_every_variant`
hand-listed array).

The matching payload type
`audit_payloads::InvoiceSubmissionAttemptFailedPayload`
lives in `apps/aberp/src/audit_payloads.rs` with this shape:

```rust
pub struct InvoiceSubmissionAttemptFailedPayload {
    /// Prefixed `inv_<ULID>` form — same shape as every other
    /// invoice-bearing payload.
    pub invoice_id: String,
    /// The F8 idempotency key carry-forward — same canonical
    /// form as every other NAV-related entry for this invoice.
    pub idempotency_key: String,
    /// `"test"` or `"production"` — same shape as
    /// `InvoiceSubmissionAttemptPayload.endpoint`. The audit-
    /// evidence bundle (ADR-0009 §8) needs the environment
    /// explicit for inspector triage.
    pub endpoint: String,
    /// Failure class string. Enumerated values:
    /// `"transport"` — TLS / DNS / socket failure (the wire
    ///     broke; NAV may or may not have processed the
    ///     submission). The residual that motivates the
    ///     Layer-2 `queryInvoiceCheck` surface.
    /// `"http_status"` — non-2xx HTTP response from NAV.
    /// `"application"` — NAV-side application error
    ///     (`INVALID_SECURITY_USER`, `SCHEMA_VIOLATION`, etc.);
    ///     non-retryable per ADR-0009 §5.
    /// `"retryable_application"` — NAV-side retryable error
    ///     (`OPERATION_FAILED`, HTTP 504 per ADR-0009 §5).
    /// `"envelope"` — envelope construction failure (rare;
    ///     indicates a programmer error or upstream
    ///     quick-xml change).
    /// `"credential"` — keychain access failure
    ///     (KeychainItemMissing / KeychainBackend).
    /// `"client_build"` — reqwest::Client construction failure.
    pub error_class: String,
    /// NAV error code (when `error_class == "application"` or
    /// `"retryable_application"`) or HTTP status as decimal
    /// string (when `error_class == "http_status"`) or `None`
    /// for transport / envelope / credential / client_build
    /// classes.
    pub error_code: Option<String>,
    /// Operator-visible error message — the
    /// `NavTransportError::Display` rendering of the failure.
    /// Never includes secret material per the
    /// `NavTransportError::Display` implementation discipline
    /// (ADR-0020 §3).
    pub error_message: String,
    /// Verbatim response bytes IF a response body was
    /// received before the error fired (e.g., for
    /// `http_status` and `application` / `retryable_application`
    /// classes — NAV's error response body carries the
    /// `<funcCode>` + `<errorCode>` + `<message>` triple
    /// the bundle reader uses for diagnosis). `None` for
    /// `transport` / `envelope` / `credential` /
    /// `client_build` classes where no response body exists.
    pub response_xml: Option<Vec<u8>>,
}
```

The classification of `NavTransportError` variants into the
`error_class` string is deterministic (CLAUDE.md rule 5) and
lives in a single classifier function
`submission_queue::classify_attempt_failure` (next to the
existing `is_transport_error` classifier — same module, same
test coverage discipline). Adding a new
`NavTransportError` variant requires adding its arm to the
classifier; the default arm classifies as `"application"`
(the safe direction — misclassification as application
keeps drain continuing on a real outage rather than
mis-stopping on a real application error).

### 3. nav-transport `manage_invoice::call` split

`crates/nav-transport/src/operations/manage_invoice.rs`
gains two new public functions:

- `manage_invoice::build_request(credentials, tax_number_8,
  exchange_token, items) -> Result<Vec<u8>, NavTransportError>`
  — renders the `<ManageInvoiceRequest>` envelope bytes via
  the existing `crate::soap::render_manage_invoice_request`
  helper. No wire. Surfaces every existing envelope-
  construction error (`ManageInvoiceEmpty`,
  `ManageInvoiceTooManyItems`, `EnvelopeWriteFailed`).
- `manage_invoice::send_built_request(transport, request_xml)
  -> Result<SendBuiltRequestOutcome, NavTransportError>` —
  takes the pre-rendered envelope bytes, POSTs to
  `<endpoint>/manageInvoice`, captures the response verbatim,
  parses, classifies errors. Returns
  `SendBuiltRequestOutcome { transaction_id, response_xml }`
  (no request_xml — that lives with the caller). Surfaces
  every existing send-path error (`ManageInvoiceHttp`,
  `ManageInvoiceHttpStatus`, `ManageInvoiceResponseParse`,
  `ManageInvoiceNonRetryable`, `ManageInvoiceRetryable`).

The existing `manage_invoice::call` is retained verbatim
as a backward-compatibility wrapper that calls
`build_request` then `send_built_request` and assembles
the existing `ManageInvoiceOutcome` return shape. No
caller is forced to migrate; `submit_invoice` /
`retry_submission` / `drain_submission_queue` migrate to
the split helpers per §6 below because they each need the
TX1 → wire → TX2 ordering.

The two new helpers are NOT mirrored on
`manage_annulment` (PR-13) or other operations in this PR.
PR-19's scope is the invoice-issuing path (F40); the
annulment-side surface remains the existing single-call
`manage_annulment::call` shape. A future PR that wants the
same Attempt-before-call posture for the annulment surface
would split `manage_annulment::call` symmetrically — named
trigger: F40-equivalent finding on the annulment side after
NAV-testbed verification.

### 4. retry-submission state-2 acceptance + StuckStage

`apps/aberp/src/audit_query.rs` extends the
`StuckPrecondition` struct with a new `stage: StuckStage`
field and a stage-classifying walker. The new shape:

```rust
pub enum StuckStage {
    /// Attempt exists, no Response, no Abandoned. State-2 per
    /// PR-19. Transport-mid-flight loss residual: NAV may have
    /// processed the prior Attempt's submission — Layer-2
    /// `queryInvoiceCheck` is the named-deferred disambiguation
    /// surface (see ADR-0009 §5). Operator retry produces a
    /// fresh Attempt-Response pair via `retry-submission`; the
    /// operator-visible summary names the duplicate-submission
    /// residual loud.
    Pending,
    /// Response exists, no terminal ack, no Abandoned. State-3
    /// per the existing PR-8 / ADR-0009 §5 surface. The
    /// transaction was accepted by NAV but the ack poll either
    /// did not reach a terminal status or never ran. Operator
    /// retry produces a fresh Attempt-Response pair.
    AwaitingAck,
}

pub struct StuckPrecondition {
    pub stage: StuckStage,
    /// `Some` for `AwaitingAck` (from the prior `InvoiceSubmissionResponse`);
    /// `None` for `Pending` (no prior Response exists yet).
    pub prior_transaction_id: Option<String>,
    /// `Some` for `AwaitingAck` when an `InvoiceAckStatus` entry
    /// exists; `None` for `Pending` (no ack poll possible
    /// without a Response).
    pub prior_last_ack_status: Option<String>,
    /// The F8 idempotency key — taken from the prior
    /// `InvoiceSubmissionResponse` for `AwaitingAck`, or from
    /// the prior `InvoiceSubmissionAttempt` for `Pending`.
    pub idempotency_key: IdempotencyKey,
}
```

The precondition walker (`audit_query::stuck_precondition`)
classifies in this order:

1. `InvoiceMarkedAbandoned` exists → `NotStuck(AlreadyAbandoned)`.
2. Latest `InvoiceSubmissionResponse` exists:
   - Latest `InvoiceAckStatus` is `"SAVED"` → `NotStuck(AlreadyFinalized)`.
   - Latest `InvoiceAckStatus` is `"ABORTED"` → `NotStuck(AlreadyRejected)`.
   - Else → `Stuck(AwaitingAck, prior_transaction_id=Some, ...)`.
3. Latest `InvoiceSubmissionAttempt` exists → `Stuck(Pending,
   prior_transaction_id=None, prior_last_ack_status=None,
   idempotency_key=from Attempt payload)`.
4. Else → `NotStuck(NeverSubmitted)`.

The presence of an `InvoiceSubmissionAttemptFailed` entry
does NOT change the classification. An Attempt followed by
an AttemptFailed is still state-2 Pending (the operator may
retry; multiple failures accumulate in the audit chain as
evidence). The audit-evidence bundle reader sees the
Attempt + AttemptFailed + Attempt + Response sequence and
infers the retry chain.

`InvoiceRetryRequestedPayload.prior_transaction_id` becomes
`Option<String>` to carry the `StuckPrecondition.prior_transaction_id`
verbatim. The `#[serde(default)]` attribute keeps pre-PR-19
entries readable (their `prior_transaction_id` is the
existing `String` shape, which round-trips into the new
`Option<String>` via JSON's non-null parsing). The
operator-visible message for state-2 names
`<no prior NAV transaction id — state-2 Pending>` explicitly.

### 5. submission_queue third-predicate clause

`apps/aberp/src/submission_queue.rs` extends
`classify_pending`'s exclusion set with a third predicate
clause: invoices with any `InvoiceSubmissionAttempt` entry
are excluded from the pending list (drain skips them).

The three predicates are now:

- No `InvoiceSubmissionResponse` for this invoice. (Existing.)
- No `InvoiceMarkedAbandoned` for this invoice. (Existing.)
- No `InvoiceSubmissionAttempt` for this invoice. (NEW.)

The rationale: an invoice with an Attempt entry is either
in-flight (race with the drain), state-2 Pending (failed
mid-flight), or about to land a Response (which would
re-exclude it on the next drain run). Drain's automatic
posture is wrong for any of these states; the operator's
explicit `retry-submission` is the correct surface. Drain
handles only pure-Draft invoices.

The fourth predicate clause (`InvoiceSubmissionAttemptFailed`
existence) is NOT added. An AttemptFailed entry exists IFF
an Attempt entry exists (the two are written in TX1 + TX2
of the same submission); excluding by AttemptFailed alone
would be redundant. The Attempt exclusion subsumes it.

### 6. Per-command tx-shape rewrite

The three NAV `manageInvoice`-bearing commands each rewrite
to the two-tx posture per §1 above:

- **`submit-invoice` (`apps/aberp/src/submit_invoice.rs`).**
  Phase split: prepare (tokenExchange + envelope build,
  no wire), TX1 (write Attempt), send (POST + parse), TX2
  (write Response or AttemptFailed). The existing call_nav
  helper splits into `prepare_for_attempt_audit` (returns
  `(NavTransport, decoded_token, request_xml)`) and
  `send_built_manage_invoice` (takes the prepared bundle
  and the request_xml, returns the
  `SendBuiltRequestOutcome`). Both phases run on the same
  tokio current-thread runtime.

- **`retry-submission` (`apps/aberp/src/retry_submission.rs`).**
  Same split as submit-invoice, with TX1 widened to also
  write the `InvoiceRetryRequested` entry (operator
  decision + Attempt are atomically paired). The
  precondition walker (`audit_query::stuck_precondition`)
  is consulted before any NAV call; the F8 idempotency-key
  mismatch check (which previously checked against the
  Response's key) now checks against the precondition's
  key (which is the Attempt's key for state-2 and the
  Response's key for state-3 — both equal the original
  issuance's key per the F8 contract).

- **`drain-submission-queue` (`apps/aberp/src/drain_submission_queue.rs`).**
  Per-invoice pipeline shifts from one-tx-per-invoice to
  two-tx-per-invoice. The `DrainPerInvoiceError` enum is
  unchanged; the classification (transport vs application)
  drives both the loop's break/continue decision AND the
  AttemptFailed payload's `error_class` field. The drain's
  pending-classification (via `submission_queue::pending_from_ledger`)
  picks up the §5 third-predicate-clause change for free —
  no drain-side code change for the predicate.

### 7. Deferred scope

Three sub-surfaces are NAMED here and DEFERRED per
CLAUDE.md rule 2 + ADR-0021's just-in-time-ADR posture.
Each has a named trigger and a one-line description of the
failure mode that fires when the trigger arrives.

- **F44 — Layer-2 `queryInvoiceCheck` reconciliation.**
  ADR-0009 §5 names this as the disambiguation surface for
  "did NAV actually receive the prior Attempt?" Today
  state-2 retry produces a potential duplicate-submission
  on NAV (loud-warned in the operator-visible summary; not
  silent). Named trigger: first PR introducing the
  `queryInvoiceCheck` operation in `nav-transport`. Likely
  amends `retry-submission` + `drain` to consult
  `queryInvoiceCheck` before re-POSTing for state-2 invoices.

- **F45 — Automatic state-2 retry loop.** PR-19's
  state-2 retry is operator-driven (the operator runs
  `retry-submission` after seeing the audit bundle's
  Pending state). A future operational pattern may want an
  automatic retry after N minutes of state-2 — same shape
  as the drain's per-run loop but stage-aware. Named
  trigger: first operator request OR NAV-testbed
  verification of a transport-flake pattern.

- **F46 — Operator-tunable attempt-failed alert thresholds.**
  Per-tenant config surface for "alert when the count of
  AttemptFailed entries in the last N minutes exceeds M"
  (the rate-based outage detector). Today every AttemptFailed
  surfaces as a per-invoice LOUD per CLAUDE.md rule 12 but
  there is no aggregate alert. Named trigger: first PR
  introducing the operator config file surface (likely the
  same trigger as F42).

## Consequences

**Positive**

- The audit-evidence bundle (ADR-0009 §8) now carries
  evidence for EVERY submission attempt, success or
  failure. The "the wire broke and ABERP forgot we tried"
  failure mode is structurally impossible — TX1 commits the
  Attempt BEFORE the wire send.
- The transport-mid-flight loss residual that PR-18's
  adversarial review #6 acknowledged is operator-recoverable
  via state-2 `retry-submission` without requiring Layer-2
  `queryInvoiceCheck` first. The duplicate-submission residual
  is loud-warned, not silent.
- The drain worker's predicate (third clause) cleanly
  separates "we never tried" (drain handles) from "we tried
  and got stuck" (retry-submission handles). Operators no
  longer need to walk the audit bundle to decide which
  command applies.
- The nav-transport split (`build_request` +
  `send_built_request`) is a one-time refactor that future
  Attempt-before-call ports (e.g., to the annulment side,
  query-invoice-data, etc.) can replicate with the same
  shape. No callers are forced to migrate today.
- The F12 four-edit ritual fires for the tenth time. The
  ritual continues to perform its job — the trap is
  caught at test time, not at runtime.

**Negative**

- One extra DuckDB transaction per submission. The fixed
  cost is a `Connection::transaction()` + `commit()` pair;
  on a single-writer DuckDB tenant DB this is sub-millisecond
  but non-zero. Per-invoice latency increase is dominated by
  the NAV round-trip (orders of magnitude larger).
- One extra mirror-file `sync_mirror` call per submission
  (one after TX1, one after TX2). The mirror file's bytes
  grow O(entries) which is unchanged; the syscalls per
  submission double for the mirror sync.
- The state-2 retry's duplicate-submission residual stands
  until Layer-2 `queryInvoiceCheck` lands. PR-19 surfaces
  the residual loud (operator-visible summary names it);
  Layer-2 is the named-trigger for the disambiguation.
- The `InvoiceRetryRequestedPayload.prior_transaction_id`
  field becomes `Option<String>`. Pre-PR-19 entries
  deserialise into the new shape transparently (JSON
  non-null → `Some(String)`), but the operator-visible
  summary's wording changes for state-2 retries.
- The `nav-transport` crate's public surface grows by two
  functions. The existing `call` wrapper is retained for
  backward compat, but a future refactor that removes
  `call` would touch every test fixture that uses it.

**Locked in**

- Once an `InvoiceSubmissionAttemptFailed` entry is written
  to the audit ledger, it is immutable per ADR-0008 (the
  hash chain locks it in). A future ADR that changes the
  `error_class` enumeration (e.g., splits `"transport"`
  into `"tls"` + `"dns"` + `"socket"`) must add the new
  classes as additional valid strings; the existing
  `"transport"` entries remain valid for historical
  reading. No retroactive reclassification.
- The two-tx posture commits to a per-submission ordering
  of "Attempt first, Response/Failed second." A future
  refactor that wants to batch multiple submissions into
  one tx (e.g., a hypothetical batch-submit command) would
  need to either keep the per-invoice two-tx posture (one
  tx per Attempt across the batch, one tx per Response
  across the batch) or file a new ADR that explicitly
  amends ADR-0032 §1.

## Adversarial review

A hostile NAV inspector and a hostile-engineer review, in
alternation.

1. **"Two transactions per submission means more state in
   flight. Why is this safer than the one-tx posture?"** The
   one-tx posture's failure mode is silent (no audit entry on
   error). The two-tx posture's failure mode is visible (an
   Attempt entry exists in the ledger; the absence of a
   Response or AttemptFailed is itself the evidence that
   something happened mid-flight). Inspectors prefer visible
   to silent; the project owner's stated bar (CLAUDE.md rule
   12 — "fail loud") favours the two-tx posture.

2. **"State-2 retry might produce a duplicate submission to
   NAV. Why is this acceptable?"** Acknowledged. ADR-0009 §5
   names Layer-2 `queryInvoiceCheck` as the eventual
   disambiguation surface; PR-19 surfaces the duplicate-
   submission residual loud in the operator-visible summary
   (the message explicitly names "this retry may produce a
   duplicate submission to NAV — the prior Attempt may have
   reached NAV before the wire broke"). Until Layer-2 lands,
   the operator absorbs the residual; the alternative (refuse
   state-2 retry entirely) leaves operators with no recovery
   path for a transport-mid-flight loss. Loud-acceptance is
   preferable to silent-refusal.

3. **"The new EventKind variant's classification is one
   string field. What if the inspector wants kind-alone
   classification per ADR-0026 §2's posture?"** Two reasons
   against kind-per-class. (a) The failure classes are
   sub-types of "the submission attempt failed," not
   structurally distinct events; they share the same
   precondition (an Attempt was written) and the same
   downstream behaviour (operator retry or operator
   abandon). (b) The F12 ritual fires N times for N
   variants; the audit-ledger crate's `EventKind` surface
   would grow by ~7 variants for a single failure-class
   axis. The error_class field on the payload provides the
   inspector the disambiguation; the bundle reader filters
   by payload field.

4. **"The mirror file sync doubles per submission. Does that
   matter for cost?"** Probably not — the mirror file write
   is an append + fsync; the syscall cost is sub-millisecond
   on local SSD and bounded by the NAV round-trip latency
   regardless. At hyperscale (F39 named trigger) the doubled
   sync cost is one of several factors that motivates the
   bundle-generation cost mitigation. The mitigation is
   scoped to F39's trigger, not PR-19's.

5. **"What if TX1 commits the Attempt but the process crashes
   before the NAV call begins?"** The Attempt entry is in the
   ledger; the mirror file is synced. `retry-submission`
   sees state-2 Pending and the operator can re-drive the
   submission. The audit-evidence bundle reflects "we
   intended to submit and crashed before sending"; the
   subsequent retry's Attempt + Response (or AttemptFailed)
   shows the recovery. The chain reads forward correctly.

6. **"What if TX2 fails to commit after a successful NAV
   call?"** NAV has the submission (with a `transaction_id`
   ABERP would have recorded), but ABERP's audit ledger
   shows only the Attempt. On `retry-submission`, the
   operator submits again — NAV may accept the duplicate
   (returning a different `transactionId`) or reject it with
   `INVOICE_NUMBER_NOT_UNIQUE`. The Layer-2 `queryInvoiceCheck`
   surface is the named-deferred disambiguation; until it
   lands, the duplicate-submission residual is the
   operator-visible warning. PR-19's adversarial-review #2
   already accepts this.

7. **"How does the audit-evidence bundle reader handle the
   new AttemptFailed entries?"** Existing readers that filter
   by `invoice.*` glob pick up the new entries automatically
   (the F11 / F12 prefix discipline holds — the new on-disk
   string is `invoice.submission_attempt_failed`). The bundle
   reader's existing filter logic dispatches on kind; the new
   kind is treated as evidence (verbatim bytes in payload)
   without requiring reader changes for v1. A future reader
   enhancement may render the error_class field for inspector
   triage; the data is in the payload either way.

8. **"What if the operator runs `mark-abandoned` against a
   state-2 Pending invoice (Attempt but no Response)?"** The
   existing `mark-abandoned` precondition walker uses the
   same `audit_query::stuck_precondition` helper; under PR-19
   it now accepts state-2 as a valid Stuck precondition. The
   operator's `mark-abandoned` writes the
   `InvoiceMarkedAbandoned` entry, which terminates the
   invoice in the audit ledger. Subsequent drain runs skip
   the invoice (the §5 third-predicate-clause check picks up
   the Abandoned exclusion via the existing second
   predicate). The invoice's NAV-side fate may still be
   "submitted but ABERP forgot" — the duplicate-submission
   residual is the same as the state-2 retry case;
   `mark-abandoned`'s operator-visible message names the
   residual loud per ADR-0009 §5 + CLAUDE.md rule 12.

9. **"You're adding more `Option<...>` to payload fields.
   Doesn't that make the JSON wire shape ambiguous?"** No.
   `Option<String>` serialises as either `null` or a string;
   `Option<Vec<u8>>` serialises as either `null` or a
   base64-encoded byte array (via serde_json's default Vec<u8>
   handling). The audit-evidence bundle reader's type-checked
   deserialisation through the typed payload structs preserves
   the distinction. Pre-PR-19 entries (without the new fields)
   deserialise via `#[serde(default)]` where applicable;
   PR-19's payload deltas (the new
   `prior_transaction_id: Option<String>` on the existing
   `InvoiceRetryRequestedPayload`) carry the same
   `#[serde(default)]` discipline.

## Alternatives considered

- **Keep the single-tx posture; write the Attempt entry AFTER
  manageInvoice returns success.** Rejected — explicitly the
  pattern F40 names as broken. ADR-0009 §8's "Fires before the
  response is received" wording is the design intent; preserving
  the single-tx posture preserves the silent-on-failure failure
  mode.

- **Three transactions per submission (TX1 = decision, TX2 =
  Attempt, TX3 = Response/Failed).** Rejected — adds tx
  overhead without adding evidence. The decision-to-submit is
  not a state change worth its own audit entry today (it would
  duplicate the existing `InvoiceDraftCreated`-implied
  intent). Future flexibility (CLAUDE.md rule 2 — "minimum
  code, no speculative abstractions") is the wrong principle
  to invoke here.

- **One new EventKind variant per failure class (transport,
  http_status, application, etc.).** Rejected — see §"Surfaced
  conflict 2 Reading A" above. F12 ritual fires N times for
  marginal classification benefit.

- **A separate operator command for state-2 retry
  (`retry-pending`) distinct from `retry-submission`.**
  Rejected — see §"Surfaced conflict 3 Reading A" above. Same
  underlying recovery action; doubling the operator surface
  area for a stage distinction that the precondition walker
  already classifies.

- **Refuse state-2 retry entirely (state-2 invoices require
  Layer-2 `queryInvoiceCheck` first).** Rejected — leaves
  operators with no recovery path for a transport-mid-flight
  loss until Layer-2 lands; Layer-2 has no firm trigger date
  (it depends on NAV-testbed verification + operational
  experience). The loud-warned state-2 retry is the
  operator-recoverable path today.

- **Reclassify the existing PR-18 drain to handle state-2 in
  addition to pure-Draft.** Rejected — see §"Surfaced conflict
  3 Reading C" above. Drain is automatic; state-2 requires
  operator acknowledgement of the duplicate-submission
  residual. The drain's predicate cleanly separates the two
  via the §5 third-predicate-clause exclusion.

- **Compose the two transactions into a single savepoint-
  bounded DuckDB unit instead of two distinct commits.**
  Rejected — savepoints inside a single tx do not survive
  process crash (the outer tx rolls back). The whole point of
  the two-tx posture is that TX1's commit is durable
  independently of TX2's outcome.

## Open questions

The full list of cross-cutting open questions is consolidated
in `docs/research/nav-and-billingo.md`; the items below
specifically block work that ADR-0032 touches:

- **Layer-2 `queryInvoiceCheck` operation in `nav-transport`.**
  Named in ADR-0009 §5; the named-trigger for F44 closure.
  Until it lands, the state-2 retry's duplicate-submission
  residual is operator-absorbed.
- **Automatic state-2 retry policy** (timeouts, backoff,
  attempts). Named-trigger F45; today every state-2 retry is
  operator-driven.
- **Operator-tunable attempt-failed alert thresholds**
  (count-in-window, age-of-oldest-AttemptFailed). Named-trigger
  F46; today every AttemptFailed is per-invoice LOUD with no
  aggregate alert.
- **NAV-testbed verification** of the transport-mid-flight
  loss frequency. Whether it's a real operational pattern
  determines whether F45's automatic-retry surface is
  load-bearing or a luxury.

## Follow-on ADRs unblocked by this decision

- **ADR — Layer-2 `queryInvoiceCheck` reconciliation
  (F44 closure).** First PR introducing the
  `queryInvoiceCheck` operation in `nav-transport`. Amends
  `retry-submission` to consult `queryInvoiceCheck` before
  re-POSTing for state-2 invoices; amends the drain similarly.
- **ADR — Automatic state-2 retry loop (F45 closure).**
  First operator request OR NAV-testbed verification of a
  transport-flake pattern.
- **ADR — Operator-tunable threshold config (F42 + F46
  joint closure).** First PR introducing the operator
  config file surface; lifts F42 (drain alert thresholds)
  and F46 (attempt-failed alert thresholds) together.
