//! [`EventKind`] — typed event kinds per ADR-0008 §"Entry shape".
//!
//! `kind` is the type discriminant for `payload`'s schema. Schema versioning
//! is implicit in the kind name: bumping a payload schema renames the kind,
//! and the old kind remains valid for historical entries.
//!
//! No serde derive: PR-3 stores the kind as a plain text column in DuckDB
//! via [`EventKind::as_str`]. Serde will join when a serialization path
//! (export bundle, wire protocol) actually needs it.

/// PR-3 shipped only `Test`. PR-5 added the first two invoice-lifecycle
/// kinds from ADR-0009 §2 (`InvoiceSequenceReserved`, `InvoiceDraftCreated`).
/// PR-7-B-3 adds the three NAV-submission evidence kinds from ADR-0009 §8
/// (`InvoiceSubmissionAttempt`, `InvoiceSubmissionResponse`,
/// `InvoiceAckStatus`). The first two of those three fire in PR-7-B-3's
/// `submit-invoice` flow; `InvoiceAckStatus` is added now (rather than
/// in PR-7-C) so the three-coordinated-edit trap (PR-6.1 F12 — variant +
/// `as_str` + `from_storage_str` + the test-list array) is closed for the
/// whole NAV submission path in one PR.
///
/// PR-8 adds two operator-unblock kinds from ADR-0009 §5
/// (`InvoiceRetryRequested`, `InvoiceMarkedAbandoned`). Each marks an
/// **operator-initiated** event distinct from the per-attempt NAV
/// evidence kinds: `InvoiceRetryRequested` records the operator's
/// decision to re-submit a stuck invoice (the retry itself then
/// produces normal `InvoiceSubmissionAttempt` / `InvoiceSubmissionResponse`
/// entries via the existing submit pipeline); `InvoiceMarkedAbandoned`
/// records the operator's decision to stop retrying. Both adds
/// re-exercise the F12 four-coordinated-edit trap — variant +
/// `as_str` + `from_storage_str` + the `round_trip_for_every_variant`
/// hand-listed array. This is the first PR since PR-6.1 to add a new
/// variant; the trap is performing its job by definition only if all
/// four edits land in the same commit.
///
/// PR-10 (ADR-0023) graduates the long-anticipated `InvoiceStornoIssued`
/// from doc-comment hint to actual variant. A storno is itself an
/// invoice (ADR-0009 §6); its sequence-reservation + draft-creation
/// audit entries reuse `InvoiceSequenceReserved` / `InvoiceDraftCreated`
/// unchanged. `InvoiceStornoIssued` is the **chain-link** entry: it
/// carries the base invoice's id + sequence number + the new storno's
/// own ids + the `modificationIndex` allocated in the same DuckDB
/// transaction (per ADR-0023 §4). The base invoice's typestate
/// transition (`Finalized → Storno` per ADR-0009 §2) is DERIVED from
/// the existence of this entry — no second ledger entry is written
/// against the base (ADR-0023 §2).
///
/// PR-11 (ADR-0024) adds `InvoiceModificationIssued` — the MODIFY
/// chain-link entry parallel to `InvoiceStornoIssued`. Same structural
/// shape: a modification is itself an invoice with its own
/// `InvoiceSequenceReserved` + `InvoiceDraftCreated` entries plus a
/// chain-link entry that carries the base's id + the modification's
/// own ids + the `modificationIndex` (allocated in the same DuckDB
/// transaction by a walk that now considers BOTH `InvoiceStornoIssued`
/// AND `InvoiceModificationIssued` entries against the same base —
/// ADR-0024 §7). The base's derived typestate transition (`Finalized →
/// Amended` per ADR-0009 §2) is observed by the existence of this
/// entry; the same "no second source of truth" posture as STORNO.
///
/// PR-12 (ADR-0025) adds `InvoiceTechnicalAnnulmentRequested` — the
/// third and final ADR-0009 §6 surface. Structurally **different**
/// from STORNO + MODIFY: a technical annulment is NOT itself an
/// invoice (no sequence-slot burn, no `InvoiceSequenceReserved` /
/// `InvoiceDraftCreated` pair). The annulment is a NAV-side
/// data-submission withdrawal whose canonical record is the
/// `InvoiceTechnicalAnnulmentRequested` entry alone — a single
/// operator-decision audit entry, NOT a chain link. The base
/// invoice's derived typestate is NOT transitioned by an annulment
/// request (ADR-0025 §2) — annulment is data-submission withdrawal,
/// not legal cancellation; the base's `Finalized` / `Rejected` /
/// `Stuck` / `Abandoned` state is unchanged. NAV-side fulfillment
/// (receiver confirms in the NAV web UI) is asynchronous and observed
/// by a future polling PR.
///
/// PR-13 (ADR-0026) adds `InvoiceAnnulmentSubmissionAttempt` +
/// `InvoiceAnnulmentSubmissionResponse` — the **wire half** of the
/// technical-annulment surface. Structural parallel to PR-7-B-3's
/// `InvoiceSubmissionAttempt` + `InvoiceSubmissionResponse` (same
/// verbatim-bytes-before-parse posture per ADR-0009 §8) but
/// deliberately forked at the discriminator level per ADR-0026 §2
/// + ADR-0026 §"Surfaced conflict 1". Rationale: kind-alone
/// classification in the audit-evidence bundle (ADR-0009 §8) —
/// a NAV inspector reading the per-invoice trail sees "ABERP
/// requested technical annulment → ABERP submitted the annulment
/// to NAV → NAV responded with TXID-Q" as a sequence of distinct
/// kinds, not as "submit, submit" requiring payload XML
/// inspection to disambiguate from a fresh invoice submission.
/// The F12 four-edit ritual re-fires twice (once per variant) and
/// closes the seventh and eighth times across PR-6.1 / PR-7-B-3 /
/// PR-8 / PR-10 / PR-11 / PR-12 / PR-13.
///
/// PR-14 (ADR-0027) adds `InvoiceAnnulmentAckStatus` — the
/// **wire-poll half** of the technical-annulment surface, paired
/// with PR-13's wire-submission entries. Closes the ADR-0009 §6
/// observation gap at the wire level (the receiver-confirmation
/// observation remains a separate future surface per ADR-0027
/// §"Surfaced conflict 3"). Structural parallel to PR-7-B-3's
/// `InvoiceAckStatus` (same `queryTransactionStatus` wire
/// endpoint per ADR-0027 §3 + §"Surfaced conflict 1") but
/// deliberately forked at the discriminator level per ADR-0027
/// §2. Rationale identical to ADR-0026 §2: kind-alone
/// classification at the audit-evidence-bundle level is the
/// load-bearing inspector-facing property.
///
/// PR-15 (ADR-0028) adds `InvoiceAnnulmentReceiverConfirmation`
/// — the **receiver-confirmation observation half** of the
/// technical-annulment surface, closing the final ADR-0009 §6
/// observation gap at the audit-evidence level (the
/// semantic-interpretation layer — parsing a `receiver_state`
/// field within the response bytes — is deferred per ADR-0028
/// §"Surfaced conflict 3" until NAV-testbed verification
/// surfaces its shape). Structurally parallel to PR-14's
/// `InvoiceAnnulmentAckStatus` (same audit-evidence shape:
/// verbatim NAV response bytes + the F8 lineage idempotency-
/// key chain) but deliberately forked at the discriminator
/// level per ADR-0028 §2. The two observation surfaces are
/// operationally distinct facts: PR-14 observes NAV-side wire
/// processing (seconds-paced); PR-15 observes NAV-side
/// receiver-confirmation (human-paced). The audit ledger
/// keeps them distinguishable by kind. The F12 four-edit
/// ritual fires once — the ninth landing across PR-6.1 /
/// PR-7-B-3 / PR-8 / PR-10 / PR-11 / PR-12 / PR-13 / PR-14 /
/// PR-15, mechanical at this point.
///
/// PR-19 (ADR-0032) adds `InvoiceSubmissionAttemptFailed` — the
/// failure half of the Attempt/Response audit pair per ADR-0009
/// §8's "Fires before the response is received" design intent.
/// Closes F40 at the issuing-path level. The new variant pairs
/// with the existing `InvoiceSubmissionAttempt` (PR-7-B-3) under
/// the two-tx posture ADR-0032 §1 names: TX1 commits the Attempt
/// before the NAV `manageInvoice` POST; TX2 commits either
/// `InvoiceSubmissionResponse` (success) or
/// `InvoiceSubmissionAttemptFailed` (failure). Failure classes
/// (transport / http_status / application / retryable_application
/// / envelope / credential / client_build) are carried as a
/// typed string field on the payload per ADR-0032 §2 + §"Surfaced
/// conflict 2 Reading B" — kind-alone classification would multiply
/// the F12 ritual surface for sub-types of "the wire call failed."
/// The state-2 Pending precondition (Attempt-without-Response per
/// ADR-0032 §4) is operator-recoverable via the existing
/// `retry-submission` command — no new operator command.
/// The F12 four-edit ritual fires once — the tenth landing
/// across PR-6.1 / PR-7-B-3 / PR-8 / PR-10 / PR-11 / PR-12 /
/// PR-13 / PR-14 / PR-15 / PR-19, mechanical at this point.
///
/// PR-20 (ADR-0033) adds `InvoiceCheckPerformed` — the Layer-2
/// NAV-side existence-check evidence per ADR-0009 §5's named
/// disambiguation surface. Closes F44 at the state-2 Pending
/// disambiguation level. The new variant captures the outcome
/// of a `queryInvoiceCheck` call performed by `retry-submission`
/// BEFORE the manageInvoice re-POST, so the operator-visible
/// retry path no longer carries the duplicate-submission residual
/// PR-19's adversarial review #2 named-warned. Outcomes
/// (`"exists"` / `"absent"` / `"failure"`) are carried as a typed
/// string field on the payload per ADR-0033 §2 + §"Surfaced
/// conflict 2 Reading B" — kind-alone classification would
/// multiply the F12 ritual surface for sub-types of "ABERP asked
/// NAV whether it has invoice X." The post-positive-check chain-
/// reconstruction surface (fetching the chain via queryInvoiceData
/// per ADR-0009 §5's full intent) is named-deferred as F48.
/// The F12 four-edit ritual fires once — the eleventh landing
/// across PR-6.1 / PR-7-B-3 / PR-8 / PR-10 / PR-11 / PR-12 /
/// PR-13 / PR-14 / PR-15 / PR-19 / PR-20, mechanical at this
/// point.
///
/// The remaining invoice-lifecycle kinds (`Finalized`, `Rejected`,
/// `SubmissionStuck`, `Voided`) land when their state transition
/// first fires in the codebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// Test-only kind used by `tests/chain_conformance.rs`. Not allowed in
    /// production code; a future conformance check should gate this.
    Test,

    /// A sequence number was reserved in `invoice_sequence_reservation`
    /// as part of the atomic allocator (ADR-0009 §3).
    InvoiceSequenceReserved,

    /// An invoice row was inserted with state `Draft` (ADR-0009 §2).
    /// In PR-5 this fires together with `InvoiceSequenceReserved`
    /// because the binary's command path goes Draft -> Ready in one
    /// allocator call. A future PR may split them.
    InvoiceDraftCreated,

    /// A `manageInvoice` request was POSTed to NAV. Payload carries the
    /// verbatim request XML (ADR-0009 §8). Fires before the response is
    /// received so a crash between POST and response still leaves the
    /// audit trail intact. PR-7-B-3.
    InvoiceSubmissionAttempt,

    /// A `manageInvoice` response was received from NAV with the
    /// `transactionId`. Payload carries the verbatim response XML and
    /// the parsed `transaction_id`. Fires AFTER `InvoiceSubmissionAttempt`
    /// in the same `submit-invoice` flow. PR-7-B-3.
    InvoiceSubmissionResponse,

    /// A `queryTransactionStatus` poll completed. Payload carries the
    /// verbatim response XML and the parsed ack status
    /// (`RECEIVED` / `PROCESSING` / `SAVED` / `ABORTED`). PR-7-C will
    /// emit this; the variant is declared in PR-7-B-3 to close the
    /// three-coordinated-edit trap in one go.
    InvoiceAckStatus,

    /// The operator initiated a re-submission of an invoice that is in
    /// the `SubmissionStuck` precondition per ADR-0009 §5. Payload
    /// carries the prior `transaction_id`, the prior last ack status
    /// (the audit precondition justification), and the operator's
    /// reason text. The retry itself then fires the normal
    /// `InvoiceSubmissionAttempt` + `InvoiceSubmissionResponse` pair
    /// via the existing submit pipeline; this kind records the
    /// **operator's decision** distinctly so the audit-evidence
    /// bundle (ADR-0009 §8) makes the unblock explicit. PR-8.
    InvoiceRetryRequested,

    /// The operator marked a stuck invoice abandoned per ADR-0009 §5.
    /// Terminal in the audit ledger — no further automatic state
    /// advance is permitted for this invoice. Payload carries the
    /// prior `transaction_id`, the prior last ack status, and the
    /// operator's reason text. PR-8.
    InvoiceMarkedAbandoned,

    /// A storno invoice was issued against a base invoice
    /// (ADR-0009 §6, ADR-0023). The storno is itself an invoice and
    /// got its own `InvoiceSequenceReserved` + `InvoiceDraftCreated`
    /// entries in the same DuckDB transaction; THIS entry is the
    /// chain-link payload (ADR-0023 §3) — it carries the base
    /// invoice's id, the base's NAV-facing sequence number, the new
    /// storno's own id + sequence + reservation id + idempotency
    /// key, and the allocated `modificationIndex`.
    ///
    /// The base invoice's typestate transition (`Finalized → Storno`)
    /// is **derived** from the existence of this entry pointing at
    /// the base; no separate ledger entry is written against the
    /// base (ADR-0023 §2). PR-10.
    InvoiceStornoIssued,

    /// A modification (MODIFY) invoice was issued against a base
    /// invoice (ADR-0009 §6, ADR-0024). Same structural shape as
    /// `InvoiceStornoIssued`: the modification is itself an invoice
    /// and got its own `InvoiceSequenceReserved` + `InvoiceDraftCreated`
    /// entries in the same DuckDB transaction; THIS entry is the
    /// chain-link payload (ADR-0024 §5) — it carries the base
    /// invoice's id, the base's NAV-facing sequence number, the new
    /// modification's own id + sequence + reservation id + idempotency
    /// key, the allocated `modificationIndex` (allocated by a walk
    /// that considers BOTH this kind AND `InvoiceStornoIssued` against
    /// the same base — ADR-0024 §7), and the operator-supplied
    /// `<modificationIssueDate>` (NAV-required for MODIFY but not for
    /// STORNO; distinguishes the two operations on the wire — ADR-0024
    /// §3).
    ///
    /// The base invoice's typestate transition (`Finalized → Amended`)
    /// is **derived** from the existence of this entry pointing at
    /// the base; no separate ledger entry is written against the
    /// base (ADR-0024 §2). PR-11.
    InvoiceModificationIssued,

    /// The operator requested a NAV-side technical annulment of a
    /// prior data submission against an invoice (ADR-0009 §6,
    /// ADR-0025). Technical annulment is **distinct** from STORNO
    /// and MODIFY: it withdraws a NAV-side data submission (e.g.,
    /// a test invoice accidentally sent to production) WITHOUT
    /// legally cancelling the invoice as a document.
    ///
    /// Structural contrasts with `InvoiceStornoIssued` /
    /// `InvoiceModificationIssued`:
    ///
    ///   - **Not a chain entry.** No `<invoiceReference>` block,
    ///     no `modificationIndex`, no chain-allocator walk
    ///     (ADR-0025 §7).
    ///   - **No sequence-slot burn.** The annulment is not itself
    ///     an invoice; no `InvoiceSequenceReserved` /
    ///     `InvoiceDraftCreated` pair is written. The annulment's
    ///     audit footprint is THIS entry alone.
    ///   - **No derived typestate transition.** The base invoice's
    ///     state (`Finalized` / `Rejected` / `Stuck` / `Abandoned`)
    ///     is unchanged by the annulment *request* alone; NAV-side
    ///     fulfillment (receiver confirms in NAV's web UI) is
    ///     asynchronous and not yet observed in code (future PR).
    ///
    /// Payload carries the base `invoice_id`, the operator-decision
    /// idempotency key, the base's prior `transactionId` (the
    /// thing being withdrawn), the NAV annulment code
    /// (`ERRATIC_DATA` / `ERRATIC_INVOICE_NUMBER` /
    /// `ERRATIC_INVOICE_ISSUE_DATE` /
    /// `ERRATIC_ELECTRONIC_HASH_VALUE`), and the operator's
    /// free-form reason text. PR-12.
    InvoiceTechnicalAnnulmentRequested,

    /// A `manageAnnulment` request was POSTed to NAV — the wire
    /// half of the technical-annulment surface (ADR-0009 §6,
    /// ADR-0026). Payload carries the verbatim
    /// `<ManageAnnulmentRequest>` envelope bytes (ADR-0009 §8 —
    /// captured BEFORE the response is parsed so a crash mid-flight
    /// still leaves the audit trail pointing at "we tried to
    /// withdraw data submission X with body Y"), the base
    /// `invoice_id`, the annulment-request's `idempotency_key`
    /// (F8 — flows from the prior
    /// `InvoiceTechnicalAnnulmentRequested` entry per ADR-0026 §6),
    /// and the `endpoint` label (`"test"` or `"production"`).
    ///
    /// Structurally parallel to `InvoiceSubmissionAttempt` but
    /// **deliberately forked at the discriminator** so the audit-
    /// evidence bundle reader can distinguish a manageInvoice
    /// submission from a manageAnnulment submission by kind alone
    /// (ADR-0026 §2 + ADR-0026 §"Surfaced conflict 1"). PR-13.
    InvoiceAnnulmentSubmissionAttempt,

    /// A `manageAnnulment` response was received from NAV with a
    /// `transactionId`. Payload carries the verbatim
    /// `<ManageAnnulmentResponse>` bytes (ADR-0009 §8) plus the
    /// parsed `transaction_id` (NAV's annulment-side tracking id),
    /// the base `invoice_id`, and the annulment-request's
    /// `idempotency_key`. Fires AFTER
    /// `InvoiceAnnulmentSubmissionAttempt` in the same
    /// `submit-annulment` flow.
    ///
    /// Same structural-parallel-with-fork posture as
    /// `InvoiceAnnulmentSubmissionAttempt`. PR-13, ADR-0026 §2.
    InvoiceAnnulmentSubmissionResponse,

    /// A `queryTransactionStatus` poll completed against an
    /// annulment-side `transactionId` (ADR-0009 §6, ADR-0027).
    /// Payload carries the verbatim
    /// `<QueryTransactionStatusResponse>` bytes (ADR-0009 §8) plus
    /// the parsed ack status (`RECEIVED` / `PROCESSING` /
    /// `SAVED` / `ABORTED` per NAV v3.0 — same enumeration as
    /// `InvoiceAckStatus`), the base `invoice_id`, and the
    /// annulment-side `transaction_id` (looked up from the prior
    /// `InvoiceAnnulmentSubmissionResponse` entry per ADR-0027
    /// §4).
    ///
    /// Structural parallel to PR-7-B-3's `InvoiceAckStatus` but
    /// **deliberately forked at the discriminator** so the
    /// audit-evidence bundle reader can distinguish an
    /// invoice-side poll from an annulment-side poll by kind alone
    /// (ADR-0027 §2). The wire endpoint is REUSED
    /// (`queryTransactionStatus`) per ADR-0027 §3 + §"Surfaced
    /// conflict 1"; the discriminator fork at the audit level is
    /// independent of the wire-endpoint reuse.
    ///
    /// On terminal `SAVED`, the operator-visible message names the
    /// receiver-confirmation gap loud per ADR-0027 §5 + CLAUDE.md
    /// rule 12 — NAV's SAVED for an annulment submission means
    /// "NAV accepted the annulment for processing," NOT "the
    /// receiver has confirmed the annulment in the NAV web UI";
    /// the receiver-confirmation observation is a separate future
    /// surface per ADR-0027 §"Surfaced conflict 3". PR-14,
    /// ADR-0027 §2.
    InvoiceAnnulmentAckStatus,

    /// A `queryInvoiceData` call against the BASE invoice's NAV-
    /// facing invoice number completed (ADR-0009 §6, ADR-0028).
    /// Closes the final ADR-0009 §6 observation gap at the audit-
    /// evidence level — the operator can now drive the full
    /// technical-annulment lifecycle AND observe NAV-side
    /// receiver-confirmation evidence.
    ///
    /// Payload carries the verbatim `<QueryInvoiceDataResponse>`
    /// bytes (ADR-0009 §8 — the audit evidence cannot be lost to
    /// a parser bug), the base `invoice_id`, the NAV-facing
    /// `nav_invoice_number` (the string that was queried —
    /// recorded so the bundle reader can see what was queried
    /// without re-deriving from `series.code + seq`), the
    /// annulment-side `annulment_transaction_id` (from the prior
    /// `InvoiceAnnulmentSubmissionResponse` — pinned so the
    /// reader walks back to the annulment lineage by ID without
    /// re-walking), and the annulment-request's `idempotency_key`
    /// (F8 carry-forward per ADR-0028 §7 — same posture as the
    /// PR-14 ack-status entries; closes the per-annulment audit
    /// lineage end-to-end).
    ///
    /// **No `receiver_state` field on this payload today** per
    /// ADR-0028 §"Surfaced conflict 3". The semantic-interpretation
    /// layer (parsing a NAV-emitted receiver-confirmed marker
    /// within the response bytes) lands in a future amendment ADR
    /// after NAV-testbed verification surfaces the actual response
    /// shape. PR-15's contract is verbatim-bytes-as-evidence; the
    /// operator-visible message names the verbatim bytes in the
    /// audit ledger as the source of truth and explicitly does
    /// NOT claim a parsed receiver-confirmation state per
    /// CLAUDE.md rule 12. PR-15, ADR-0028 §2.
    InvoiceAnnulmentReceiverConfirmation,

    /// A `manageInvoice` submission attempt failed — the failure
    /// half of the `InvoiceSubmissionAttempt` / response pair per
    /// ADR-0009 §8's "Fires before the response is received"
    /// design intent and ADR-0032's two-tx posture (§1). Written
    /// in TX2 of the submission flow when the NAV call returns
    /// an error (transport-layer, HTTP-status, application-layer,
    /// envelope-construction, credential, or client-build failure)
    /// instead of `InvoiceSubmissionResponse`.
    ///
    /// Payload (`InvoiceSubmissionAttemptFailedPayload` in the
    /// binary's `audit_payloads.rs`) carries the `invoice_id`,
    /// the F8 `idempotency_key` carry-forward, the `endpoint`
    /// label (`"test"` / `"production"`), a typed `error_class`
    /// string (one of `"transport"`, `"http_status"`,
    /// `"application"`, `"retryable_application"`, `"envelope"`,
    /// `"credential"`, `"client_build"` per ADR-0032 §2), an
    /// optional `error_code` (NAV code or HTTP status as string),
    /// the operator-visible `error_message`, and the verbatim
    /// response bytes IF a response body was received before the
    /// error fired (None for transport / envelope / credential
    /// / client-build classes).
    ///
    /// An invoice with `InvoiceSubmissionAttempt` + this kind +
    /// no `InvoiceSubmissionResponse` classifies as state-2
    /// Pending per ADR-0032 §4 — operator-recoverable via the
    /// existing `retry-submission` command (which writes a fresh
    /// Attempt-Response pair).
    ///
    /// The `invoice.` prefix MUST hold so the per-invoice export
    /// bundle's (ADR-0009 §8) `invoice.*` glob picks up the new
    /// entries alongside every other lifecycle entry — same
    /// silent-omission-failure-mode posture every prior PR's
    /// prefix-pin test names. PR-19, ADR-0032 §2.
    InvoiceSubmissionAttemptFailed,

    /// A Layer-2 `queryInvoiceCheck` against the invoice's
    /// NAV-facing invoice number completed (ADR-0009 §5,
    /// ADR-0033 §1). Written by `retry-submission`'s state-2
    /// Pending branch BEFORE the manageInvoice re-POST, so the
    /// retry can disambiguate "NAV already has this submission"
    /// from "the wire broke before NAV saw it" and skip the
    /// re-POST in the former case (no duplicate submission to
    /// NAV).
    ///
    /// Payload (`InvoiceCheckPerformedPayload` in the binary's
    /// `audit_payloads.rs`) carries the `invoice_id`, the F8
    /// `idempotency_key` carry-forward, the `endpoint` label
    /// (`"test"` / `"production"`), the
    /// `nav_invoice_number` that was queried, a typed `outcome`
    /// string (one of `"exists"` / `"absent"` / `"failure"` per
    /// ADR-0033 §2), the verbatim
    /// `<QueryInvoiceCheckRequest>` bytes, the verbatim NAV
    /// response bytes (Option — Some when a body was received),
    /// and three optional `failure_*` fields populated iff
    /// `outcome == "failure"` (matching the seven-class
    /// enumeration `InvoiceSubmissionAttemptFailedPayload.error_class`
    /// uses).
    ///
    /// An `InvoiceCheckPerformed` entry is **informational
    /// only** in the sense that `audit_query::stuck_precondition`
    /// does NOT consult it — the precondition walker continues
    /// to classify by `InvoiceSubmissionAttempt` /
    /// `InvoiceSubmissionResponse` / `InvoiceMarkedAbandoned`
    /// presence per ADR-0032 §4. Per ADR-0033 §6, the state-2
    /// → not-stuck transition (when NAV has the invoice but
    /// ABERP did not record the prior Response) is the
    /// F48-deferred recover-from-nav surface; until F48 lands,
    /// `InvoiceCheckPerformed(outcome=exists)` entries
    /// accumulate as audit evidence that the operator skipped
    /// re-POST despite the local state-2 Pending classification.
    ///
    /// The `invoice.` prefix MUST hold so the per-invoice export
    /// bundle's (ADR-0009 §8) `invoice.*` glob picks up the new
    /// entries alongside every other lifecycle entry — same
    /// silent-omission-failure-mode posture every prior PR's
    /// prefix-pin test names. PR-20, ADR-0033 §2.
    InvoiceCheckPerformed,
}

impl EventKind {
    /// Render in the on-disk form. Paired with [`EventKind::from_storage_str`]
    /// as a round-trip-proven pair (unit tests in this module check that
    /// for every variant `V`, `from_storage_str(V.as_str()) == Ok(V)`).
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Test => "test",
            EventKind::InvoiceSequenceReserved => "invoice.sequence_reserved",
            EventKind::InvoiceDraftCreated => "invoice.draft_created",
            EventKind::InvoiceSubmissionAttempt => "invoice.submission_attempt",
            EventKind::InvoiceSubmissionResponse => "invoice.submission_response",
            EventKind::InvoiceAckStatus => "invoice.ack_status",
            EventKind::InvoiceRetryRequested => "invoice.retry_requested",
            EventKind::InvoiceMarkedAbandoned => "invoice.marked_abandoned",
            EventKind::InvoiceStornoIssued => "invoice.storno_issued",
            EventKind::InvoiceModificationIssued => "invoice.modification_issued",
            EventKind::InvoiceTechnicalAnnulmentRequested => {
                "invoice.technical_annulment_requested"
            }
            EventKind::InvoiceAnnulmentSubmissionAttempt => {
                "invoice.annulment_submission_attempt"
            }
            EventKind::InvoiceAnnulmentSubmissionResponse => {
                "invoice.annulment_submission_response"
            }
            EventKind::InvoiceAnnulmentAckStatus => "invoice.annulment_ack_status",
            EventKind::InvoiceAnnulmentReceiverConfirmation => {
                "invoice.annulment_receiver_confirmation"
            }
            EventKind::InvoiceSubmissionAttemptFailed => "invoice.submission_attempt_failed",
            EventKind::InvoiceCheckPerformed => "invoice.check_performed",
        }
    }

    /// Parse the on-disk form back into an `EventKind`. Errors on
    /// unknown strings — silent fallback would mask schema drift per
    /// CLAUDE.md rule 12 ("fail loud").
    ///
    /// Adding a new `EventKind` variant requires three coordinated
    /// edits: the variant itself, an arm in [`EventKind::as_str`],
    /// and an arm here. The round-trip unit test below will fail
    /// loudly if `as_str` and `from_storage_str` ever drift apart
    /// for an existing variant. Adding a variant without updating
    /// this function is a compile error only if the new variant's
    /// `as_str` arm is also added — caller is on the hook for both;
    /// PR-6.1 surfaced this trap (Fortnightly review F12).
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "test" => Ok(EventKind::Test),
            "invoice.sequence_reserved" => Ok(EventKind::InvoiceSequenceReserved),
            "invoice.draft_created" => Ok(EventKind::InvoiceDraftCreated),
            "invoice.submission_attempt" => Ok(EventKind::InvoiceSubmissionAttempt),
            "invoice.submission_response" => Ok(EventKind::InvoiceSubmissionResponse),
            "invoice.ack_status" => Ok(EventKind::InvoiceAckStatus),
            "invoice.retry_requested" => Ok(EventKind::InvoiceRetryRequested),
            "invoice.marked_abandoned" => Ok(EventKind::InvoiceMarkedAbandoned),
            "invoice.storno_issued" => Ok(EventKind::InvoiceStornoIssued),
            "invoice.modification_issued" => Ok(EventKind::InvoiceModificationIssued),
            "invoice.technical_annulment_requested" => {
                Ok(EventKind::InvoiceTechnicalAnnulmentRequested)
            }
            "invoice.annulment_submission_attempt" => {
                Ok(EventKind::InvoiceAnnulmentSubmissionAttempt)
            }
            "invoice.annulment_submission_response" => {
                Ok(EventKind::InvoiceAnnulmentSubmissionResponse)
            }
            "invoice.annulment_ack_status" => Ok(EventKind::InvoiceAnnulmentAckStatus),
            "invoice.annulment_receiver_confirmation" => {
                Ok(EventKind::InvoiceAnnulmentReceiverConfirmation)
            }
            "invoice.submission_attempt_failed" => Ok(EventKind::InvoiceSubmissionAttemptFailed),
            "invoice.check_performed" => Ok(EventKind::InvoiceCheckPerformed),
            _ => Err("unknown EventKind storage string"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip every known variant. If a future contributor adds a
    /// variant + `as_str` arm but forgets the `from_storage_str` arm,
    /// this test fails for that variant — the maintenance trap F12
    /// named is now caught at test time, not at runtime against a
    /// production row.
    #[test]
    fn round_trip_for_every_variant() {
        // Hand-listed so a future variant addition makes the maintainer
        // *think* about whether they updated this list. `strum`-style
        // auto-iteration would silently exclude a new variant if the
        // contributor forgot to add a derive — exactly the trap.
        let variants = [
            EventKind::Test,
            EventKind::InvoiceSequenceReserved,
            EventKind::InvoiceDraftCreated,
            EventKind::InvoiceSubmissionAttempt,
            EventKind::InvoiceSubmissionResponse,
            EventKind::InvoiceAckStatus,
            EventKind::InvoiceRetryRequested,
            EventKind::InvoiceMarkedAbandoned,
            EventKind::InvoiceStornoIssued,
            EventKind::InvoiceModificationIssued,
            EventKind::InvoiceTechnicalAnnulmentRequested,
            EventKind::InvoiceAnnulmentSubmissionAttempt,
            EventKind::InvoiceAnnulmentSubmissionResponse,
            EventKind::InvoiceAnnulmentAckStatus,
            EventKind::InvoiceAnnulmentReceiverConfirmation,
            EventKind::InvoiceSubmissionAttemptFailed,
            EventKind::InvoiceCheckPerformed,
        ];
        for v in variants {
            let s = v.as_str();
            let parsed = EventKind::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?} -> {e}"));
            assert_eq!(parsed, v, "round-trip mismatch for {s:?}");
        }
    }

    #[test]
    fn from_storage_str_rejects_unknown() {
        assert!(EventKind::from_storage_str("invoice.future_kind").is_err());
        assert!(EventKind::from_storage_str("").is_err());
    }

    /// PR-7-B-3 specifically: the three new on-disk strings must
    /// match the dot-separated convention so existing tooling that
    /// filters by prefix (`invoice.*`) catches them. If a future
    /// contributor renames one without the `invoice.` prefix, this
    /// assertion fires.
    #[test]
    fn pr_7_b_3_kinds_use_invoice_prefix() {
        assert!(EventKind::InvoiceSubmissionAttempt
            .as_str()
            .starts_with("invoice."));
        assert!(EventKind::InvoiceSubmissionResponse
            .as_str()
            .starts_with("invoice."));
        assert!(EventKind::InvoiceAckStatus.as_str().starts_with("invoice."));
    }

    /// PR-8 specifically: the two operator-unblock kinds must also use
    /// the `invoice.` prefix so the audit-evidence bundle (ADR-0009 §8)
    /// can be filtered with the same prefix glob as the NAV-evidence
    /// kinds. Same loud-fail rationale as `pr_7_b_3_kinds_use_invoice_prefix`.
    #[test]
    fn pr_8_operator_unblock_kinds_use_invoice_prefix() {
        assert!(EventKind::InvoiceRetryRequested
            .as_str()
            .starts_with("invoice."));
        assert!(EventKind::InvoiceMarkedAbandoned
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-10 specifically: `InvoiceStornoIssued` is the chain-link
    /// kind for ADR-0009 §6 / ADR-0023. The on-disk string must keep
    /// the `invoice.` prefix so the audit-evidence bundle's
    /// `invoice.*` glob picks it up alongside every other invoice-
    /// lifecycle entry — a storno that did not match the glob would
    /// be silently absent from the per-invoice export bundle, which
    /// is the exact failure mode CLAUDE.md rule 12 names.
    #[test]
    fn pr_10_storno_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceStornoIssued.as_str(),
            "invoice.storno_issued"
        );
        assert!(EventKind::InvoiceStornoIssued
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-11 specifically: `InvoiceModificationIssued` is the MODIFY
    /// chain-link kind for ADR-0009 §6 / ADR-0024 — same posture as
    /// PR-10's storno-kind prefix test. The MODIFY entry MUST share
    /// the `invoice.` prefix so the per-invoice export bundle picks
    /// up both STORNO and MODIFY chain entries with one glob; a
    /// MODIFY entry under a different prefix would split the chain
    /// across two glob patterns and produce the silent-omission
    /// failure mode CLAUDE.md rule 12 names.
    #[test]
    fn pr_11_modification_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceModificationIssued.as_str(),
            "invoice.modification_issued"
        );
        assert!(EventKind::InvoiceModificationIssued
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-12 specifically: `InvoiceTechnicalAnnulmentRequested` is
    /// the third and final ADR-0009 §6 surface (ADR-0025). The
    /// `invoice.` prefix MUST hold for the same reason PR-10 and
    /// PR-11 pin it — the per-invoice export bundle (ADR-0009 §8)
    /// `invoice.*` glob must pick up technical-annulment entries
    /// alongside storno + modification + every other invoice-
    /// lifecycle entry. An annulment under a different prefix would
    /// be silently absent from the per-invoice export bundle —
    /// exactly the silent-omission failure mode CLAUDE.md rule 12
    /// names.
    #[test]
    fn pr_12_technical_annulment_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceTechnicalAnnulmentRequested.as_str(),
            "invoice.technical_annulment_requested"
        );
        assert!(EventKind::InvoiceTechnicalAnnulmentRequested
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-13 / ADR-0026 §2: the wire-evidence attempt for the
    /// annulment surface. The `invoice.` prefix MUST hold for the
    /// same per-invoice-export-bundle reason PR-10 / PR-11 / PR-12
    /// pin it — the audit-evidence bundle's `invoice.*` glob
    /// (ADR-0009 §8) must pick up annulment-wire entries alongside
    /// every other lifecycle entry. An entry under a different
    /// prefix would be silently absent from the per-invoice export
    /// bundle — exactly the silent-omission failure mode CLAUDE.md
    /// rule 12 names.
    #[test]
    fn pr_13_annulment_submission_attempt_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceAnnulmentSubmissionAttempt.as_str(),
            "invoice.annulment_submission_attempt"
        );
        assert!(EventKind::InvoiceAnnulmentSubmissionAttempt
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-13 / ADR-0026 §2: the wire-evidence response. Same
    /// `invoice.` prefix pin as the attempt above; the two land
    /// in this PR as a pair per the structural-parallel-with-fork
    /// posture (ADR-0026 §2).
    #[test]
    fn pr_13_annulment_submission_response_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceAnnulmentSubmissionResponse.as_str(),
            "invoice.annulment_submission_response"
        );
        assert!(EventKind::InvoiceAnnulmentSubmissionResponse
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-13 / ADR-0026 §2: deliberate fork from the manageInvoice
    /// kinds. The two new wire-evidence kinds MUST have distinct
    /// storage strings from `InvoiceSubmissionAttempt` /
    /// `InvoiceSubmissionResponse` so the audit-evidence bundle
    /// reader's kind-alone classification works. Pinning this here
    /// catches a future refactor accidentally collapsing the four
    /// kinds onto two on-disk strings.
    #[test]
    fn pr_13_annulment_kinds_are_distinct_from_invoice_kinds() {
        assert_ne!(
            EventKind::InvoiceAnnulmentSubmissionAttempt.as_str(),
            EventKind::InvoiceSubmissionAttempt.as_str()
        );
        assert_ne!(
            EventKind::InvoiceAnnulmentSubmissionResponse.as_str(),
            EventKind::InvoiceSubmissionResponse.as_str()
        );
    }

    /// PR-14 / ADR-0027 §2: the wire-poll ack-status kind for the
    /// annulment surface. The `invoice.` prefix MUST hold for the
    /// same per-invoice-export-bundle reason every prior PR pins
    /// it — the audit-evidence bundle's `invoice.*` glob
    /// (ADR-0009 §8) must pick up the annulment-poll entries
    /// alongside every other lifecycle entry. An entry under a
    /// different prefix would be silently absent from the per-
    /// invoice export bundle — exactly the silent-omission
    /// failure mode CLAUDE.md rule 12 names.
    #[test]
    fn pr_14_annulment_ack_status_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceAnnulmentAckStatus.as_str(),
            "invoice.annulment_ack_status"
        );
        assert!(EventKind::InvoiceAnnulmentAckStatus
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-14 / ADR-0027 §2: deliberate fork from the invoice-side
    /// `InvoiceAckStatus`. The wire endpoint
    /// (`queryTransactionStatus`) is REUSED across the two flows
    /// per ADR-0027 §3 + §"Surfaced conflict 1", but the audit-
    /// ledger discriminator MUST be distinct so the audit-evidence
    /// bundle reader can classify by kind alone (ADR-0027 §2).
    /// Pinning this here catches a future refactor accidentally
    /// collapsing the two poll-ack kinds onto one on-disk string.
    #[test]
    fn pr_14_annulment_ack_status_is_distinct_from_invoice_ack_status() {
        assert_ne!(
            EventKind::InvoiceAnnulmentAckStatus.as_str(),
            EventKind::InvoiceAckStatus.as_str()
        );
    }

    /// PR-15 / ADR-0028 §2: the receiver-confirmation observation
    /// kind for the annulment surface. The `invoice.` prefix MUST
    /// hold for the same per-invoice-export-bundle reason every
    /// prior PR pins it — the audit-evidence bundle's `invoice.*`
    /// glob (ADR-0009 §8) must pick up the new entries alongside
    /// every other lifecycle entry. An entry under a different
    /// prefix would be silently absent from the per-invoice
    /// export bundle — exactly the silent-omission failure mode
    /// CLAUDE.md rule 12 names.
    #[test]
    fn pr_15_annulment_receiver_confirmation_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceAnnulmentReceiverConfirmation.as_str(),
            "invoice.annulment_receiver_confirmation"
        );
        assert!(EventKind::InvoiceAnnulmentReceiverConfirmation
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-15 / ADR-0028 §2: deliberate fork from the wire-side
    /// `InvoiceAnnulmentAckStatus`. The two observation surfaces
    /// (wire-side ack-poll vs receiver-confirmation observation)
    /// are operationally distinct facts per ADR-0028 §2 —
    /// pinning the discriminator-level distinction here catches
    /// a future refactor accidentally collapsing the two
    /// observation kinds onto one on-disk string. Same posture
    /// `pr_13_annulment_kinds_are_distinct_from_invoice_kinds` /
    /// `pr_14_annulment_ack_status_is_distinct_from_invoice_ack_status`
    /// use for their respective fork-discipline pins.
    #[test]
    fn pr_15_receiver_confirmation_is_distinct_from_annulment_ack_status() {
        assert_ne!(
            EventKind::InvoiceAnnulmentReceiverConfirmation.as_str(),
            EventKind::InvoiceAnnulmentAckStatus.as_str()
        );
    }

    /// PR-19 / ADR-0032 §2: the failure half of the Attempt /
    /// Response audit pair. The `invoice.` prefix MUST hold so
    /// the per-invoice export bundle's (ADR-0009 §8) `invoice.*`
    /// glob picks up the new entries alongside every other
    /// lifecycle entry — same silent-omission-failure-mode
    /// posture every prior PR's prefix-pin test names.
    #[test]
    fn pr_19_submission_attempt_failed_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceSubmissionAttemptFailed.as_str(),
            "invoice.submission_attempt_failed"
        );
        assert!(EventKind::InvoiceSubmissionAttemptFailed
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-19 / ADR-0032 §2: deliberate fork from the success-side
    /// `InvoiceSubmissionResponse`. The two outcomes of a
    /// submission attempt (NAV-acknowledged-with-transactionId vs
    /// NAV-rejected-or-wire-broken) are operationally distinct
    /// facts per ADR-0032 §2 + §"Surfaced conflict 2" — pinning
    /// the discriminator-level distinction here catches a future
    /// refactor accidentally collapsing the two outcomes onto one
    /// on-disk string. Same posture
    /// `pr_13_annulment_kinds_are_distinct_from_invoice_kinds` /
    /// `pr_15_receiver_confirmation_is_distinct_from_annulment_ack_status`
    /// use for their respective fork-discipline pins.
    #[test]
    fn pr_19_attempt_failed_is_distinct_from_submission_response() {
        assert_ne!(
            EventKind::InvoiceSubmissionAttemptFailed.as_str(),
            EventKind::InvoiceSubmissionResponse.as_str()
        );
        assert_ne!(
            EventKind::InvoiceSubmissionAttemptFailed.as_str(),
            EventKind::InvoiceSubmissionAttempt.as_str()
        );
    }

    /// PR-20 / ADR-0033 §2: the Layer-2 queryInvoiceCheck
    /// evidence kind. The `invoice.` prefix MUST hold so the
    /// per-invoice export bundle's (ADR-0009 §8) `invoice.*`
    /// glob picks up the new entries alongside every other
    /// lifecycle entry — same silent-omission-failure-mode
    /// posture every prior PR's prefix-pin test names.
    #[test]
    fn pr_20_check_performed_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceCheckPerformed.as_str(),
            "invoice.check_performed"
        );
        assert!(EventKind::InvoiceCheckPerformed
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-20 / ADR-0033 §2: deliberate fork from the
    /// submission-side kinds. The Layer-2 existence-check
    /// outcome is a NAV-side query event, structurally
    /// distinct from a manageInvoice submission attempt /
    /// response / failure. Pinning the discriminator-level
    /// distinction here catches a future refactor accidentally
    /// collapsing the Layer-2 evidence kind onto one of the
    /// existing submission kinds. Same posture
    /// `pr_19_attempt_failed_is_distinct_from_submission_response`
    /// + `pr_13_annulment_kinds_are_distinct_from_invoice_kinds`
    /// use for their respective fork-discipline pins.
    #[test]
    fn pr_20_check_performed_is_distinct_from_submission_kinds() {
        assert_ne!(
            EventKind::InvoiceCheckPerformed.as_str(),
            EventKind::InvoiceSubmissionAttempt.as_str()
        );
        assert_ne!(
            EventKind::InvoiceCheckPerformed.as_str(),
            EventKind::InvoiceSubmissionResponse.as_str()
        );
        assert_ne!(
            EventKind::InvoiceCheckPerformed.as_str(),
            EventKind::InvoiceSubmissionAttemptFailed.as_str()
        );
    }
}
