# ADR-0009 — NAV invoice issuing

- **Status:** Accepted
- **Date:** 2026-05-19
- **Amended:** 2026-05-29 (`DONE` status value; forward-tolerant poll parse — see [Amendment 2026-05-29](#amendment-2026-05-29--done-added-to-the-closed-status-vocab-poll-parse-made-forward-tolerant))
- **Deciders:** Ervin
- **Depends on:** ADR-0001 (Rust), ADR-0002 (DB-per-tenant), ADR-0004 (Tauri+Svelte), ADR-0005 (ULID), ADR-0006 (module boundaries), ADR-0007 (security baseline), ADR-0008 (audit ledger), ADR-0019 (storage strategy / no FKs)
- **Source material:** [docs/research/nav-and-billingo.md](../docs/research/nav-and-billingo.md)

## Context

NAV invoice issuing is ABERP's first production surface and the first time
ABERP talks to a regulator. It is also, per the project owner's framing,
the keystone — *"if we are NAV-compliant, the rest is golden."* Every later
adapter (Billingo ingestion, label printing, robotics handoff) inherits the
patterns established here for sequence allocation, idempotency, audit
evidence, and offline behaviour.

The decision must work against:

- **Hungarian fiscal law.** Act CXXVII of 2007 on VAT (Áfa törvény) §169
  binds gap-free sequence numbering per series; the implementing decree
  23/2014 (VI. 30.) NGM details invoicing rules.
- **NAV Online Számla v3.0** as the technical surface.
- **A NAV inspector's audit visit** as the worst-case operational event:
  ABERP must produce a verifiable per-invoice evidence bundle on demand
  and must not have silently dropped or duplicated submissions.

This ADR is the source of truth for issuance. The state machine and the
sequence allocator defined here are reused by ADR-0010 (Billingo +
historical-NAV ingestion), ADR-0012 (label sequence allocation pattern),
and ADR-0013 (robotics task numbering pattern).

### Constraints inherited from other ADRs

- **ADR-0005.** Internal identity is `inv_<ULID>`. The NAV-facing
  sequence number is a separate typed field, never a primary key.
  External identifiers (NAV `transactionId`, customer VAT number) are
  typed fields with their own validation.
- **ADR-0006.** The billing module owns its tables. Other modules learn
  about invoices via events (`InvoiceIssued`, `InvoiceFinalized`,
  `InvoiceStornoIssued`, `InvoiceAmended`, `InvoiceRejected`). No
  cross-module joins.
- **ADR-0007.** Secrets in OS keychain. State transitions audit-logged.
  No PII in logs beyond IDs. No `unsafe` Rust in this module. The
  inherited *intent* — protect credentials, pin TLS roots, treat the
  operator as a threat actor — is honoured in full. The inherited
  *wording* "mTLS to NAV" does not survive contact with the research:
  see "Inherited wording correction" below.
- **ADR-0008.** Every state transition writes a typed audit-ledger
  entry. NAV request/response bodies are stored verbatim *and* parsed.
- **ADR-0019.** No foreign keys. The sequence-allocator reservation row
  and the invoice row reference each other by ULID; integrity is
  enforced in the command handler and by a periodic integrity-scan job.
- **ADR-0017.** The first dense-table UI screen will be the invoice list.
  UI scope itself is deferred to build phase; flagged here only because
  it is the first concrete consumer of the design language.

### Inherited wording correction (surfaced, not silently averaged)

ADR-0007 §Transport originally read "mTLS where the counterparty supports
it (NAV does)." Public NAV documentation, the v3.0 interface specification,
and every consulted open-source NAV client (PHP and Node) describe NAV's
client auth as **application-level over plain HTTPS** — no client X.509
certificate. The keychain content for the NAV adapter is therefore the
**technical-user password + `xmlSignKey` + `xmlChangeKey`**, not an mTLS
certificate. This ADR proceeds on that basis.

The follow-on correction is now filed: see
[ADR-0020 — NAV transport and credential posture correction](0020-nav-transport-credential-correction.md),
which partially supersedes ADR-0007's NAV-specific clauses (§Transport and
the matching threat-model trust-boundary entry) while leaving the rest of
ADR-0007 in force. ADR-0007's general "mTLS where the counterparty
supports it" principle remains in force for other counterparties. The
posture documented in §4 (Credentials) and §6 (Transport) of this ADR
matches ADR-0020 exactly.

## Decision

### 1. API surface and version

- **Target:** NAV Online Számla v3.0. `requestVersion` = `3.0`,
  `headerVersion` = `1.0`. Production endpoint
  `https://api.onlineszamla.nav.gov.hu/invoiceService/v3/`; test endpoint
  `https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3/`.
- **Schema-drift detection.** The NAV v3.0 XSD files are vendored at a
  pinned patch level (initial pin: latest 3.0.x as of build). At
  process start the loaded XSD files are SHA-256-hashed and compared to
  a build-time allow-list. **On mismatch the NAV adapter refuses to
  submit** and surfaces "schema upgrade required" to the operator. This
  is the loud-fail posture — silent acceptance of an unknown schema is
  the failure mode we explicitly refuse.
- **Submission posture.** Direct submission to NAV from day one.
  Billingo is not on the issuance path (Billingo's role is a one-time
  migration source per ADR-0010).
- **Currency.** HUF only for v1. The command boundary rejects any
  currency code other than `HUF`. Multi-currency adds a separate ADR
  with explicit trigger: *first non-HUF customer signed.*
- **Self-billing (önszámlázás).** Out of scope. A future ADR adds the
  `selfBillingIndicator` path and the supplier-side reporting
  responsibility nuance.

### 2. Invoice state machine

Typed Rust enum with illegal transitions refused at compile time
(the new-type-state pattern: each state is its own type; transitions
consume one and produce another).

```
Draft ──┐
        ├─► Ready ── (queue) ─► Submitted ─► AckPending ─┬─► Finalized
        │                          │                     │
        │                          │                     ├─► Rejected ◄─ (ABORTED)
        │                          │                     │
        │                          └─► SubmissionStuck   └─► (loop, polling)
        │
        └─► Voided  (reservation released; sequence-slot recorded with reason)

Finalized ─┬─► Amended  (a MODIFY chain invoice has been issued)
           └─► Storno   (a STORNO chain invoice has been issued)
```

- `Draft` — created in ABERP; not yet validated for submission.
- `Ready` — passed local XSD validation; **sequence number reserved**
  in the same transaction.
- `Submitted` — `manageInvoice` accepted by NAV; `transactionId` recorded.
- `AckPending` — polling `queryTransactionStatus`. State PROCESSING.
- `Finalized` — NAV terminal-positive (`SAVED`); invoice is legally
  issued and reported.
- `Rejected` — NAV terminal-negative (`ABORTED`). Original invoice
  number is **not reused** (sequence is gap-free); a corrective new
  invoice must be issued. The rejected sequence slot is recorded as
  used-with-reason in the reservation table (see §3).
- `SubmissionStuck` — bounded retries exhausted on transient errors.
  Operator-action-required; no automatic state advance.
- `Amended` — a MODIFY chain invoice references this one. Side path.
- `Storno` — a STORNO chain invoice cancels this one. Side path.

Every transition writes a typed audit-ledger entry per ADR-0008. The
typed kinds: `invoice.draft_created`, `invoice.sequence_reserved`,
`invoice.ready`, `invoice.submitted`, `invoice.ack_pending`,
`invoice.ack_received`, `invoice.finalized`, `invoice.rejected`,
`invoice.submission_stuck`, `invoice.voided`, `invoice.amended`,
`invoice.storno_issued`, `invoice.technical_annulment_requested`.

### 3. Sequence-number allocator

This is the hardest part of the ADR. It must produce gap-free,
crash-safe, replay-safe sequence numbers under all failure modes,
without foreign keys, and work the same on DuckDB (today) and Postgres-
per-tenant (later) — ADR-0019.

**Data model (per tenant DB):**

```
table invoice_series
  id            ULID    primary key   -- internal id
  code          TEXT    unique         -- human-facing series name, e.g. "INV-2026"
  reset_policy  ENUM   { Never, AnnualOnFiscalYear }
  fiscal_year   INTEGER nullable       -- present iff reset_policy = AnnualOnFiscalYear
  created_at    TIMESTAMP

table invoice_sequence_state
  series_id     ULID    primary key    -- references invoice_series.id by ULID, no FK
  fiscal_year   INTEGER                -- 0 for Never, actual year for AnnualOnFiscalYear
  next_number   BIGINT  NOT NULL CHECK (next_number >= 1)
  updated_at    TIMESTAMP

table invoice_sequence_reservation
  id            ULID    primary key
  series_id     ULID    NOT NULL       -- ULID reference, no FK
  fiscal_year   INTEGER NOT NULL
  number        BIGINT  NOT NULL
  invoice_id    ULID    NOT NULL       -- ULID reference, no FK
  status        ENUM   { Reserved, Used, Voided }
  void_reason   TEXT    nullable
  reserved_at   TIMESTAMP
  used_at       TIMESTAMP nullable
  voided_at     TIMESTAMP nullable
  UNIQUE (series_id, fiscal_year, number)
```

**Allocate (atomic):** in **one** database transaction, the
`IssueInvoiceCommand` handler does, in order:

1. `SELECT next_number FROM invoice_sequence_state WHERE series_id = ?
   AND fiscal_year = ? FOR UPDATE`. (DuckDB single-writer makes
   `FOR UPDATE` a no-op today; on Postgres it serializes.)
2. If `reset_policy = AnnualOnFiscalYear` and the invoice's *issue date*
   year != the state row's fiscal_year, **roll the year**: insert a new
   `invoice_sequence_state` row for the new year with `next_number = 1`,
   and audit-log `invoice.sequence_reset`. Issue date is server-clock-
   only (ADR-0007) so the year is deterministic.
3. Compute `allocated = next_number`; `UPDATE invoice_sequence_state
   SET next_number = next_number + 1`.
4. `INSERT INTO invoice_sequence_reservation (id, series_id, fiscal_year,
   number, invoice_id, status='Reserved', reserved_at=now)`.
5. `INSERT INTO invoice` (the actual invoice row, with `sequence_number
   = allocated` and `sequence_series_id = ?`).
6. Append audit-ledger entries (`invoice.sequence_reserved`,
   `invoice.draft_created`) in the same transaction.
7. Commit.

**Why this is gap-free under crash:** every operation that mutates
`next_number` lives inside the same transaction as the invoice row and
the reservation row. If any step fails or the process crashes before
commit, the entire transaction rolls back — `next_number` is unchanged,
no reservation exists, no invoice row exists. There is no window in
which a number is "burned without an invoice."

**Why this is replay-safe under client retry:** the
`IssueInvoiceCommand` carries a **client-side idempotency key** = the
ULID of the command itself (ADR-0005). The command handler first looks
up "does this idempotency key already have an invoice?" via the audit
ledger's `idempotency_key` field (ADR-0008). If yes, return that
invoice (no second allocation). If no, allocate fresh.

**Void path:** if an invoice in `Ready` state must be voided before
submission (operator cancels, business logic refuses), the reservation
status flips `Reserved → Voided` with `void_reason`. The number is
not re-allocated. The sequence remains gap-free in the legal sense —
the number was *used*, and the audit ledger documents *why* it was
voided. **[OPEN, accountant]** Whether Hungarian practice requires the
void to be replaced with a placeholder corrective invoice. Decision
deferred to first accountant review; the data model supports either.

**Startup reconciliation:** on process start the module runs a scan:
for every reservation with `status = Reserved`, verify the corresponding
invoice row exists and is in `Draft` or `Ready` state. Mismatches
(reservation without invoice, or invoice without reservation) are
written to the audit ledger as `invoice.reconciliation_anomaly` and
surfaced to the operator. Loud failure (ADR-0007), never silent.

**Series + reset-policy decision per series:** Hungarian rule permits
annual reset *optionally* per series. The default ABERP series
`INV-default` ships with `reset_policy = Never`. Operators may create
additional series with `AnnualOnFiscalYear` if their accountant wants
yearly resets. The decision is per-series, not project-wide.

### 4. Authentication and credentials

NAV's auth is application-level over HTTPS (no client X.509):

- Per-request `<user>` block:
  - `login` — technical user login name
  - `passwordHash` = SHA-512(plaintext password), `cryptoType="SHA-512"`
  - `taxNumber` = 8-digit base of the taxpayer's tax number
  - `requestSignature` = SHA3-512(...), `cryptoType="SHA3-512"`

- `requestSignature` input:
  - Non-`manageInvoice` ops:
    `requestId || requestTimestamp(UTC, YYYYMMDDhhmmss) || xmlSignKey`
  - `manageInvoice` / `manageAnnulment`: same input, plus per
    invoice-index a SHA3-512 of `operation || base64(invoiceData)`,
    concatenated in index order.

- Submission flow per invoice:
  1. `tokenExchange` — receive `<encodedExchangeToken>`; decrypt with
     **AES-128/ECB** using `xmlChangeKey`. Token valid ~5 minutes
     (**[OPEN]** confirm in spec).
  2. `manageInvoice` (operation = `CREATE` | `MODIFY` | `STORNO`) with
     the decrypted token and the per-invoice signature. NAV returns a
     `transactionId`.
  3. Poll `queryTransactionStatus` until terminal (`SAVED` or `ABORTED`).

**Credential storage (per ADR-0007 intent):** the OS keychain holds, per
tenant:

- `nav.technical_user.login`
- `nav.technical_user.password` (plaintext — hashed per request)
- `nav.xml_sign_key`
- `nav.xml_change_key`

Secrets are read at process start, held in `Zeroizing<String>`
(`zeroize` crate), and never logged. Rotation is operator-initiated
(regenerate in the NAV web UI, re-import into keychain via a guided
flow). Auth failures (`INVALID_SECURITY_USER`, `INVALID_REQUEST_SIGNATURE`)
are **not retried** — they are not transient. The invoice transitions to
`SubmissionStuck` and the operator is alerted.

**TLS posture:** standard HTTPS to `api.onlineszamla.nav.gov.hu` with the
NAV server certificate's issuing root **pinned** in ABERP's trust store.
This is the strongest one-directional posture available given NAV's
contract (per the user's framing: "security toward NAV is one-directional
because we adapt to a legacy regulator").

### 5. Idempotency on NAV submission

Two layers, applied in order on every submit:

- **Layer 1 — client-side idempotency key.** The
  `IssueInvoiceCommand` ULID. Persisted in the audit ledger
  (`idempotency_key` field per ADR-0008). On retry of the same command,
  the handler returns the prior result without re-invoking NAV.

- **Layer 2 — NAV-side reconciliation by invoice number.** If the
  process crashed between `manageInvoice` returning and the
  `transactionId` being persisted (no Layer-1 record yet), the retry
  path **first calls `queryInvoiceCheck`** against the invoice number.
  If NAV already has it, fetch the chain via `queryInvoiceData` and
  reconstruct local state. If NAV does not have it, submit fresh.

**Retry policy:**

- Retryable errors: HTTP 504, `OPERATION_FAILED`, connection reset,
  DNS failure.
- Non-retryable errors (each transitions invoice to `SubmissionStuck`
  with audit entry): `INVALID_REQUEST_SIGNATURE`,
  `INVALID_SECURITY_USER`, `INCORRECT_REQUEST_SCHEMA`,
  `SCHEMA_VIOLATION`, `INVOICE_NUMBER_NOT_UNIQUE` (handled via Layer
  2 disambiguation first).
- Max attempts: **5**, exponential backoff (1s, 2s, 4s, 8s, 16s).
- After max attempts the invoice is `SubmissionStuck` — no further
  automatic action. Operator unblocks via a typed `RetrySubmission`
  or `MarkSubmissionAbandoned` command, each with its own audit entry.

### 6. Storno and modification chain

- A **storno** is itself an invoice. It gets its own ULID and consumes
  the next sequence slot in the appropriate series. Submitted via
  `manageInvoice` with `operation = STORNO`. The chain link is the base
  invoice's invoice number in `<invoiceReference>` plus a
  `<modificationIndex>` (starts at 1 per base invoice; increments).
- A **modification (módosítás)** is structurally identical but
  `operation = MODIFY`. Same chain-link mechanics.
- A **technical annulment** is **not** a storno. It withdraws a faulty
  data submission only. Endpoint: `manageAnnulment`. Operator-action-
  required path; requires the receiver to confirm in the NAV web UI.
  Used only for true submission-side errors (e.g., a test invoice
  reached production). Distinct command type: `RequestTechnicalAnnulment`.
- **Migrated-from-Billingo invoices** that need a later amendment are
  handled by first calling `queryInvoiceChainDigest` to determine the
  next valid `modificationIndex` for the base invoice (which was
  originally issued in Billingo and reported by Billingo to NAV).

Sequence numbers are **never reused**. The chain link in the audit
ledger is always explicit and ULID-keyed (no cross-table FK per
ADR-0019).

### 7. Offline submission queue

NAV unavailability cannot block invoice **issuance** — a NAV inspector
visit scenario requires invoices to be issuable with NAV unreachable.
ABERP therefore queues `Ready` invoices and submits when NAV is
reachable again.

- **Queue: bounded.** Hard cap **50 unsubmitted invoices**. Reaching
  the cap, ABERP refuses to advance new invoices from `Draft` to
  `Ready` and surfaces a loud operator alert (per ADR-0007 fail-loud).
  Issuance does not silently succeed under back-pressure.
- **Operator alert thresholds** (operator-tunable; defaults set here):
  - Either **5** invoices in queue, **or** **30 minutes** since the
    oldest unsubmitted, whichever trips first.
- **Submission-deadline soft limit:** ABERP aims to submit within
  **24 hours** of invoice issue date and refuses to submit anything
  older than **72 hours** without explicit operator confirmation
  (the operator command `ConfirmLateSubmission` carries its own audit
  entry). **[OPEN]** Confirm NAV's actual data-reporting deadline
  after issue date; tightening this is cheap, loosening it requires
  spec evidence.
- The submission worker runs on a single tenant connection, processes
  the queue in FIFO order by issue date, and writes
  `invoice.submission_attempt` audit entries for each try.

### 8. Audit-evidence retention and export

Every NAV interaction produces ledger entries (ADR-0008):

- `invoice.submission_attempt` — request body (XML) verbatim in the
  payload (`payload.request_xml`) **and** parsed (`payload.request_parsed`).
  Signature hashes recorded but the keys themselves are **never**
  serialized into the ledger.
- `invoice.submission_response` — response body verbatim and parsed,
  plus the `transactionId`.
- `invoice.ack_status` — `queryTransactionStatus` response verbatim and
  parsed, per poll.

**Per-invoice export bundle** (operator-callable, used during a NAV
audit visit) contains, for the requested invoice ULID:

- Every audit-ledger entry for that invoice, in order.
- The verbatim request and response XML for every NAV interaction.
- The verbatim `queryTransactionStatus` responses across the chain.
- Every attestation checkpoint (ADR-0008) covering the entries.
- The binary hash of the ABERP build at submission time.
- The schema hash that validated the XML at submission time.
- A signature over the whole bundle, verifiable by anyone holding the
  attestation public key.

Bundle output: a single signed `.tar.zst` file written to a path the
operator picks. **Generated, not stored** — re-generated on demand,
which keeps the canonical state in the ledger and avoids divergence.

### 9. Certification posture

Target: NAV conformance achieved **before first real customer goes
live**. Concretely, before any production tenant is provisioned:

- A **conformance test plan** lives at
  `docs/conformance/nav-test-plan.md` (to be authored in the
  conformance phase). Coverage: every operation, every state
  transition, every retryable and non-retryable error path, both
  reset-policy series modes, storno and modification chains, technical
  annulment.
- Conformance is exercised against the NAV test environment
  (`api-test.onlineszamla.nav.gov.hu`) using a self-provisioned test
  taxpayer.
- Evidence captured: every submitted XML, every response, every audit
  ledger entry. Replayable.
- **No formal accreditation is required today** (May 2026). The ViDA-
  driven mandatory accreditation regime is in consultation; ABERP
  re-visits this when accreditation rules are published with a firm
  date.

## Consequences

**Positive**

- Gap-free sequence numbering is a structural property, not a runtime
  invariant we have to defend. The transaction boundary does the work.
- Replay-safe by construction: the same client-side idempotency key
  produces the same outcome regardless of how many times it arrives.
- A NAV audit-visit evidence bundle is one operator action away, and
  it is verifiable by the auditor without trusting ABERP at runtime.
- The state machine is enforced at compile time. Most classes of
  "submitted-but-not-acked-yet" bug become impossible to express.
- The credential model (technical-user password + two keys in keychain)
  is the smallest set that NAV actually accepts; we are not pretending
  to a stronger posture than NAV supports.

**Negative**

- The sequence allocator pattern is more code than `SELECT MAX(num) + 1`.
  Accepted: that pattern is unsafe under any concurrency or crash.
- The void / placeholder treatment for unused reservations is unresolved
  pending accountant confirmation. The data model supports both legal
  modes; we accept the wait.
- The submission deadline (24h soft / 72h hard) is provisional pending
  external verification. Tightening is cheap; loosening requires
  evidence.
- ADR-0007's "mTLS to NAV" wording correction is now filed as ADR-0020
  (partial supersede). No longer an open item for this ADR.

**Locked in**

- Once a tenant has issued an invoice under a series's reset policy,
  the policy is not changeable for that series. Operators create a new
  series instead.
- The technical-user credentials are NAV-side artifacts the taxpayer
  controls. ABERP cannot rotate them autonomously; rotation is a
  guided operator flow.

## Adversarial review

A hostile NAV inspector and a hostile-engineer review, in alternation.

1. **"The sequence allocator's single-writer claim under DuckDB is a
   single point of failure."** Yes — under DuckDB the single-writer
   serializes all allocations across all series in the tenant.
   Throughput is bounded but the regulator-facing invariant (gap-free)
   is preserved. When the cloud Postgres adapter lands (ADR-0016), the
   `FOR UPDATE` per state row gives per-series concurrency without
   reintroducing the gap-free risk. Accepted: this ADR is sized to
   per-tenant volumes, not to a hyperscale issuance throughput. The
   trade-off is consistent with the project's owner framing — one
   tenant is a real company, not a multi-million-invoice marketplace.

2. **"What if the audit-ledger entry is appended in the same transaction
   as the allocator but the ledger's mirror-file fsync fails?"** The
   transaction commits the ledger row alongside the allocator state and
   the invoice row inside the tenant DB. The mirror-file write per
   ADR-0008 happens post-commit on a separate path. A mirror-file fsync
   failure is detected by the mirror-writer (loud alert) and the canonical
   state in the DB remains correct. The mirror is recoverable from the
   DB; the DB is not recoverable from the mirror. We accept that the
   mirror is best-effort secondary evidence, not primary.

3. **"Idempotency Layer 2 assumes `queryInvoiceCheck` is cheap and
   instantaneous. What if NAV is itself slow on the check?"** The check
   is short and synchronous. If it times out we treat the prior
   submission as suspect and the invoice goes to `SubmissionStuck`
   pending operator action — no automatic retry, no automatic resubmit.
   Operator decides. This is intentionally pessimistic — the regulator
   cost of a duplicate submission is higher than the operator cost of
   one manual unblock.

4. **"`SubmissionStuck` will accumulate. Operators will start ignoring
   it."** Two countermeasures. (a) `SubmissionStuck` is loud per
   ADR-0007: persistent operator alert, dashboard counter, and an
   audit-ledger entry every time a stuck invoice is observed by the
   poller. (b) The submission deadline (72h hard) means a stuck invoice
   gets harder to ignore as it ages. Beyond 72h, advancing it requires
   the explicit `ConfirmLateSubmission` command — a deliberate decision,
   audit-logged. We accept that there is no purely-mechanical fix for
   operator inattention; we make inattention expensive.

5. **"What stops the operator from issuing a back-dated invoice via the
   issue-date field?"** The invoice issue date is server-clock-only
   (ADR-0007 §Operator-as-threat-actor controls). The operator cannot
   set it; the system assigns it at `Draft → Ready` transition and
   audit-logs the wall+monotonic clock pair at that moment. The same
   issue date is what the allocator uses for year-roll decisions, so
   any future attempt to shift it would be visible in the ledger.

6. **"The void path puts business judgment in the gap-free invariant.
   A bad accountant call here breaks compliance."** Acknowledged.
   The data model supports both treatments (void marker vs corrective
   placeholder) and the open question is flagged loud. The legal
   resolution is an accountant question, not an engineering question.
   We refuse to implement a default that might be wrong; the operator
   chooses per series, and the audit ledger documents the choice.

7. **"You depend on NAV honouring uniqueness on `invoiceNumber` for
   Layer-2 idempotency. If NAV ever accepts a duplicate, ABERP
   double-reports."** Correct, and we mitigate by making Layer 1 the
   primary defence (the audit-ledger idempotency lookup on the
   client). Layer 2 is the disaster-recovery path for a process crash
   between submit and ack. A NAV-side dedup bug is outside our trust
   boundary; we would detect it via the per-day reconciliation scan
   (next adversarial point).

8. **"How would ABERP detect that NAV silently dropped a submission?"**
   A nightly reconciliation job compares `queryInvoiceDigest` (NAV's
   record of submissions for our tax number, the prior day) against
   ABERP's local audit-ledger record of submissions for the same
   window. Any divergence is an `invoice.reconciliation_anomaly` ledger
   entry and a loud operator alert. Detection time bounded by a day,
   which is well inside the 8-year audit retention requirement.

## Alternatives considered

- **Per-row `SELECT MAX(sequence_number) + 1` allocation.** Rejected —
  unsafe under any concurrency, and the audit-ledger reconciliation
  can't recover lost numbers because it has no record that they were
  intended.
- **Reservation via dedicated sequence service** (e.g., a Redis-backed
  counter). Rejected — introduces a network dependency on the issuance
  path, complicates the local-first deployment posture, and replaces a
  database guarantee with an external coordinator that itself needs a
  crash-safe story.
- **Submit synchronously and refuse issuance when NAV is down.**
  Rejected — NAV inspector visits explicitly require ABERP to issue with
  NAV unreachable. The legal date is the issue date, not the submission
  date.
- **Use NAV's `transactionId` as the client idempotency key.** Rejected —
  the `transactionId` is assigned *by NAV after the first call
  succeeds*, which is too late to use for the crash-between-submit-and-
  ack scenario.
- **Optimistic submission: skip the queue, submit inline on
  `Ready`.** Rejected — couples the issuance UX to NAV's latency and
  removes the offline-queue invariant. The queue is at most a few
  seconds latency on the happy path.
- **Use Billingo's submission path during early period to defer the
  direct-NAV work.** Rejected per the project's explicit framing:
  Billingo is migration-only, ABERP owns the issuance path.

## Amendment 2026-05-29 — `DONE` added to the closed status vocab; poll parse made forward-tolerant

The production NAV test endpoint was observed (2026-05-28) returning
`<invoiceStatus>DONE</invoiceStatus>` for terminally-processed invoices.
The original §2 closed vocabulary (`RECEIVED`, `PROCESSING`, `SAVED`,
`ABORTED`) is incomplete against current NAV behaviour, and the strict
parser in `crates/nav-transport/src/operations/query_transaction_status.rs`
(`ProcessingStatus::from_nav_str`) was rejecting the entire response as a
non-retryable parse error. The poll loop (`apps/aberp/src/poll_ack.rs`)
classified that as `StuckNonRetryable`, so the SPA pictogram stayed on
⌛ Submitted forever with no operator recourse.

Decision:

1. **`DONE` is terminal-success, semantically identical to `SAVED`.**
   `ProcessingStatus::from_nav_str("DONE")` now parses to
   `ProcessingStatus::Saved` rather than erroring. Collapsing at this single
   parse boundary means the entire downstream pipeline is unchanged: the
   poll-ack handler writes the same `InvoiceAckStatus` audit entry it writes
   for `SAVED` (`ack_status = "SAVED"`), and `serve::derive_state`'s existing
   `Some("SAVED") => Finalized` rule flips the pictogram to ✓ Final. The
   verbatim `DONE` bytes are still preserved in the audit `response_xml`, so
   no audit fidelity is lost. (A distinct `Done` enum variant was considered
   and rejected per CLAUDE.md rules 2/3/13 — it would be behaviourally
   identical to `Saved` everywhere and would force matching edits across the
   SPA `AckStatus` wire mirror, `parse_ack_status`, and `derive_state`.)

2. **The poll read is forward-tolerant.** Any *other* unrecognized
   `<invoiceStatus>` value no longer fatals the read. `from_nav_str` stays
   strict (fail-loud, returns `Err`), but the read boundary
   (`parse_processing_status_forward_tolerant`, used by `call`) logs the raw
   value at WARN and maps it to a new `ProcessingStatus::Unknown` variant.
   `Unknown` is non-terminal: the loop keeps polling and, at attempt
   exhaustion, surfaces `StuckIntermediate("UNKNOWN")` — actionable, not a
   silent terminal. Future NAV additions therefore never strand an invoice.

   `Unknown` is a unit variant (the enum keeps `#[derive(Copy)]`, which is
   relied on pervasively); the raw NAV string is preserved via the WARN log
   and the verbatim `response_xml` rather than carried in the variant.

The closed vocab is still a real closed vocab on the WRITE side — ABERP
never *emits* `Unknown`; it only arises when *reading back* an external NAV
response. Forward-tolerance applies in one direction only: external reads,
never internal writes.

## Open questions

The full list is consolidated in
[docs/research/nav-and-billingo.md](../docs/research/nav-and-billingo.md);
the items below specifically block work in this module:

- ~~**ADR-0007 wording correction (mTLS).**~~ Resolved by ADR-0020
  (partial supersede, 2026-05-19). NAV §Transport posture now
  authoritatively documented in ADR-0020.
- **Void treatment for unused reservations** (accountant question).
  Until resolved, the data model supports both modes and the default
  per series is void-marker. First accountant review chooses.
- **Exact NAV submission deadline after issue date** (Hungarian dev
  external check). ABERP's provisional 24h soft / 72h hard limits will
  tighten or loosen accordingly.
- **Storno-of-a-storno practice** (accountant question). API-permitted;
  Hungarian accounting convention may prefer a fresh corrective. Affects
  operator command vocabulary, not the data model.
- **NAV response signing** (Hungarian dev external check). If NAV signs
  response bodies, the verbatim-store path also verifies the signature
  and refuses on mismatch.

## Follow-on ADRs unblocked by this decision

- **ADR-0010 — Billingo + historical NAV ingestion (read path).** Reuses
  the audit-ledger NAV-evidence shape and the `queryInvoiceChainDigest`
  pattern for migrated invoices.
- **Stack-baseline ADR** (deferred per `adr/README.md`). Required
  before any Rust file lands. The NAV adapter's needs — async HTTP
  client with TLS pinning, XML schema validation, retry primitive —
  inform that ADR concretely.
- **Wire-protocol ADR** (deferred per `adr/README.md`). The
  invoice-list UI per ADR-0017 is the first concrete consumer.
- ~~**ADR-0007 amendment / superseder** for the mTLS wording correction.~~ Filed as ADR-0020 (2026-05-19).
