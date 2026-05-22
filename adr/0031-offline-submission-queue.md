# ADR-0031 — offline submission queue — `pending` is derived from the audit ledger (no side table), the `drain-submission-queue` CLI worker processes pending invoices in FIFO by issue date, the `issue-invoice` path enforces the ADR-0009 §7 hard cap of 50 unsubmitted invoices, the alert thresholds (5 pending or 30-minute oldest) surface as drain-time WARN log lines, the XML path is recorded on `InvoiceDraftCreatedPayload` as a backward-compatible optional field, and offline detection is implicit via NAV transport-error propagation; closes ADR-0009 §7 at the infrastructure level

- **Status:** Accepted
- **Date:** 2026-05-22
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — first PR after
  ADR-0030 / PR-17 to extend the binary's submit-path surface.
  Lifts the last named ADR-0009 §7 infrastructure surface
  (the offline submission queue). Audit-ledger crate
  unchanged at the schema level (no new `EventKind`); the
  binary's `apps/aberp/src/` gains one new module
  (`submission_queue`) + one new CLI subcommand
  (`drain-submission-queue`); `InvoiceDraftCreatedPayload`
  gains one backward-compatible optional field
  (`nav_xml_path`); `issue-invoice` gains one pre-allocation
  cap check. Load-bearing deltas: §1 (queue membership —
  derived from audit ledger, no side table per CLAUDE.md
  rule 2), §2 (XML path — additive optional field on
  `InvoiceDraftCreatedPayload` with `#[serde(default)]`,
  consumed by drain at read time), §3 (drain CLI surface —
  FIFO by issue date, per-invoice token-exchange +
  manage-invoice, audit writes mirror `submit-invoice`'s),
  §4 (offline detection — implicit via NAV transport-error
  propagation, drain stops on first error), §5 (hard cap of
  50 — pre-allocation pending-count check in
  `issue-invoice::run_single_tx`), §6 (alert thresholds —
  drain-time WARN log lines, no control flow), §7 (deferred
  scope — failed-attempt audit trail, submission-deadline
  soft/hard gates, queue-driven token-exchange amortisation).
  Does **not** supersede ADR-0009, ADR-0030, or any prior
  ADR; all remain in force.
- **Related:**
  - **ADR-0009 §7** — the parent posture: "ABERP therefore
    queues `Ready` invoices and submits when NAV is
    reachable again. Queue: bounded. Hard cap 50 unsubmitted
    invoices. Operator alert thresholds (5 invoices in queue,
    or 30 minutes since the oldest unsubmitted). The
    submission worker runs on a single tenant connection,
    processes the queue in FIFO order by issue date, and
    writes `invoice.submission_attempt` audit entries for
    each try." ADR-0031 realises this posture at the
    infrastructure level. The submission-deadline soft/hard
    limits (24h soft / 72h hard via `ConfirmLateSubmission`)
    are NAMED in ADR-0009 §7 and DEFERRED here per §7 below
    (named-trigger finding F41).
  - **ADR-0009 §5** — the operator-unblock surface
    (`retry-submission` / `mark-abandoned`) for SubmissionStuck
    invoices. The drain command's scope is the PRE-submission
    queue (invoices that never reached `Submitted`); the §5
    retry surface remains the path for POST-submission stuck
    invoices. Two distinct precondition surfaces, two distinct
    commands; the drain refuses non-Ready states loud per
    CLAUDE.md rule 12 (§3 below).
  - **ADR-0008 §"Storage"** — transactional posture: "Entries
    are written in the same transaction as the state change
    they describe." The drain's audit writes (Attempt +
    Response) compose with the existing
    `submit-invoice`-style single-tx write per-invoice. The
    drain does NOT widen the tx boundary across multiple
    invoices — each invoice's submission is its own atomic
    unit (rollback semantics unchanged).
  - **ADR-0019** — storage strategy: relational source-of-
    truth, no foreign keys. The queue-membership predicate
    walks the audit ledger; no side table is introduced. This
    matches ADR-0019's no-second-source-of-truth posture
    (the audit ledger IS the source of truth for "is invoice
    X pending submission").
  - **ADR-0030** — the audit-ledger mirror file. The drain's
    audit writes flow through `append_in_tx` + post-commit
    `sync_mirror` per ADR-0030 §2; the per-invoice mirror
    sync is unchanged from PR-17's eleven CLI-command touches.
    The drain adds the twelfth touch (a 13th if `serve.rs`
    ever gains an append path, which it does not).
  - **Session-21 handoff §"Suggested next session sub-split"**
    — named PR-18 as the offline submission queue and listed
    three open design questions (storage location, offline
    detection, retry-submission interaction). ADR-0031
    resolves each in §1 / §4 / §3 respectively.
- **Source material:** ADR-0009 §7 (the parent posture),
  session-21 handoff §"Suggested next session sub-split"
  (the pre-PR-18 housekeeping list), `apps/aberp/src/
  submit_invoice.rs` (the existing single-shot submit
  surface PR-18 generalises), `apps/aberp/src/audit_query.rs`
  (the precedent module for typed audit-ledger reads).

## Context

After ADR-0030 / PR-17 closed F10 and the audit-evidence
chain is end-to-end durable (the bundle reader now consumes
the mirror file as second-source assertion; divergence
refuses the bundle output), the loudest remaining ADR-0009
gap is §7's offline submission queue. The operator-visible
problem: a NAV inspector visit may happen on a workstation
without NAV reachability, so invoice issuance MUST succeed
when NAV is unreachable; the submission then defers until
NAV is reachable again. PR-18 lands the queue + worker.

### Prerequisite-gate state at PR-18 time

- **ADR-0009 §7** — open at the infrastructure level. The
  parent posture exists in design but no code today
  enforces the hard cap, surfaces the alert thresholds, or
  processes a queue. `submit-invoice` is a single-shot
  command; the operator manually iterates over Ready
  invoices today.
- **ADR-0009 §8** — CLOSED at the audit-evidence-bundle
  level by ADR-0029 / PR-16 + ADR-0030 / PR-17. The
  remaining §8 surfaces are F5 (attestation signing), F38
  (bundle verifier tool), F36 (parsed `receiver_state`
  field, NAV-testbed gated).
- **ADR-0008 §"Storage"** — CLOSED at the mirror-file
  level by ADR-0030 / PR-17. The remaining §"Storage"
  surface is the long-term retention / cold-storage policy
  (still deferred at predicate level).
- **F12 four-edit ritual** — NOT fired by this PR. The
  drain command writes `InvoiceSubmissionAttempt` +
  `InvoiceSubmissionResponse` (existing PR-7-B-3 kinds);
  no new `EventKind` variant. The ritual remains at its
  ninth landing.

### Surfaced conflicts (CLAUDE.md rule 7)

Three ambiguities the build-phase will otherwise paper over:

1. **Queue storage — derived from the audit ledger, or a
   side DuckDB table, or a sibling JSON-Lines file.**
   Three readings:

   - **Reading A: A new DuckDB table
     `submission_queue(invoice_id, issue_date, xml_path,
     attempt_count, last_attempt_time_wall, last_error)`.**
     Operationally rich — drain reads the table directly,
     no audit-ledger walk per invocation. Rejected because:
     (a) introduces a second source of truth for "is
     invoice X pending submission" (the ledger already
     names it via `InvoiceDraftCreated` minus
     `InvoiceSubmissionResponse`); (b) drift between the
     table and the ledger is exactly the CLAUDE.md rule 7
     trap ("two patterns in the codebase? pick one"); (c)
     the operationally rich fields (attempt_count,
     last_error) are not in ADR-0009 §7's scope, so
     introducing them is feature creep per CLAUDE.md
     rule 2.

   - **Reading B: A sibling JSON-Lines file `<db>.queue.log`,
     mirror-file-shaped.** Operationally similar to the
     mirror file in posture. Rejected because: (a) same
     second-source-of-truth concern as Reading A; (b) the
     mirror file's value is unintentional-corruption
     recovery (ADR-0030 §"Adversarial review bullet 1"),
     not operational state — a queue file would conflate
     the two purposes; (c) ADR-0008 §"What goes in the
     ledger" already names "Every business state change
     (invoice issued, payment recorded, ...)" — the queue
     IS a business state derived from issued + not-yet-
     submitted, so the ledger IS the canonical surface.

   - **Reading C: Derive queue membership from the audit
     ledger** (this ADR's pick). The predicate:
     `InvoiceDraftCreated` exists AND no
     `InvoiceSubmissionResponse` for the same invoice AND
     no `InvoiceMarkedAbandoned` for the same invoice.
     Pure code, no LLM, no new table, no new EventKind.
     The audit ledger is the source of truth per ADR-0008
     §"Storage"'s transactional contract.

   PR-18 commits to **Reading C**. The cost is an O(n)
   ledger scan per drain invocation; F39 (bundle-generation
   cost at hyperscale) is the existing named-trigger for
   this concern. Per-tenant volumes (single company, not
   marketplace) put the cost in the milliseconds today.

2. **XML path — extend `InvoiceDraftCreatedPayload`, or
   `--xml-dir` convention, or persist verbatim XML bytes
   in a new audit payload.** Three readings:

   - **Reading A: Operator passes `--xml-dir <dir>` to
     drain; worker maps invoice_id to
     `<dir>/<invoice_id>.xml` by convention.** Rejected
     because: (a) it imposes a filename convention the
     operator must enforce themselves at
     `issue-invoice --out` time, which they currently
     choose freely; (b) historical invoices issued before
     PR-18 may have arbitrary `--out` paths; (c) a missing
     convention is a silent error mode (the worker looks
     for the file, doesn't find it, retries the wrong
     invoice) which is exactly the CLAUDE.md rule 12 trap.

   - **Reading B: Extend `InvoiceDraftCreatedPayload` with
     a `#[serde(default)] pub nav_xml_path: Option<String>`
     field; populated at issue time, consumed by drain at
     read time** (this ADR's pick). The audit_payloads.rs
     header explicitly names this case as backward-
     compatible: "Adding a field is backward-compatible
     (older readers see the old shape via `#[serde(default)]`
     if they choose to parse). Removing a field or changing
     a field's semantic shape requires a *new* `EventKind`
     variant." This is the add-a-field path, not the
     rename-a-kind path. Pre-PR-18 entries deserialise with
     `nav_xml_path: None`; drain loud-fails on those with a
     message naming `--xml-path-override` as the per-
     invocation escape hatch.

   - **Reading C: A new audit payload
     `InvoiceQueuedForSubmissionPayload` that captures the
     verbatim XML bytes (not the path) at issue time.**
     Most-faithful to the audit-grade verbatim posture
     ADR-0009 §8 uses for NAV interactions. Rejected
     because: (a) the verbatim XML is already on disk at
     the operator-chosen `--out` path — duplicating it
     into the audit payload doubles the storage cost; (b)
     adds a new EventKind (F12 ritual fires) for a purpose
     that the existing `InvoiceDraftCreated` already
     covers; (c) the audit-ledger crate's
     `InvoiceDraftCreatedPayload` comment names the payload
     as "a pointer, not a duplicate" — extending it with
     the on-disk path matches that posture exactly.

   PR-18 commits to **Reading B**. The optional-field
   extension is the minimum-code path (CLAUDE.md rule 2);
   the `#[serde(default)]` attribute is the explicit
   backward-compat contract; the per-invocation
   `--xml-path-override` flag is the operator-visible
   escape hatch for pre-PR-18 entries.

3. **Offline detection — explicit operator flag, implicit
   via NAV transport error, or both.** Three readings:

   - **Reading A: Operator passes `--offline` to refuse
     to drive any NAV calls.** Useful for a "process the
     drain when I get home" workflow. Rejected because
     the offline state is not a user-chosen mode in
     ADR-0009 §7's framing — §7's framing is "submit when
     NAV is reachable again," which is observed, not
     declared. A user-chosen `--offline` flag would also
     be feature creep per CLAUDE.md rule 2.

   - **Reading B: Drain attempts the first invoice; if the
     NAV call fails with a transport error
     (`NavTransportError::TokenExchangeHttp` /
     `ManageInvoiceHttp` / etc.), drain stops the loop and
     surfaces the error LOUD via tracing + stdout** (this
     ADR's pick). NAV-side application errors (
     `INVALID_SECURITY_USER`, `INCORRECT_REQUEST_SCHEMA`,
     etc.) DO NOT trigger stop — they are per-invoice
     failures that the operator addresses per invoice, and
     the drain moves on to the next invoice in queue.
     Distinguishing transport-vs-application is a match
     on the typed `NavTransportError` enum (deterministic
     code per CLAUDE.md rule 5).

   - **Reading C: Both — operator can pass `--offline` to
     skip; drain auto-detects via transport error
     otherwise.** Rejected because it bundles Reading A's
     feature-creep with Reading B's correctness, and the
     resulting two-axis state machine is harder to reason
     about per CLAUDE.md rule 2.

   PR-18 commits to **Reading B**. The match-on-typed-
   error path is in `submission_queue::is_transport_error`
   (a free function in the new module); the drain's loop
   body inspects the typed error path and short-circuits
   on transport errors only.

## Decision

### 1. Queue membership predicate — pure-derived from the audit ledger

**Module:** `apps/aberp/src/submission_queue.rs` (new).

**Definition.** An invoice is `pending submission` iff ALL
of the following hold:

- The audit ledger contains an `InvoiceDraftCreated` entry
  whose payload's `invoice_id` field equals the invoice's
  prefixed ULID.
- The audit ledger does NOT contain any
  `InvoiceSubmissionResponse` entry whose payload's
  `invoice_id` field equals the same invoice id.
- The audit ledger does NOT contain any
  `InvoiceMarkedAbandoned` entry whose payload's
  `invoice_id` field equals the same invoice id.

**Why three predicates, not two.** The two-predicate version
(`InvoiceDraftCreated` minus `InvoiceSubmissionResponse`)
would treat abandoned invoices as forever-pending — defeating
the operator's terminal-by-operator-decision (ADR-0009 §5).
The three-predicate version honours the
`InvoiceMarkedAbandoned`-is-terminal contract.

**`PendingInvoice` shape:** carries the invoice id (prefixed
ULID), the recorded XML path (`Option<String>` per §2 below),
the issue date (read from the `InvoiceSequenceReserved` entry
that precedes the `InvoiceDraftCreated` in the same issuance
tx — pinned by F8 idempotency_key linkage), and the
idempotency key (for the F8 contract carryforward to the new
Attempt / Response entries).

**`PendingInvoiceSet::from_ledger(&Ledger) -> Result<Vec<PendingInvoice>>`:**
walks the ledger once, classifies each
`InvoiceDraftCreated` entry, filters those with a matching
`InvoiceSubmissionResponse` or `InvoiceMarkedAbandoned`,
and returns the survivors in issue-date order. FIFO per
ADR-0009 §7. The function is loud per CLAUDE.md rule 12:
unparseable payloads, missing `InvoiceSequenceReserved`
predecessors (the F8 lookup), and idempotency-key parse
failures all surface as `Err(_)`.

### 2. XML path — additive optional field on `InvoiceDraftCreatedPayload`

**New field:**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceDraftCreatedPayload {
    pub invoice_id: String,
    pub line_count: usize,
    pub idempotency_key: String,
    /// NAV InvoiceData XML path the binary wrote at issue time
    /// (see `issue-invoice --out`, `issue-storno --out`,
    /// `issue-modification --out`). `None` for entries written by
    /// pre-PR-18 binaries (per the `#[serde(default)]` posture
    /// the audit-payloads header names) — drain loud-fails on
    /// those unless the operator passes `--xml-path-override`.
    #[serde(default)]
    pub nav_xml_path: Option<String>,
}
```

**New constructor** alongside the existing `from_invoice`:

```rust
impl InvoiceDraftCreatedPayload {
    pub fn from_invoice_with_xml_path(
        invoice: &ReadyInvoice,
        idempotency_key: IdempotencyKey,
        nav_xml_path: PathBuf,
    ) -> Self;
}
```

The three issue-* binary call sites (`issue_invoice`,
`issue_storno`, `issue_modification`) switch to
`from_invoice_with_xml_path(...)` and pass `args.out.clone()`.
The existing `from_invoice` REMAINS for the round-trip test
in `audit_payloads.rs::tests::draft_created_round_trip` (which
asserts the default-None path) and as the construct-without-
path surface should a future caller want it.

**Why `Option<String>` and not `String`.** Per the audit-
payloads header schema-versioning rule: adding a non-optional
field would change the payload's semantic shape and require a
new EventKind. The optional field with `#[serde(default)]`
preserves the existing kind and lets pre-PR-18 entries
deserialise cleanly.

### 3. Drain CLI surface — `drain-submission-queue`

**Subcommand:**

```
aberp drain-submission-queue \
    --tax-number <8-digit base or dashed form> \
    --tenant <id, default "default"> \
    --db <path, default "./aberp.duckdb"> \
    --endpoint test|production \
    [--xml-path-override <invoice-id>=<path>]... \
    [--max-invoices <N>]
```

**Pipeline (per invoice):**

1. Compute `PendingInvoiceSet::from_ledger(...)` against the
   tenant DB. FIFO by issue date.
2. Surface alert-threshold WARN lines (§6 below).
3. For each pending invoice (up to `--max-invoices` if set):
   a. Resolve the XML path: payload's `nav_xml_path` if
      `Some(...)`, else the `--xml-path-override` map for
      this invoice id, else loud-fail per CLAUDE.md rule 12.
   b. Read the XML bytes from disk; loud-fail if missing.
   c. Validate via `aberp_nav_xsd_validator::validate_invoice_data`
      (same pre-NAV gate every existing `submit-*` command runs).
   d. Open a tokio current-thread runtime; do tokenExchange +
      manageInvoice (same `call_nav` shape as
      `submit_invoice::call_nav`).
   e. Write `InvoiceSubmissionAttempt` + `InvoiceSubmissionResponse`
      in one DuckDB transaction (mirror of
      `submit_invoice::write_submission_audit_entries`).
   f. Re-open Ledger; `verify_chain` + `sync_mirror` per
      ADR-0030 §2.
   g. Print per-invoice OK line.
4. On transport error from step 3d: short-circuit the loop,
   surface the error LOUD via tracing + stdout, exit non-
   zero. Subsequent invoices remain pending.
5. On application error from step 3d (NAV non-success status,
   parser failure on response, etc.): record nothing in the
   audit ledger for that invoice (per the existing
   submit_invoice transaction posture — the audit writes
   happen only on NAV-OK), surface the error LOUD, and
   CONTINUE to the next invoice. ADR-0009 §7's "submission
   worker ... writes `invoice.submission_attempt` audit
   entries for each try" remains DEFERRED — the audit
   write-before-call requires a transaction posture change
   to `submit_invoice` that is OUT OF SCOPE for PR-18 (named
   F40 in §7 below).

**Why per-invoice token-exchange and not amortised.** NAV's
v3.0 protocol assigns a per-request token; tokens are not
multi-use. Each invoice in the drain queue therefore drives
its own tokenExchange. The drain's wall-clock cost is
dominated by the per-invoice round-trip (~1-2 seconds in
the NAV-test environment); amortising the token across N
invoices is not protocol-compliant.

**Why drain does NOT accept stuck invoices.** The drain's
precondition is "pending = never received a response." An
invoice with an `InvoiceSubmissionResponse` is by definition
NOT pending; the operator's path for that invoice is
`retry-submission` (if SubmissionStuck) or `poll-ack` (if
not yet terminally acked). Drain refuses to operate on
non-pending invoices implicitly — they don't appear in
`PendingInvoiceSet::from_ledger`'s output.

### 4. Offline detection — implicit via NAV transport error

**Free function in `submission_queue`:**

```rust
fn is_transport_error(err: &NavTransportError) -> bool;
```

Matches on the typed variant. Returns `true` for transport-
layer failures (`TokenExchangeHttp(...)`, `ManageInvoiceHttp(...)`,
`ClientBuild(...)`); `false` for NAV-side application errors
(`TokenExchangeHttpStatus { status }`,
`TokenExchangeResponseParse(...)`, etc.) and for credential
errors (`KeychainItemMissing`, `KeychainBackend`).

The drain's loop body inspects the typed error path
(downcast through `anyhow::Error::downcast_ref`) and
short-circuits on `is_transport_error`. Operator-visible
message: `"NAV transport error at invoice <ID>; drain
stopped. <N> invoices remain pending. Re-run when NAV is
reachable."`

### 5. Hard cap of 50 — pre-allocation pending-count check

**Constant in `submission_queue`:**

```rust
pub const HARD_CAP_PENDING: usize = 50;
```

**Check site:** the start of `issue_invoice::run` (and the
matching `issue_storno::run`, `issue_modification::run`).
After credentials are loaded (so the keychain is consulted
before the DB), BEFORE `run_single_tx` opens its transaction:

```rust
let pending_count = submission_queue::count_pending(&args.db, tenant.clone(), binary_hash_bytes)?;
if pending_count >= submission_queue::HARD_CAP_PENDING {
    return Err(anyhow!(
        "submission queue is full ({}/{} pending invoices per ADR-0009 §7); \
         run `aberp drain-submission-queue --endpoint <test|production>` to submit \
         the backlog, or `aberp mark-abandoned --invoice-id <id> --reason ...` \
         on invoices the operator has decided not to submit",
        pending_count,
        submission_queue::HARD_CAP_PENDING,
    ));
}
```

**Why pre-allocation, not post-allocation rollback.** Loud-
fail BEFORE allocating preserves the sequence-slot invariant
(ADR-0009 §3 — no gap-free reservation is burned on a
refused issuance). Post-allocation rollback would surface
the same observable outcome (the operator's CLI errors out)
but at the cost of an extra `Transaction::drop` roundtrip.

**Why `count_pending`, not a slice of `PendingInvoiceSet`.**
The cap-check path doesn't need the per-invoice metadata;
only the count. `count_pending` walks the same ledger entries
but builds an integer, not a `Vec<PendingInvoice>`. The
allocation cost difference is small but the type signature
makes the cap-check's read-only-ness loud at the call site.

### 6. Alert thresholds — drain-time WARN log lines

**Constants in `submission_queue`:**

```rust
pub const ALERT_PENDING_COUNT: usize = 5;
pub const ALERT_OLDEST_PENDING: Duration = Duration::from_secs(30 * 60);
```

**Surface site:** the start of `drain-submission-queue`'s
loop body, after `PendingInvoiceSet::from_ledger` returns:

- If `pending.len() >= ALERT_PENDING_COUNT`: emit
  `tracing::warn!(threshold = "count", count = N, ...)` and
  a stdout WARN line.
- If the oldest pending's issue_date is older than
  `ALERT_OLDEST_PENDING` ago: emit
  `tracing::warn!(threshold = "age", oldest = T, ...)` and
  a stdout WARN line.

The thresholds are operator-tunable per ADR-0009 §7 ("operator-
tunable; defaults set here") but the tunability surface is
DEFERRED to a future PR (the operator config file does not
exist yet — F42 in §7 below). The defaults are the ones
ADR-0009 §7 names verbatim.

**Why WARN and not refuse.** ADR-0009 §7 names the thresholds
as "operator alert" surfaces, NOT as control-flow gates. A
refuse-on-threshold would conflate the alert (visibility) and
the cap (control); the cap is at 50, the alerts are at 5/30min.
Conflating them would force the operator to clear the alert
even when they have an operational reason to keep issuing
(e.g., end-of-month bulk issuance while NAV maintenance
window is ongoing).

### 7. Deferred scope — named follow-on findings

Three items NAMED in ADR-0009 §7's parent posture that PR-18
deliberately DEFERS with named-trigger findings:

- **F40 — failed-attempt audit trail (Attempt-before-call
  posture).** ADR-0009 §8 names
  `invoice.submission_attempt` as "Fires before the response
  is received so a crash between POST and response still
  leaves the audit trail intact." The current
  `submit_invoice::run` writes Attempt+Response in one
  post-NAV-success transaction; a failed manage-invoice call
  leaves NO audit trail. PR-18 PRESERVES this behaviour
  (rule 3 — surgical changes) and DEFERS the Attempt-before-
  call posture to a future PR amending the `submit-*` family's
  transaction shape. The drain's loop body inherits the same
  posture: an NAV-side application error on invoice K leaves
  invoice K in `pending` (no audit footprint), and the operator
  can re-drain freely. Named trigger: first PR that introduces
  `queryInvoiceCheck` (ADR-0009 §5 Layer 2 idempotency) and
  therefore needs the Attempt-before-call posture to
  disambiguate "we tried" from "we never tried."

- **F41 — submission-deadline soft/hard gates.** ADR-0009 §7
  names "24h soft / 72h hard limits" + the operator command
  `ConfirmLateSubmission` with its own audit entry. PR-18
  DEFERS: drain prints a WARN at the 30-minute oldest-pending
  threshold (§6 above) but does NOT enforce the 24h or 72h
  limits, and does NOT add a `ConfirmLateSubmission`
  EventKind variant. The deadline is also `[OPEN]` in
  ADR-0009 §7 itself ("Confirm NAV's actual data-reporting
  deadline after issue date; tightening this is cheap,
  loosening it requires spec evidence"). Named trigger: first
  NAV-testbed run that observes a late-submission rejection
  shape, OR first operator request for the
  `--confirm-late-submission` flag.

- **F42 — operator-tunable threshold config.** ADR-0009 §7
  names the alert thresholds as "operator-tunable; defaults
  set here." PR-18 hard-codes the defaults (5 / 30min /
  50-cap). Named trigger: first PR that introduces an
  operator config file (per-tenant settings; no current
  config-file surface exists in ABERP today).

## Consequences

**Positive.**

- The offline-issuance scenario (NAV inspector visit on a
  workstation without NAV reachability) is unblocked end-to-end:
  issue-invoice succeeds, the queue holds the invoice, drain-
  submission-queue processes the backlog when NAV is reachable.
- The bounded cap at 50 surfaces the "operator forgot to drain"
  failure mode LOUD at issue time — silent runaway growth of
  the unsubmitted backlog is impossible per CLAUDE.md rule 12.
- Queue membership is derived from the audit ledger; no second
  source of truth, no schema migration risk, no drift between
  the ledger and a side table (CLAUDE.md rule 7).
- The XML path lives in the audit-payload itself; bundle
  exports already include the payload, so a NAV inspector
  reading the bundle sees the path the operator chose at
  issue time (operationally useful for audit reconstruction).

**Negative.**

- The ledger scan in `count_pending` and
  `PendingInvoiceSet::from_ledger` is O(n) over the tenant's
  full audit chain. Per-tenant volumes (single company, not
  marketplace) put the wall-clock cost in the milliseconds
  today; F39 (bundle-generation cost at hyperscale) is the
  existing named-trigger for an index/cache mitigation when
  the cost matters.
- The optional `nav_xml_path` field on
  `InvoiceDraftCreatedPayload` is backward-compatible at the
  serde layer, but pre-PR-18 entries CANNOT be processed by
  drain without the per-invocation `--xml-path-override` flag.
  The flag is the operator-visible escape hatch, and the
  loud-fail message names it explicitly.
- A drain failure (transport error) leaves the operator-visible
  state ambiguous in one corner case: NAV may have RECEIVED the
  manageInvoice POST but the response was lost. The audit
  ledger has no record (no Attempt entry per F40 above). The
  next drain attempts the invoice again; NAV's Layer-2
  idempotency (`<invoiceNumber>` uniqueness) catches a true
  duplicate IF NAV processed the first attempt — but
  `queryInvoiceCheck` is not yet implemented (also F40-gated).
  The operator's manual recovery is to inspect the NAV web UI;
  the gap is acceptable at PR-18 time because the failure mode
  is rare (transport mid-flight) and the manual-recovery path
  is documented in ADR-0009 §5.

**Locks-in.**

- The audit ledger as the source of truth for queue membership.
  A future shift to a side table is now an ADR-revision (this
  ADR's §1) plus a schema migration; the migration is non-
  trivial because the ledger is hash-chained.
- The `nav_xml_path` field's presence on
  `InvoiceDraftCreatedPayload`. Removing it later would change
  the payload's semantic shape and require a new EventKind
  variant (per audit_payloads.rs's own schema-versioning rule).
- The drain command's name (`drain-submission-queue`) and the
  CLI's stable surface — renaming is a backward-compatibility
  break for any operator scripts.

## Adversarial review

A hostile NAV inspector and a hostile-engineer review, in
alternation.

1. **"The hard cap at 50 is a denial-of-service — an operator
   issuing one invoice at a time hits the cap on day 50 if
   NAV is offline that whole time."** Acknowledged and
   accepted as the intended behaviour per ADR-0009 §7 —
   "Reaching the cap, ABERP refuses to advance new invoices
   from `Draft` to `Ready` and surfaces a loud operator alert
   (per ADR-0007 fail-loud)." The DoS is the FEATURE: silent
   runaway is the failure mode CLAUDE.md rule 12 names. The
   operator's recovery is to drain (if NAV is reachable) or
   `mark-abandoned` invoices the operator has decided not to
   submit. The cap encourages exactly the operator attention
   ADR-0009 §5 designs for (alert-threshold inattention is
   structurally expensive).

2. **"The pure-derived queue scan is O(n). What if the tenant
   has 100,000 entries in the audit ledger?"** Accepted at
   PR-18 size. Per-tenant volumes are bounded by ADR-0002's
   "one tenant is a real company, not a multi-million-invoice
   marketplace" framing. Wall-clock cost of an O(100k) ledger
   walk is dominated by the DuckDB `SELECT_ALL` query, which
   the audit-ledger crate's `entries()` already materialises
   in seq order; the per-row classification cost is a serde
   decode + a string compare. F39's named-trigger covers the
   hyperscale mitigation (an index on `kind` or a
   per-invoice-id materialized view); PR-18 does not pre-
   emptively design either per CLAUDE.md rule 2.

3. **"What if the drain's NAV call ACTUALLY succeeded but the
   audit-write transaction failed (e.g., disk full)?"** The
   audit-write tx is per-invoice and happens AFTER the
   manage-invoice OK response is in hand. A failed audit-write
   leaves the invoice in `pending` (no Response entry) and
   surfaces the disk-full error LOUD per CLAUDE.md rule 12. On
   re-drain, NAV's Layer-2 idempotency (`<invoiceNumber>`
   uniqueness) is the protection: NAV refuses the duplicate
   submission with `INVOICE_NUMBER_NOT_UNIQUE`, which the
   drain treats as a per-invoice application error (does NOT
   short-circuit the loop), surfaces LOUD, and continues. The
   operator's manual recovery: inspect the NAV web UI for the
   submission's actual state, then `aberp mark-abandoned` the
   ABERP-side invoice if NAV recorded it (the legal record
   exists in NAV; the ABERP-side bookkeeping is closed by
   abandonment). This same posture is what ADR-0009 §5 names
   for the existing single-shot `submit-invoice`; the drain
   inherits it.

4. **"The `nav_xml_path` field leaks the operator's local
   filesystem layout into the audit ledger. A bundle shared
   with a NAV inspector reveals `/home/aliceb/work/...` or
   `C:\Users\Alice\Documents\...`."** Accepted as a residual
   concern at PR-18 time. The path is operator-chosen and
   recorded for audit-reconstruction value; a future PR could
   canonicalise to a relative-to-tenant-DB form (e.g.,
   `./xml/inv_01J...xml`) but that imposes a directory
   convention the operator currently doesn't follow. The
   exposure is consistent with the OS-keychain login the
   audit ledger already records (`Actor::from_local_cli`'s
   `user_id` is the keychain login string). Names follow-on
   finding F43 ("operator-PII surface in audit-evidence
   bundles") — same trigger as a future bundle-redaction PR.

5. **"What stops an operator from using a different XML file
   than the one issued? The drain reads from
   `nav_xml_path`, but the file at that path could have been
   hand-edited."** This is the same trap the existing
   `submit_invoice::run` step 3a guards against:
   `aberp_nav_xsd_validator::validate_invoice_data` runs
   BEFORE the NAV call; a hand-edit that breaks the v3.0
   invariant loud-fails before any wire activity. A
   semantically-valid but content-changed XML would still
   submit (the operator could intentionally substitute one
   valid invoice for another) — but the audit ledger
   records the verbatim bytes that hit the wire in the
   `InvoiceSubmissionAttempt` payload (ADR-0009 §8); a NAV
   inspector comparing the issuance's `InvoiceDraftCreated`
   audit payload (which carries the line_count + idempotency
   key) against the Attempt's verbatim XML can detect the
   substitution forensically. The drain does not introduce a
   new substitution surface beyond what `submit-invoice`
   already exposes.

6. **"The drain doesn't write a per-invocation 'I tried'
   audit entry. What if NAV did process invoice K but the
   response was lost, and the operator never realises K is
   in a duplicate state?"** This is the F40 deferral named
   in §7. Until F40 lifts (Attempt-before-call posture),
   the gap stands: a transport-mid-flight loss leaves
   invoice K in `pending`, the next drain re-attempts, NAV's
   Layer-2 idempotency catches a true duplicate as
   `INVOICE_NUMBER_NOT_UNIQUE`, drain reports the per-invoice
   error LOUD, operator inspects NAV web UI, decides
   `mark-abandoned` vs. accepting the prior success. The
   manual-recovery path matches ADR-0009 §5's
   `SubmissionStuck` flow — the operator-attention cost is
   bounded by the same surface ADR-0009 §5 already names.

7. **"What if two drain processes run concurrently against
   the same tenant DB?"** DuckDB's file-lock discipline
   prevents two writers on the same DB file (ADR-0030 §6
   names the per-tenant single-writer posture). The second
   `drain-submission-queue` process loud-fails on
   `Connection::open` with the DuckDB file-lock error. The
   drain inherits this protection without any new code; per
   CLAUDE.md rule 2 we do not pre-emptively design a
   process-level mutex on top of the file-lock.

8. **"The XML path is recorded once at issue time. If the
   operator moves the file later (e.g., re-organises a
   directory), drain fails with a file-not-found error and
   the operator is stuck."** Operator-visible failure with
   a loud message naming `--xml-path-override` as the
   per-invocation escape hatch. The flag accepts
   `<invoice-id>=<new-path>` mappings (repeatable). The
   operator re-runs drain with the override; success on the
   subsequent re-drain. Accepted: the convenience cost is
   real but bounded (per-invoice override on a recovery
   path), and the alternative (rewriting the audit payload
   to track moves) violates ADR-0008's append-only contract.

## Alternatives considered

- **A side `submission_queue` DuckDB table.** Rejected per
  Surfaced conflict 1 above — second source of truth, drift
  risk, ADR-0019 violation.
- **A sibling `<db>.queue.log` JSON-Lines file.** Rejected
  per Surfaced conflict 1 above — same concerns as the side
  table plus conflation with the mirror file's anti-corruption
  purpose.
- **A `--xml-dir <dir>` convention with `<dir>/<invoice_id>.xml`
  filename mapping.** Rejected per Surfaced conflict 2 above
  — imposes filename convention, breaks historical invoices.
- **A new `InvoiceQueuedForSubmissionPayload` EventKind that
  captures verbatim XML bytes.** Rejected per Surfaced
  conflict 2 above — duplicates on-disk artifact, exercises
  F12 ritual for marginal benefit.
- **An explicit `--offline` flag.** Rejected per Surfaced
  conflict 3 above — feature creep, not in ADR-0009 §7's
  framing.
- **Token-exchange amortisation across multiple invoices.**
  Rejected because NAV's v3.0 protocol does not support
  multi-use tokens.
- **Refuse-on-threshold instead of WARN-on-threshold.**
  Rejected per §6 above — conflates alert (visibility) and
  cap (control), forces operator to clear alerts during
  legitimate bulk issuance windows.
- **Drain handles SubmissionStuck invoices too.** Rejected
  per §3 above — the §5 retry surface already covers post-
  submission stuck invoices with operator-confirmed
  `--reason`; folding it into drain would conflate two
  distinct precondition surfaces.

## Open questions

- **Exact NAV submission-deadline.** ADR-0009 §7's
  `[OPEN]` marker remains open (Hungarian dev external
  check). F41 (submission-deadline gates) is gated on this
  + a NAV-testbed late-submission test case.
- **Operator config file for tunable thresholds.** F42's
  trigger.
- **Bundle-redaction posture for `nav_xml_path` PII.** F43's
  trigger.
- **`queryInvoiceCheck` Layer-2 idempotency surface.** F40's
  trigger; until then the transport-mid-flight loss residual
  stands per Adversarial review #6.

## Follow-on ADRs unblocked by this decision

- **F40 closure ADR** — amends the `submit-*` family's
  transaction posture to Attempt-before-call per ADR-0009
  §8's design intent. Likely introduces a new
  `InvoiceSubmissionAttemptFailed` EventKind variant and
  extends `retry-submission`'s precondition to accept
  state-2 (Attempt-failed-no-Response) invoices. F12 ritual
  fires.
- **F41 closure ADR** — adds the 24h soft / 72h hard
  submission-deadline gates + `ConfirmLateSubmission`
  EventKind variant. Gated on NAV-testbed late-submission
  verification.
- **F42 closure ADR** — operator config file surface for
  per-tenant tunables.
