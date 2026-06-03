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
/// PR-92 (ADR-0047) adds `InvoiceEmailedSent` — the operational
/// "invoice emailed to buyer" event. Operator-twin: there must be a
/// durable record of WHEN the buyer was sent the invoice, TO WHICH
/// address, and the OUTCOME (succeeded / failed). Critical because
/// the SMTP layer is the buyer-communication path — silence-by-
/// omission ("we don't know if the buyer got it") is the wrong
/// default for a buyer-comms product per [[aberp-notes-and-email]].
///
/// Payload (`InvoiceEmailedSentPayload` in the binary's
/// `audit_payloads.rs`) carries: `invoice_id`, `idempotency_key`,
/// `recipient` (the to-address — visible operator data, not a
/// secret), `subject` (verbatim email subject sent), `outcome`
/// (closed-vocab `"succeeded"` / `"failed"`), `error_class`
/// (`None` on succeeded; closed-vocab `"transport"` / `"tls"` /
/// `"auth"` / `"recipient_rejected"` / `"compose"` / `"other"` on
/// failed), `error_detail` (operator-readable explanation —
/// scrubbed of credentials by the SMTP send path), `auto` (bool —
/// `true` when the post-issue auto-send fired, `false` when the
/// operator clicked the manual "Email to buyer" button), and
/// `attached_xml` (bool — did the NAV XML ride along).
///
/// CRITICALLY: the payload MUST NOT carry the SMTP password, the
/// SMTP server host (defence-in-depth — host is in seller.toml and
/// has its own audit trail elsewhere; including it in every email
/// audit entry would smear server identity across the ledger), or
/// the email body bytes. ADR-0047 §4 pins the secret-scrubbing
/// posture; the unit pin in `tests/audit_payload_emailed_no_secrets`
/// catches any future field addition that violates it.
///
/// The `invoice.` prefix MUST hold so the per-invoice export bundle's
/// (ADR-0009 §8) `invoice.*` glob picks up the new entries alongside
/// every other lifecycle entry — same silent-omission-failure-mode
/// posture every prior PR's prefix-pin test names. PR-92.
///
/// PR-70 (ADR-0039) adds `InvoicePaymentRecorded` — the operational
/// "quick mark as paid" event per the Tier-2-lifted-to-Tier-1
/// roadmap decision at session 81 (`project_aberp_ux_roadmap.md`).
/// Structurally distinct from every prior invoice-lifecycle kind:
/// it does NOT touch `derive_state` (paid-vs-unpaid is operational
/// metadata, not a NAV regulatory typestate transition; the ladder
/// remains `Draft / Ready / Pending / Submitted / Finalized /
/// Stornoed / Modified / Abandoned / ...` per ADR-0036). The
/// payment record is queried separately via
/// `audit_query::payment_record_for` and rendered alongside the
/// state chip as a parallel "Paid" badge.
///
/// Payload (`InvoicePaymentRecordedPayload` in the binary's
/// `audit_payloads.rs`) carries the `invoice_id`, the
/// operator-decision `idempotency_key`, the operator-supplied
/// `paid_at` date (canonical `YYYY-MM-DD`), the `amount_minor`
/// in the invoice's stored minor-unit form (i64), the `currency`
/// (must match the invoice's currency — enforced at the route
/// boundary), the `method` (closed-vocab: `BankTransfer` / `Cash`
/// / `Card` / `Other`), and an optional `reference` (free-form
/// operator note: bank transaction id, cheque number, etc.).
///
/// One entry per invoice; the route layer enforces no-double-pay
/// via 409 Conflict. The audit chain remains append-only — if a
/// payment is recorded in error, the operator fixes it via a new
/// audit entry in a future PR or via direct ledger inspection
/// (rare; not in v1 scope per the session-92 brief). The F12
/// four-edit ritual fires once — the twelfth landing across
/// PR-6.1 / PR-7-B-3 / PR-8 / PR-10 / PR-11 / PR-12 / PR-13 /
/// PR-14 / PR-15 / PR-19 / PR-20 / PR-70, mechanical at this
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

    /// An operator recorded a payment against a `Finalized` invoice
    /// (PR-70, ADR-0039). Operational metadata only — the NAV
    /// regulatory state ladder is unchanged by this entry. Payload
    /// (`InvoicePaymentRecordedPayload`) carries `invoice_id`,
    /// `idempotency_key`, `paid_at` (YYYY-MM-DD), `amount_minor`
    /// (i64), `currency` (must match invoice), `method`
    /// (closed-vocab: BankTransfer / Cash / Card / Other), and
    /// optional `reference`.
    ///
    /// The `invoice.` prefix MUST hold so the per-invoice export
    /// bundle's (ADR-0009 §8) `invoice.*` glob picks up the new
    /// entries alongside every other lifecycle entry — same
    /// silent-omission-failure-mode posture every prior PR's
    /// prefix-pin test names. PR-70, ADR-0039 §2.
    InvoicePaymentRecorded,

    /// An invoice was emailed to its buyer via SMTP (PR-92, ADR-0047).
    /// One entry per send ATTEMPT — both successful sends and
    /// transport / TLS / auth / recipient-rejected failures emit an
    /// entry so the operator-twin record never has gaps. Payload
    /// (`InvoiceEmailedSentPayload`) carries `invoice_id`,
    /// `idempotency_key`, `recipient`, `subject`, `outcome`
    /// (`"succeeded"` / `"failed"`), optional `error_class`,
    /// optional `error_detail`, `auto` (post-issue auto-send vs
    /// operator-clicked manual send), and `attached_xml` (whether
    /// the NAV XML rode alongside the PDF). NO secrets — see the
    /// payload type docs and `audit_payload_emailed_no_secrets` pin.
    ///
    /// The `invoice.` prefix MUST hold so the per-invoice export
    /// bundle's (ADR-0009 §8) `invoice.*` glob picks up the new
    /// entries alongside every other lifecycle entry — same
    /// silent-omission-failure-mode posture every prior PR's
    /// prefix-pin test names. PR-92, ADR-0047 §4.
    InvoiceEmailedSent,

    /// The operator acknowledged the one-time first-production-launch
    /// confirmation (S166, prod-prep PR #2). Written by the
    /// `/health/acknowledge-first-prod-launch` route when the operator
    /// types `ABERP` and clicks Proceed on the first launch of a
    /// production binary. Payload (`FirstProdLaunchAcknowledgedPayload`)
    /// carries `acknowledged_at` (RFC3339) and `tenant`.
    ///
    /// NOT an `invoice.`-scoped event — there is no invoice in flight; it
    /// is a system-lifecycle event, so it carries the `system.` prefix.
    /// Preserving it in the ledger gives a permanent, hash-chained record
    /// that a human consented to real fiscal operation before the first
    /// real submission — a legal-grade event worth the unusual move of
    /// writing to the audit trail from a `/health` endpoint.
    FirstProdLaunchAcknowledged,

    /// S171 / PR-171 — the boot-time upgrade-snapshot check (see
    /// `apps/aberp/src/serve.rs::check_upgrade_snapshot`) detected a
    /// delta between the operator's pre-upgrade snapshot of
    /// `[seller.smtp]` + `[seller.numbering]` (written by
    /// `tools/snapshot-prod.sh` into
    /// `~/.aberp/<tenant>/.upgrade-snapshot.toml`) and the current
    /// `seller.toml`. The check refuses to start, but appends THIS
    /// audit entry first so the divergence is permanently recorded
    /// in the hash chain — even if the operator resolves it by
    /// `mv`-ing the snapshot file to `.acknowledged-*`. Payload
    /// (`UpgradeSnapshotMismatchPayload`) carries the list of
    /// changed field names and the tenant. Like
    /// `FirstProdLaunchAcknowledged` this is a system-lifecycle
    /// event, not invoice-scoped, so it carries the `system.` prefix
    /// and never enters a per-invoice export bundle.
    UpgradeSnapshotMismatch,

    /// S177 / PR-177 — an INCOMING (supplier-issued, ABERP-received)
    /// invoice was ingested into the local AP-side mirror table
    /// `ap_invoice`. Carries the local AP-side row id
    /// (`apinv_<ULID>`), the operator-decision idempotency key, the
    /// supplier's tax number + name, the supplier's invoice number,
    /// the dates + totals + currency, and an optional pointer to the
    /// raw NAV InvoiceData XML on disk
    /// (`~/.aberp/<tenant>/ap-artifacts/<id>.xml`).
    ///
    /// **NOT `invoice.`-prefixed.** Outgoing-invoice lifecycle events
    /// use the `invoice.` prefix so the per-invoice export bundle's
    /// `invoice.*` glob (ADR-0009 §8) picks them up. An incoming
    /// invoice is NOT one of this tenant's regulated outgoing
    /// invoices — it has no `inv_<ULID>` id, no `invoice_id` field on
    /// the payload — sweeping it into an outgoing invoice's bundle
    /// would be wrong. The `system.` prefix keeps it out of every
    /// per-invoice bundle by construction. The downstream
    /// per-invoice export bundle's exhaustive match (`apps/aberp/src/
    /// export_invoice_bundle.rs::extract_nav_xml`) and the verifier's
    /// (`crates/aberp-verify/src/verify.rs`) both classify this kind
    /// as no-NAV-bytes-in-bundle.
    ///
    /// **AP module v1 ships in two parts.** S177 is the BACKEND:
    /// schema + status workflow + audit events + HTTP routes + a
    /// manual-ingestion route that takes operator-supplied typed
    /// fields and an optional raw NAV InvoiceData XML. The NAV
    /// auto-sync daemon (`queryInvoiceDigest INBOUND` + per-digest
    /// `queryInvoiceData` fanout + InvoiceData parser) is a SEPARATE
    /// PR — `queryInvoiceDigest` is not yet in `nav-transport` and
    /// adding it requires NAV-testbed verification of the digest
    /// response shape per the same posture
    /// `render_query_invoice_data_request` documents
    /// (`crates/nav-transport/src/soap/mod.rs`). The audit kind +
    /// payload shape ARE the load-bearing surface for the future
    /// daemon — it will call the same `ingest_incoming_invoice`
    /// helper and write the same audit entry, so adding the daemon
    /// later is additive and does not bump the kind. F12 four-edit
    /// ritual fires once.
    IncomingInvoiceIngested,

    /// S177 / PR-177 — operator-decided status change on an
    /// AP-side incoming invoice. Closed-vocab transitions:
    /// `Outstanding → Paid` (operator records that the supplier was
    /// paid), `Outstanding → Irrelevant` (operator marks the invoice
    /// as not-our-problem with a required reason), `Paid →
    /// Outstanding` and `Irrelevant → Outstanding` (operator unwinds
    /// a prior status change). The local mirror row's `local_status`
    /// column is the queryable read-side; THIS entry is the
    /// hash-chained audit trail of WHO changed WHAT WHEN and WHY.
    ///
    /// Payload (`IncomingInvoiceStatusChangedPayload`) carries the
    /// AP-side row id, the idempotency key, the from/to status
    /// strings, and the operator's optional free-form `reason` (REQUIRED
    /// when `to_status == "Irrelevant"`; OPTIONAL otherwise per the
    /// session-177 brief). Same `system.` prefix posture as
    /// `IncomingInvoiceIngested` — never sweeps a per-outgoing-invoice
    /// bundle. F12 four-edit ritual fires once.
    IncomingInvoiceStatusChanged,

    /// S178 / PR-178 — the AP-side auto-sync daemon completed one
    /// poll cycle against NAV's `queryInvoiceDigest INBOUND`
    /// endpoint. ONE entry per cycle (not per ingested digest); the
    /// per-digest ingestions emit their own
    /// `IncomingInvoiceIngested` entries via the same
    /// `ingest_incoming_invoice` helper the manual route uses.
    ///
    /// Payload (`IncomingInvoiceSyncCycleCompletedPayload`) carries
    /// the date window queried (`date_from` / `date_to`), the
    /// `ingested_count` (number of brand-new rows inserted),
    /// `skipped_count` (digest rows that already existed in
    /// `ap_invoice`), `pages_walked`, `elapsed_ms`, the closed-vocab
    /// `trigger` (`"daemon"` for the cadence tick / boot tick,
    /// `"manual"` for the operator-clicked /sync-now route), and an
    /// optional `error` field naming the loud-failure cause when the
    /// cycle aborted early (NAV rejected the digest call, etc.).
    ///
    /// Same `system.` prefix posture as the other AP-side events —
    /// the per-OUTGOING-invoice export bundle's `invoice.*` glob
    /// MUST NEVER sweep this. F12 four-edit ritual fires once.
    IncomingInvoiceSyncCycleCompleted,

    /// S180 / PR-180 — one outgoing invoice was restored from NAV's
    /// `queryInvoiceDigest OUTBOUND` view into the local
    /// `restored_invoice` mirror table. The operator-triggered wizard
    /// at `POST /api/restore-from-nav-outgoing` writes ONE entry per
    /// row inserted; there is no per-cycle summary kind because the
    /// wizard is operator-paced, not a recurring daemon — the
    /// HTTP response body carries the {restored, skipped, errored}
    /// counts directly.
    ///
    /// Payload (`InvoiceRestoredFromNavPayload`) carries the local
    /// `restored_invoice.id` (`rinv_<ULID>` — a NEW ULID minted at
    /// restore time per the S180 brief), the operator-decision
    /// idempotency key, NAV's `source_nav_invoice_number` (the
    /// canonical `<series>/<seq>` shape — the lookup key for
    /// idempotency), NAV's `source_nav_transaction_id` from the
    /// digest, the `issue_date` (YYYY-MM-DD), totals + currency, and
    /// the `year` window the wizard was invoked for.
    ///
    /// **NOT `invoice.`-prefixed — `system.`-prefixed.** The restored
    /// row lives in `restored_invoice` (NOT `invoice`) so the
    /// per-OUTGOING-invoice export bundle's `invoice.*` glob must
    /// NEVER sweep it. The canonical regulated invoice surface
    /// (`invoice` table, audit chain `InvoiceDraftCreated → … →
    /// InvoiceAckStatus(SAVED)`) is the operator's record for
    /// invoices ISSUED on this tenant; a restored row is a
    /// RECOVERED VIEW of an invoice NAV already holds, not a
    /// re-issuance. Treating them identically would corrupt the
    /// per-invoice export bundle, the audit-chain stuck-precondition
    /// walker, and the printed-PDF render path.
    ///
    /// **v1 is digest-only.** Same conservative posture S178 took
    /// with `IncomingInvoiceSyncCycleCompleted` — the wizard does
    /// NOT fan out per-digest `queryInvoiceData` calls. The
    /// `restored_invoice` row carries the typed fields the digest
    /// emits (invoice_number, issue_date, totals, currency,
    /// transaction_id); line-item extraction + customer extraction
    /// are deferred to v2 along with partner/product extraction per
    /// the session-180 brief's explicit scope-cap.
    ///
    /// F12 four-edit ritual fires once.
    InvoiceRestoredFromNav,

    /// S210 / PR-204 — the quote-intake daemon (sister-service poll
    /// over a bearer-authed HTTP API) completed one cycle. ONE
    /// entry per CYCLE (not per fetched quote); the per-quote
    /// staging into `quote_intake_log` is the queryable read-side
    /// for which quotes were ingested when, and the operator pickup
    /// (S211) emits the canonical `InvoiceDraftCreated` audit row
    /// when the staged draft is actually issued through the
    /// allocator.
    ///
    /// Payload (`aberp_quote_intake::QuoteIntakePollPayload`)
    /// carries the idempotency key, cycle trigger (`"daemon"` /
    /// `"manual"`), counts (`fetched` / `created` /
    /// `skipped_duplicate` / `writeback_failed` / `failed`),
    /// `elapsed_ms`, and an optional `error` field when the cycle
    /// aborted early (transport failure, 401, 503).
    ///
    /// Audit-emission policy (per S210 brief §7): the cycle entry
    /// is written ONLY when something happened — `fetched > 0`,
    /// `writeback_failed > 0`, OR `error.is_some()`. Pure-zero
    /// no-op cycles are silent to keep the audit chain from
    /// drowning in 1/minute "saw 0 quotes" noise. The brief calls
    /// this out explicitly; the per-cycle log line at `info!` still
    /// carries the summary for ops visibility.
    ///
    /// Same `system.` prefix posture as the other operator-
    /// triggered background events — the per-OUTGOING-invoice
    /// export bundle's `invoice.*` glob MUST NEVER sweep this.
    /// F12 four-edit ritual fires once.
    QuoteIntakePollCompleted,

    /// S213 / PR-209 — ONE per `aberp serve` shutdown. The
    /// graceful-shutdown coordinator (`apps/aberp/src/shutdown.rs`)
    /// emits this row exactly once at the end of its drain pass,
    /// just before `std::process::exit(0)`. Payload
    /// (`aberp::audit_payloads::DaemonShutdownCompletedPayload`)
    /// names each registered daemon's outcome (clean / timeout) so
    /// a postmortem can ask "why did NAV poll always time out?"
    /// without grepping log files.
    ///
    /// `system.`-prefixed — never sweeps a per-outgoing-invoice
    /// export bundle. The payload carries shutdown telemetry only;
    /// no NAV bytes. F12 four-edit ritual fires once.
    DaemonShutdownCompleted,

    /// S220 / PR-217 — the buyer-backfill cycle completed one pass.
    /// The boot-time backfill walks restored_invoice rows with a
    /// NULL `customer_name` and tries to fetch buyer fields via NAV's
    /// `queryInvoiceData OUTBOUND`. Per [[aberp-extnav-partner-nav-gap]]
    /// the call is entitlement-gated to the original submitter — for
    /// invoices issued via Billingo / KBoss / etc. it returns no
    /// `customerInfo` and the row stays NULL. S218 surfaced this
    /// silently; this event makes the cycle outcome observable.
    ///
    /// Payload (`RestoreBuyerBackfillCycleCompletedPayload`) carries
    /// `idempotency_key`, closed-vocab `trigger` (`"boot"` today;
    /// `"manual"` is reserved for a future operator-paced re-run),
    /// counters (`scanned` / `backfilled` / `backfilled_without_name`
    /// / `errored`), `first_error_messages` (Vec<String>, cap 3) so
    /// "why did backfill fail on these rows" is answerable without
    /// grepping logs, `elapsed_ms`, and an optional `error` when the
    /// cycle itself aborted early (transport setup, no creds).
    ///
    /// Same `system.` prefix posture as the other operator-triggered
    /// background events — the per-OUTGOING-invoice export bundle's
    /// `invoice.*` glob MUST NEVER sweep this. F12 four-edit ritual
    /// fires once.
    RestoreBuyerBackfillCycleCompleted,

    /// S220 / PR-217 — operator manually linked (or unlinked) a
    /// partner on a restored ExtNav invoice row. Per [[aberp-extnav-partner-nav-gap]]
    /// NAV won't expose buyer info for invoices submitted via other
    /// software; the SPA exposes a partner-picker so the operator can
    /// annotate ExtNav rows from their own knowledge. This is the
    /// audit trail for those decisions.
    ///
    /// Payload (`ExtNavPartnerManualLinkPayload`) carries the
    /// `restored_invoice_id`, `source_nav_invoice_number`, the
    /// `partner_id_before` / `partner_id_after` (Option<String> on
    /// both — None on "clear", None on "first link"), and the
    /// denormalized `customer_name_before` / `customer_name_after` so
    /// the audit trail tells the WHO without joining `partners`
    /// (which may have been mutated since).
    ///
    /// `system.`-prefixed: restored_invoice lives outside the
    /// canonical `invoice` table, so the per-OUTGOING-invoice export
    /// bundle's `invoice.*` glob MUST NEVER sweep this (same posture
    /// as `InvoiceRestoredFromNav`). F12 four-edit ritual fires once.
    ExtNavPartnerManualLink,

    /// S228 / PR-224 / ADR-0060 — a `CanonicalEvent` emitted by a
    /// registered Stage 3 adapter (manufacturing execution: CNC /
    /// robot / Renishaw / barcode / laser) was recorded into the
    /// audit ledger. ONE kind for ALL canonical event subtypes per
    /// ADR-0060 §"One EventKind for all MES events is too coarse —
    /// how does the operator filter the audit ledger?" — the
    /// payload's `event.type` discriminator (visible via
    /// `json_extract`) is the SPA / SQL filter handle.
    ///
    /// **New prefix family `mes.`** — a third prefix alongside
    /// `invoice.*` (per outgoing-invoice surface) and `system.*`
    /// (everything else system-lifecycle). Future Stage 3 sub-surfaces
    /// (e.g. an adapter-registered event distinct from
    /// per-event-recording) stay under `mes.*`. Rationale per
    /// ADR-0060 §"Storage prefix `mes.`": segregation keeps each
    /// existing prefix consumer's glob narrow; `system.*` consumers
    /// (per-OUTGOING-invoice export bundle's exclusion glob, the
    /// AP-side query helpers) don't get accidentally swept by Stage 3
    /// traffic.
    ///
    /// Payload (`aberp_mes::MesAdapterEventPayload` in the
    /// `aberp-mes` crate) carries the emitting adapter's `name`, the
    /// operator-decision idempotency key, and the typed
    /// `CanonicalEvent` (one of six initial variants: `PartMoved` /
    /// `MachineStateChanged` / `QualityResultReceived` /
    /// `ScanReceived` / `WorkOrderStateChanged` / `RobotTaskQueued`).
    /// Future canonical event variants extend `CanonicalEvent`
    /// without touching this `EventKind` — the audit-ledger crate
    /// stays small.
    ///
    /// **Phase α scope-cap.** Phase α (PR-224) defines this variant
    /// and the payload contract; the runtime task that subscribes to
    /// the per-adapter broadcast streams and actually writes ledger
    /// entries lands in Phase β alongside the first real adapter
    /// implementation. The audit-ledger surface is the load-bearing
    /// pin — adding the runtime later is additive.
    ///
    /// F12 four-edit ritual fires once.
    MesAdapterEvent,

    /// S231 / PR-227 / ADR-0061 — one row was appended to
    /// `stock_movements` (the inventory module's append-only ledger).
    /// Per ADR-0061 §4 stock movements are **regulated state**, not
    /// adapter telemetry, so they emit a distinct EventKind rather than
    /// riding on [`EventKind::MesAdapterEvent`] (which is subject to
    /// broadcast lossiness per ADR-0060 §"Consequences" #4 — losing a
    /// stock movement means the cache drifts and inventory is wrong).
    ///
    /// Payload (`aberp_inventory::StockMovementRecordedPayload`)
    /// carries the `movement_id` (`mvt_<ULID>`), `product_id`
    /// (`prd_<ULID>`), `qty_delta` (Decimal as string — same
    /// posture as [[decimal-quantity-s157]]), the closed-vocab
    /// `MovementReason`, an optional `MovementRefKind` + `ref_id`
    /// (NULL for manual operator adjustments — see ADR-0061 §2),
    /// the operator attribution string, and the F8 idempotency key.
    ///
    /// **`mes.` prefix** per ADR-0061 §4: Stage 3 modules (Inventory,
    /// Work Orders, QA, Dispatch) share the `mes.*` prefix family
    /// alongside [`EventKind::MesAdapterEvent`]. The per-OUTGOING-
    /// invoice export bundle's `invoice.*` glob (ADR-0009 §8)
    /// excludes these by construction; the exhaustive match in
    /// `extract_nav_xml` (verify.rs + export_invoice_bundle.rs)
    /// requires acknowledgement on the no-NAV-bytes arm.
    ///
    /// F12 four-edit ritual fires once.
    StockMovementRecorded,

    /// S232 / PR-228 / ADR-0062 — a Work Order was created. ONE entry
    /// per WO at Create time; carries the full snapshot (product_id,
    /// qty_target, routing_op_ids, actor, idempotency_key) so the
    /// future operations-dashboard projection can glob `mes.work_order_*`
    /// without sweeping inventory or QA traffic.
    ///
    /// Per ADR-0062 §4 the create-vs-transition split mirrors the
    /// Stage 1 `InvoiceDraftCreated` vs `InvoiceState*` pattern: create
    /// emits the full snapshot once, transitions are deltas.
    ///
    /// `mes.` prefix — Stage 3 modules (Inventory, Work Orders, QA,
    /// Dispatch) share the family.
    ///
    /// F12 four-edit ritual fires once.
    WorkOrderCreated,

    /// S232 / PR-228 / ADR-0062 — a Work Order transitioned between
    /// states per the closed-vocab `WorkOrderState` lifecycle
    /// (Created → Released → InProgress → Completed | Cancelled | OnHold).
    /// Carries `from_state`, `to_state`, optional `reason`, `actor`,
    /// and `source_event_id` (`Some(ULID)` when an adapter event drove
    /// the transition, `None` for SPA button presses per ADR-0062 §4 +
    /// §"Invariant 7").
    ///
    /// `source_event_id` is **load-bearing** — it cross-references the
    /// adapter event's ULID so an operator looking at the timeline can
    /// trace "the state change at 12:34 was triggered by adapter X's
    /// scan at 12:33."
    ///
    /// `mes.` prefix per ADR-0062 §4.
    ///
    /// F12 four-edit ritual fires once.
    WorkOrderStateChanged,

    /// S232 / PR-228 / ADR-0062 — a Routing Operation transitioned
    /// between states per the narrower `RoutingOpState` vocab
    /// (Pending → Active → Completed | Skipped). Per-op events are a
    /// separate kind from `WorkOrderStateChanged` so a future
    /// operations-dashboard projection can glob `mes.routing_op_*`
    /// without sweeping WO-level events (ADR-0062 §4).
    ///
    /// `mes.` prefix per ADR-0062 §4.
    ///
    /// F12 four-edit ritual fires once.
    RoutingOpStateChanged,

    /// S233 / PR-229 / ADR-0063 — one Pending `qa_inspections` row was
    /// auto-created when a routing-op transitioned to `Completed`. ONE
    /// entry per inspection at create time; carries `qa_id`, `wo_id`,
    /// `routing_op_id`, the `actor` (operator login or `adapter:<name>`
    /// or `system` per [[s232-work-order-cascade]]'s `ActorKind` pattern),
    /// and the F8 idempotency key.
    ///
    /// `mes.` prefix per ADR-0063 §5. Stage 3 modules (Inventory, Work
    /// Orders, QA, Dispatch) share the family — keeps the per-OUTGOING-
    /// invoice export bundle's `invoice.*` glob narrow and `system.*`
    /// consumers untouched.
    ///
    /// F12 four-edit ritual fires once.
    QaInspectionCreated,

    /// S233 / PR-229 / ADR-0063 — an operator (or adapter) decided on a
    /// QA inspection: Pass / Fail / Rework / Dispose. ONE entry per
    /// decision call. Carries `qa_id`, `from_state`, `to_state`,
    /// optional `reason` + `measurement`, the `actor`, the optional
    /// `source_event_id` (cross-references the upstream adapter event
    /// ULID — `None` for SPA-button-driven decisions, `Some(_)` for
    /// adapter-driven decisions per ADR-0063 §3), the optional
    /// `superseded_qa_id` (set when the decision created a NEW row +
    /// superseded a prior cross-actor row per ADR-0063 §4), and the
    /// F8 idempotency key.
    ///
    /// `mes.` prefix per ADR-0063 §5.
    ///
    /// F12 four-edit ritual fires once.
    QaInspectionDecided,

    /// S234 / PR-230 / ADR-0064 — one Drafted `dispatches` row was
    /// created by an operator (or future adapter) against a Completed
    /// work order. ONE entry per dispatch at create time; carries
    /// `dsp_id`, `wo_id`, `partner_id`, the `actor` (operator login or
    /// `adapter:<name>` or `system` per [[s232-work-order-cascade]]'s
    /// `ActorKind` pattern), and the F8 idempotency key.
    ///
    /// `mes.` prefix per ADR-0064 §6 — Stage 3 modules (Inventory, Work
    /// Orders, QA, Dispatch) share the family. Keeps the per-OUTGOING-
    /// invoice export bundle's `invoice.*` glob narrow and `system.*`
    /// consumers untouched.
    ///
    /// F12 four-edit ritual fires once.
    DispatchCreated,

    /// S234 / PR-230 / ADR-0064 — a Drafted dispatch was flipped to
    /// Shipped. ONE entry per `mark_shipped` call. Carries `dsp_id`,
    /// `wo_id`, `partner_id`, the operator-picked `carrier_kind`
    /// (closed-vocab `CarrierKind`), the optional `tracking_number`,
    /// `shipped_at`, the optional `spawned_invoice_id` (Some(_) when
    /// the injected `InvoiceSpawner` produced a draft in the same tx;
    /// None for the v1 [[NoopInvoiceSpawner]] posture pending PR-230b's
    /// sync billing extraction), the `actor`, and the F8 idempotency
    /// key.
    ///
    /// Per ADR-0064 §5 + §"Invariants pinned" #1 this entry lands in
    /// the SAME transaction as the dispatch state flip, the
    /// `stock_movements` row, and the `spawned_invoice_id` UPDATE. The
    /// audit-trail walks both ways: from dispatch to invoice via this
    /// payload's `spawned_invoice_id`; from the invoice draft's own
    /// `InvoiceDraftCreated` entry back to the dispatch via the
    /// invoice idempotency-key suffix (`derive_from(dispatch.dsp_id,
    /// "spawn_invoice")`).
    ///
    /// `mes.` prefix per ADR-0064 §6.
    ///
    /// F12 four-edit ritual fires once.
    DispatchShipped,
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
            EventKind::InvoiceAnnulmentSubmissionAttempt => "invoice.annulment_submission_attempt",
            EventKind::InvoiceAnnulmentSubmissionResponse => {
                "invoice.annulment_submission_response"
            }
            EventKind::InvoiceAnnulmentAckStatus => "invoice.annulment_ack_status",
            EventKind::InvoiceAnnulmentReceiverConfirmation => {
                "invoice.annulment_receiver_confirmation"
            }
            EventKind::InvoiceSubmissionAttemptFailed => "invoice.submission_attempt_failed",
            EventKind::InvoiceCheckPerformed => "invoice.check_performed",
            EventKind::InvoicePaymentRecorded => "invoice.payment_recorded",
            EventKind::InvoiceEmailedSent => "invoice.emailed_sent",
            EventKind::FirstProdLaunchAcknowledged => "system.first_prod_launch_acknowledged",
            EventKind::UpgradeSnapshotMismatch => "system.upgrade_snapshot_mismatch",
            EventKind::IncomingInvoiceIngested => "system.incoming_invoice_ingested",
            EventKind::IncomingInvoiceStatusChanged => "system.incoming_invoice_status_changed",
            EventKind::IncomingInvoiceSyncCycleCompleted => {
                "system.incoming_invoice_sync_cycle_completed"
            }
            EventKind::InvoiceRestoredFromNav => "system.invoice_restored_from_nav",
            EventKind::QuoteIntakePollCompleted => "system.quote_intake_poll_completed",
            EventKind::DaemonShutdownCompleted => "system.daemon_shutdown_completed",
            EventKind::RestoreBuyerBackfillCycleCompleted => {
                "system.restore_buyer_backfill_cycle_completed"
            }
            EventKind::ExtNavPartnerManualLink => "system.extnav_partner_manual_link",
            EventKind::MesAdapterEvent => "mes.adapter_event",
            EventKind::StockMovementRecorded => "mes.stock_movement_recorded",
            EventKind::WorkOrderCreated => "mes.work_order_created",
            EventKind::WorkOrderStateChanged => "mes.work_order_state_changed",
            EventKind::RoutingOpStateChanged => "mes.routing_op_state_changed",
            EventKind::QaInspectionCreated => "mes.qa_inspection_created",
            EventKind::QaInspectionDecided => "mes.qa_inspection_decided",
            EventKind::DispatchCreated => "mes.dispatch_created",
            EventKind::DispatchShipped => "mes.dispatch_shipped",
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
            "invoice.payment_recorded" => Ok(EventKind::InvoicePaymentRecorded),
            "invoice.emailed_sent" => Ok(EventKind::InvoiceEmailedSent),
            "system.first_prod_launch_acknowledged" => Ok(EventKind::FirstProdLaunchAcknowledged),
            "system.upgrade_snapshot_mismatch" => Ok(EventKind::UpgradeSnapshotMismatch),
            "system.incoming_invoice_ingested" => Ok(EventKind::IncomingInvoiceIngested),
            "system.incoming_invoice_status_changed" => Ok(EventKind::IncomingInvoiceStatusChanged),
            "system.incoming_invoice_sync_cycle_completed" => {
                Ok(EventKind::IncomingInvoiceSyncCycleCompleted)
            }
            "system.invoice_restored_from_nav" => Ok(EventKind::InvoiceRestoredFromNav),
            "system.quote_intake_poll_completed" => Ok(EventKind::QuoteIntakePollCompleted),
            "system.daemon_shutdown_completed" => Ok(EventKind::DaemonShutdownCompleted),
            "system.restore_buyer_backfill_cycle_completed" => {
                Ok(EventKind::RestoreBuyerBackfillCycleCompleted)
            }
            "system.extnav_partner_manual_link" => Ok(EventKind::ExtNavPartnerManualLink),
            "mes.adapter_event" => Ok(EventKind::MesAdapterEvent),
            "mes.stock_movement_recorded" => Ok(EventKind::StockMovementRecorded),
            "mes.work_order_created" => Ok(EventKind::WorkOrderCreated),
            "mes.work_order_state_changed" => Ok(EventKind::WorkOrderStateChanged),
            "mes.routing_op_state_changed" => Ok(EventKind::RoutingOpStateChanged),
            "mes.qa_inspection_created" => Ok(EventKind::QaInspectionCreated),
            "mes.qa_inspection_decided" => Ok(EventKind::QaInspectionDecided),
            "mes.dispatch_created" => Ok(EventKind::DispatchCreated),
            "mes.dispatch_shipped" => Ok(EventKind::DispatchShipped),
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
            EventKind::InvoicePaymentRecorded,
            EventKind::InvoiceEmailedSent,
            EventKind::FirstProdLaunchAcknowledged,
            EventKind::UpgradeSnapshotMismatch,
            EventKind::IncomingInvoiceIngested,
            EventKind::IncomingInvoiceStatusChanged,
            EventKind::IncomingInvoiceSyncCycleCompleted,
            EventKind::InvoiceRestoredFromNav,
            EventKind::QuoteIntakePollCompleted,
            EventKind::DaemonShutdownCompleted,
            EventKind::RestoreBuyerBackfillCycleCompleted,
            EventKind::ExtNavPartnerManualLink,
            EventKind::MesAdapterEvent,
            EventKind::StockMovementRecorded,
            EventKind::WorkOrderCreated,
            EventKind::WorkOrderStateChanged,
            EventKind::RoutingOpStateChanged,
            EventKind::QaInspectionCreated,
            EventKind::QaInspectionDecided,
            EventKind::DispatchCreated,
            EventKind::DispatchShipped,
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

    /// S166 specifically: `FirstProdLaunchAcknowledged` is a
    /// system-lifecycle event, NOT invoice-scoped. Its on-disk string
    /// MUST carry the `system.` prefix (and NOT `invoice.`) so the
    /// per-invoice export bundle's `invoice.*` glob never sweeps a
    /// boot-acknowledgement entry into an invoice's evidence bundle.
    /// The inverse of the `*_use_invoice_prefix` pins above.
    #[test]
    fn s166_first_prod_launch_kind_uses_system_prefix() {
        assert_eq!(
            EventKind::FirstProdLaunchAcknowledged.as_str(),
            "system.first_prod_launch_acknowledged"
        );
        assert!(EventKind::FirstProdLaunchAcknowledged
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::FirstProdLaunchAcknowledged
            .as_str()
            .starts_with("invoice."));
    }

    /// S171: `UpgradeSnapshotMismatch` is also system-lifecycle (the
    /// pre-upgrade snapshot check at boot detected drift in
    /// `[seller.smtp]` or `[seller.numbering]`); same prefix
    /// invariant as S166 above so the per-invoice export bundle
    /// glob never sweeps it.
    #[test]
    fn s171_upgrade_snapshot_mismatch_kind_uses_system_prefix() {
        assert_eq!(
            EventKind::UpgradeSnapshotMismatch.as_str(),
            "system.upgrade_snapshot_mismatch"
        );
        assert!(EventKind::UpgradeSnapshotMismatch
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::UpgradeSnapshotMismatch
            .as_str()
            .starts_with("invoice."));
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

    /// PR-70 / ADR-0039 §2: the operational mark-as-paid event.
    /// The `invoice.` prefix MUST hold so the per-invoice export
    /// bundle's (ADR-0009 §8) `invoice.*` glob picks up the new
    /// entries alongside every other lifecycle entry — same
    /// silent-omission-failure-mode posture every prior PR's
    /// prefix-pin test names.
    #[test]
    fn pr_70_payment_recorded_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoicePaymentRecorded.as_str(),
            "invoice.payment_recorded"
        );
        assert!(EventKind::InvoicePaymentRecorded
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-70 / ADR-0039 §2: deliberate fork from every other kind.
    /// Payment recording is operational metadata, structurally
    /// distinct from every regulatory-ladder entry; pinning the
    /// discriminator-level distinction here catches a future refactor
    /// accidentally collapsing payment-recorded onto an existing
    /// lifecycle kind. Same fork-discipline posture as
    /// `pr_20_check_performed_is_distinct_from_submission_kinds`.
    #[test]
    fn pr_70_payment_recorded_is_distinct_from_all_other_kinds() {
        // Spot-check against the closest semantic neighbours — the
        // chain-link entries (which also mark non-regulatory-ladder
        // transitions) and the operator-decision entries
        // (retry-requested / marked-abandoned).
        assert_ne!(
            EventKind::InvoicePaymentRecorded.as_str(),
            EventKind::InvoiceStornoIssued.as_str()
        );
        assert_ne!(
            EventKind::InvoicePaymentRecorded.as_str(),
            EventKind::InvoiceModificationIssued.as_str()
        );
        assert_ne!(
            EventKind::InvoicePaymentRecorded.as_str(),
            EventKind::InvoiceRetryRequested.as_str()
        );
        assert_ne!(
            EventKind::InvoicePaymentRecorded.as_str(),
            EventKind::InvoiceMarkedAbandoned.as_str()
        );
        assert_ne!(
            EventKind::InvoicePaymentRecorded.as_str(),
            EventKind::InvoiceTechnicalAnnulmentRequested.as_str()
        );
    }

    /// PR-92 / ADR-0047 §4: the buyer-facing emailed-sent event. The
    /// `invoice.` prefix MUST hold so the per-invoice export bundle's
    /// (ADR-0009 §8) `invoice.*` glob picks up the new entries
    /// alongside every other lifecycle entry — same silent-omission-
    /// failure-mode posture every prior PR's prefix-pin test names.
    #[test]
    fn pr_92_emailed_sent_kind_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceEmailedSent.as_str(),
            "invoice.emailed_sent"
        );
        assert!(EventKind::InvoiceEmailedSent
            .as_str()
            .starts_with("invoice."));
    }

    /// PR-92 / ADR-0047 §4: deliberate fork from every other kind.
    /// An emailed-sent event is buyer-communication evidence,
    /// structurally distinct from every prior lifecycle / payment /
    /// annulment kind; pinning the distinction here catches a future
    /// refactor accidentally collapsing emailed-sent onto an
    /// existing kind.
    #[test]
    fn pr_92_emailed_sent_is_distinct_from_all_other_kinds() {
        assert_ne!(
            EventKind::InvoiceEmailedSent.as_str(),
            EventKind::InvoicePaymentRecorded.as_str()
        );
        assert_ne!(
            EventKind::InvoiceEmailedSent.as_str(),
            EventKind::InvoiceSubmissionResponse.as_str()
        );
        assert_ne!(
            EventKind::InvoiceEmailedSent.as_str(),
            EventKind::InvoiceStornoIssued.as_str()
        );
        assert_ne!(
            EventKind::InvoiceEmailedSent.as_str(),
            EventKind::InvoiceModificationIssued.as_str()
        );
    }

    /// S177 / PR-177 — AP-side incoming-invoice ingestion event. The
    /// `system.` prefix MUST hold so the per-OUTGOING-invoice export
    /// bundle's `invoice.*` glob NEVER sweeps it into an outgoing
    /// invoice's evidence bundle (the AP row has no `inv_<ULID>` —
    /// such a sweep would be a category error). Inverse of every
    /// `invoice.`-prefix pin above.
    #[test]
    fn s177_incoming_invoice_ingested_kind_uses_system_prefix() {
        assert_eq!(
            EventKind::IncomingInvoiceIngested.as_str(),
            "system.incoming_invoice_ingested"
        );
        assert!(EventKind::IncomingInvoiceIngested
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::IncomingInvoiceIngested
            .as_str()
            .starts_with("invoice."));
    }

    /// S177 / PR-177 — AP-side status-change event (paid /
    /// outstanding / irrelevant transitions). Same `system.` prefix
    /// invariant as `IncomingInvoiceIngested`.
    #[test]
    fn s177_incoming_invoice_status_changed_kind_uses_system_prefix() {
        assert_eq!(
            EventKind::IncomingInvoiceStatusChanged.as_str(),
            "system.incoming_invoice_status_changed"
        );
        assert!(EventKind::IncomingInvoiceStatusChanged
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::IncomingInvoiceStatusChanged
            .as_str()
            .starts_with("invoice."));
    }

    /// S177 / PR-177 — the two AP-side kinds MUST be distinct from
    /// each other and from every outgoing-invoice kind. Same
    /// fork-discipline posture as
    /// `pr_92_emailed_sent_is_distinct_from_all_other_kinds`.
    #[test]
    fn s177_incoming_invoice_kinds_are_distinct() {
        assert_ne!(
            EventKind::IncomingInvoiceIngested.as_str(),
            EventKind::IncomingInvoiceStatusChanged.as_str()
        );
        // Spot-check distinctness from outgoing-invoice kinds with
        // similar semantic neighbours (payment, draft creation).
        assert_ne!(
            EventKind::IncomingInvoiceIngested.as_str(),
            EventKind::InvoiceDraftCreated.as_str()
        );
        assert_ne!(
            EventKind::IncomingInvoiceStatusChanged.as_str(),
            EventKind::InvoicePaymentRecorded.as_str()
        );
    }

    /// S178 / PR-178 — AP-side auto-sync cycle completion event.
    /// Same `system.` prefix invariant as the other AP-side kinds —
    /// must NEVER sweep a per-outgoing-invoice export bundle.
    #[test]
    fn s178_incoming_invoice_sync_cycle_completed_uses_system_prefix() {
        assert_eq!(
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str(),
            "system.incoming_invoice_sync_cycle_completed"
        );
        assert!(EventKind::IncomingInvoiceSyncCycleCompleted
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::IncomingInvoiceSyncCycleCompleted
            .as_str()
            .starts_with("invoice."));
    }

    /// S178 / PR-178 — distinct discriminator from the two prior AP
    /// kinds (cycle-completion is a daemon-tick event, not a
    /// per-invoice ingestion or status change). Same fork-discipline
    /// posture as `s177_incoming_invoice_kinds_are_distinct`.
    #[test]
    fn s178_sync_cycle_completed_is_distinct_from_other_ap_kinds() {
        assert_ne!(
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str(),
            EventKind::IncomingInvoiceIngested.as_str()
        );
        assert_ne!(
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str(),
            EventKind::IncomingInvoiceStatusChanged.as_str()
        );
    }

    /// S180 / PR-180 — NAV-as-DR restore event. `system.`-prefixed so
    /// the per-OUTGOING-invoice export bundle's `invoice.*` glob NEVER
    /// sweeps a restored row (a restored row lives in
    /// `restored_invoice`, NOT `invoice` — it is a recovered VIEW of
    /// what NAV holds, not a re-issuance on this tenant).
    #[test]
    fn s180_invoice_restored_from_nav_uses_system_prefix() {
        assert_eq!(
            EventKind::InvoiceRestoredFromNav.as_str(),
            "system.invoice_restored_from_nav"
        );
        assert!(EventKind::InvoiceRestoredFromNav
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::InvoiceRestoredFromNav
            .as_str()
            .starts_with("invoice."));
    }

    /// S180 / PR-180 — distinct discriminator from every prior AP kind
    /// (restore is an operator-triggered recovery, not an AP-side
    /// ingestion or status change). Same fork-discipline posture as
    /// the other `*_is_distinct_from` tests.
    #[test]
    fn s180_invoice_restored_from_nav_is_distinct_from_ap_kinds() {
        assert_ne!(
            EventKind::InvoiceRestoredFromNav.as_str(),
            EventKind::IncomingInvoiceIngested.as_str()
        );
        assert_ne!(
            EventKind::InvoiceRestoredFromNav.as_str(),
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str()
        );
    }

    /// S210 / PR-204 — quote-intake daemon cycle event. `system.`-
    /// prefixed so the per-OUTGOING-invoice export bundle's
    /// `invoice.*` glob NEVER sweeps a quote-intake cycle row (the
    /// quote-intake daemon is a sister-service poll, NOT an
    /// invoice-lifecycle event).
    #[test]
    fn s210_quote_intake_poll_completed_uses_system_prefix() {
        assert_eq!(
            EventKind::QuoteIntakePollCompleted.as_str(),
            "system.quote_intake_poll_completed"
        );
        assert!(EventKind::QuoteIntakePollCompleted
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::QuoteIntakePollCompleted
            .as_str()
            .starts_with("invoice."));
    }

    /// S210 / PR-204 — distinct discriminator from every prior
    /// background-cycle kind. Same fork-discipline posture as the
    /// other `*_is_distinct_from` tests.
    #[test]
    fn s210_quote_intake_poll_completed_is_distinct() {
        assert_ne!(
            EventKind::QuoteIntakePollCompleted.as_str(),
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str()
        );
        assert_ne!(
            EventKind::QuoteIntakePollCompleted.as_str(),
            EventKind::InvoiceRestoredFromNav.as_str()
        );
    }

    /// S220 / PR-217 — the buyer-backfill cycle kind is a
    /// background-cycle event with the same `system.` prefix posture
    /// as `IncomingInvoiceSyncCycleCompleted` / `QuoteIntakePollCompleted`.
    /// MUST NOT carry an `invoice.` prefix or the per-OUTGOING-invoice
    /// export bundle's `invoice.*` glob would sweep a cycle row into
    /// an evidence bundle that's supposed to carry per-invoice
    /// regulated entries only.
    #[test]
    fn s220_restore_buyer_backfill_cycle_uses_system_prefix() {
        assert_eq!(
            EventKind::RestoreBuyerBackfillCycleCompleted.as_str(),
            "system.restore_buyer_backfill_cycle_completed"
        );
        assert!(EventKind::RestoreBuyerBackfillCycleCompleted
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::RestoreBuyerBackfillCycleCompleted
            .as_str()
            .starts_with("invoice."));
    }

    /// S220 / PR-217 — the ExtNav manual-link kind is operator-paced
    /// metadata on a restored row, NOT a canonical invoice lifecycle
    /// event. Same `system.` prefix posture as `InvoiceRestoredFromNav`.
    /// MUST NOT carry an `invoice.` prefix or the per-OUTGOING-invoice
    /// export bundle's `invoice.*` glob would sweep an annotation
    /// against a restored row into the wrong export bundle.
    #[test]
    fn s220_extnav_partner_manual_link_uses_system_prefix() {
        assert_eq!(
            EventKind::ExtNavPartnerManualLink.as_str(),
            "system.extnav_partner_manual_link"
        );
        assert!(EventKind::ExtNavPartnerManualLink
            .as_str()
            .starts_with("system."));
        assert!(!EventKind::ExtNavPartnerManualLink
            .as_str()
            .starts_with("invoice."));
    }

    /// S228 / PR-224 / ADR-0060 — the Stage 3 manufacturing-execution
    /// event kind. **New prefix family `mes.`** per ADR-0060 §"Storage
    /// prefix `mes.`": a third prefix alongside `invoice.*` (per
    /// outgoing-invoice surface) and `system.*` (everything else
    /// system-lifecycle). MUST NOT start with `invoice.` (would be
    /// silently swept into per-OUTGOING-invoice export bundles, which
    /// is a category error — Stage 3 events have no `inv_<ULID>` to
    /// belong to) and MUST NOT start with `system.` (would force every
    /// existing `system.*` consumer to learn the difference between
    /// "AP sync cycle completed" and "robot arm reported position").
    /// Future Stage 3 sub-surfaces (e.g. an adapter-registered event
    /// distinct from per-event-recording) stay under `mes.*` so the
    /// segregation holds.
    #[test]
    fn s228_mes_adapter_event_uses_mes_prefix() {
        assert_eq!(EventKind::MesAdapterEvent.as_str(), "mes.adapter_event");
        assert!(EventKind::MesAdapterEvent.as_str().starts_with("mes."));
        assert!(!EventKind::MesAdapterEvent.as_str().starts_with("invoice."));
        assert!(!EventKind::MesAdapterEvent.as_str().starts_with("system."));
    }

    /// S228 / PR-224 / ADR-0060 — the MES adapter-event kind MUST be
    /// distinct from every prior cycle/observation/operator-decision
    /// kind. Same fork-discipline posture as
    /// `s210_quote_intake_poll_completed_is_distinct` /
    /// `s180_invoice_restored_from_nav_is_distinct_from_ap_kinds`.
    /// One MesAdapterEvent vs. an existing `system.*` kind would
    /// collapse two semantically distinct event families into one
    /// classifier — exactly the failure mode the prefix-fork
    /// discipline guards against.
    #[test]
    fn s228_mes_adapter_event_is_distinct() {
        assert_ne!(
            EventKind::MesAdapterEvent.as_str(),
            EventKind::QuoteIntakePollCompleted.as_str()
        );
        assert_ne!(
            EventKind::MesAdapterEvent.as_str(),
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str()
        );
        assert_ne!(
            EventKind::MesAdapterEvent.as_str(),
            EventKind::DaemonShutdownCompleted.as_str()
        );
        assert_ne!(
            EventKind::MesAdapterEvent.as_str(),
            EventKind::InvoiceRestoredFromNav.as_str()
        );
        // And distinct from every invoice-prefixed kind too — Stage 3
        // events have no invoice surface to collapse onto.
        assert_ne!(
            EventKind::MesAdapterEvent.as_str(),
            EventKind::InvoiceDraftCreated.as_str()
        );
        assert_ne!(
            EventKind::MesAdapterEvent.as_str(),
            EventKind::InvoicePaymentRecorded.as_str()
        );
    }

    /// S231 / PR-227 / ADR-0061 — the inventory-side stock-movement
    /// event kind. **Same `mes.*` prefix family as `MesAdapterEvent`**
    /// per ADR-0061 §4: Stage 3 modules (Inventory, Work Orders, QA,
    /// Dispatch) all live under `mes.*` so the per-OUTGOING-invoice
    /// export bundle's `invoice.*` glob never sweeps them, and so the
    /// `system.*` consumers (per-OUTGOING-invoice export bundle's
    /// exclusion glob, the AP-side query helpers) do not get
    /// accidentally swept by Stage 3 traffic. MUST NOT start with
    /// `invoice.` (would be silently swept into per-outgoing-invoice
    /// bundles, a category error — stock movements have no
    /// `inv_<ULID>` to belong to) and MUST NOT start with `system.`
    /// (would force every existing `system.*` consumer to learn the
    /// difference between "AP sync cycle completed" and "5 units of
    /// part X were consumed by WO Y").
    #[test]
    fn s231_stock_movement_recorded_uses_mes_prefix() {
        assert_eq!(
            EventKind::StockMovementRecorded.as_str(),
            "mes.stock_movement_recorded"
        );
        assert!(EventKind::StockMovementRecorded
            .as_str()
            .starts_with("mes."));
        assert!(!EventKind::StockMovementRecorded
            .as_str()
            .starts_with("invoice."));
        assert!(!EventKind::StockMovementRecorded
            .as_str()
            .starts_with("system."));
    }

    /// S231 / PR-227 / ADR-0061 — the stock-movement kind MUST be
    /// distinct from `MesAdapterEvent` (adapter telemetry vs regulated
    /// inventory state — different lossiness posture per ADR-0061 §4)
    /// AND from every prior cycle/observation kind. Same fork-discipline
    /// posture as `s228_mes_adapter_event_is_distinct`.
    #[test]
    fn s231_stock_movement_recorded_is_distinct() {
        // Distinct from the sibling mes.* kind — broadcast-lossy telemetry
        // vs lossy-must-not regulated state per ADR-0061 §4.
        assert_ne!(
            EventKind::StockMovementRecorded.as_str(),
            EventKind::MesAdapterEvent.as_str()
        );
        // Distinct from system.* background cycle kinds.
        assert_ne!(
            EventKind::StockMovementRecorded.as_str(),
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str()
        );
        assert_ne!(
            EventKind::StockMovementRecorded.as_str(),
            EventKind::QuoteIntakePollCompleted.as_str()
        );
        // Distinct from invoice.* lifecycle kinds.
        assert_ne!(
            EventKind::StockMovementRecorded.as_str(),
            EventKind::InvoiceDraftCreated.as_str()
        );
        assert_ne!(
            EventKind::StockMovementRecorded.as_str(),
            EventKind::InvoicePaymentRecorded.as_str()
        );
    }

    /// S232 / PR-228 / ADR-0062 — the three Work Order kinds use the
    /// `mes.*` prefix family alongside `MesAdapterEvent` /
    /// `StockMovementRecorded`. Stage 3 modules (Inventory, Work
    /// Orders, QA, Dispatch) share the family so the
    /// per-OUTGOING-invoice export bundle's `invoice.*` glob never
    /// sweeps Stage 3 traffic and `system.*` consumers stay narrow.
    /// MUST NOT start with `invoice.` or `system.`.
    #[test]
    fn s232_work_order_kinds_use_mes_prefix() {
        for k in [
            EventKind::WorkOrderCreated,
            EventKind::WorkOrderStateChanged,
            EventKind::RoutingOpStateChanged,
        ] {
            let s = k.as_str();
            assert!(s.starts_with("mes."), "{s} must start with mes.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
        }
        // Exact storage strings per ADR-0062 §4 table.
        assert_eq!(
            EventKind::WorkOrderCreated.as_str(),
            "mes.work_order_created"
        );
        assert_eq!(
            EventKind::WorkOrderStateChanged.as_str(),
            "mes.work_order_state_changed"
        );
        assert_eq!(
            EventKind::RoutingOpStateChanged.as_str(),
            "mes.routing_op_state_changed"
        );
    }

    /// S232 / PR-228 / ADR-0062 — the three Work Order kinds are
    /// distinct from each other and from the prior `mes.*` kinds
    /// (`MesAdapterEvent` / `StockMovementRecorded`). Catches a future
    /// refactor accidentally collapsing two `mes.*` kinds onto one
    /// storage string.
    #[test]
    fn s232_work_order_kinds_are_distinct() {
        let new_kinds = [
            EventKind::WorkOrderCreated.as_str(),
            EventKind::WorkOrderStateChanged.as_str(),
            EventKind::RoutingOpStateChanged.as_str(),
        ];
        // Pairwise-distinct among themselves.
        assert_ne!(new_kinds[0], new_kinds[1]);
        assert_ne!(new_kinds[0], new_kinds[2]);
        assert_ne!(new_kinds[1], new_kinds[2]);
        // Distinct from the prior `mes.*` kinds.
        for new_k in new_kinds {
            assert_ne!(new_k, EventKind::MesAdapterEvent.as_str());
            assert_ne!(new_k, EventKind::StockMovementRecorded.as_str());
        }
    }

    /// S233 / PR-229 / ADR-0063 — the two QA-queue kinds use the `mes.*`
    /// prefix family alongside the Inventory/WO kinds. MUST NOT start
    /// with `invoice.` or `system.` so the per-OUTGOING-invoice export
    /// bundle's `invoice.*` glob never sweeps QA traffic and existing
    /// `system.*` consumers stay narrow.
    #[test]
    fn s230_qa_kinds_use_mes_prefix() {
        for k in [
            EventKind::QaInspectionCreated,
            EventKind::QaInspectionDecided,
        ] {
            let s = k.as_str();
            assert!(s.starts_with("mes."), "{s} must start with mes.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
        }
        // Exact storage strings per ADR-0063 §5 table.
        assert_eq!(
            EventKind::QaInspectionCreated.as_str(),
            "mes.qa_inspection_created"
        );
        assert_eq!(
            EventKind::QaInspectionDecided.as_str(),
            "mes.qa_inspection_decided"
        );
    }

    /// S233 / PR-229 / ADR-0063 — the two QA-queue kinds are distinct
    /// from each other and from every prior `mes.*` kind. Catches a
    /// future refactor accidentally collapsing two `mes.*` kinds onto
    /// one storage string.
    #[test]
    fn s230_qa_kinds_are_distinct() {
        let qa = [
            EventKind::QaInspectionCreated.as_str(),
            EventKind::QaInspectionDecided.as_str(),
        ];
        assert_ne!(qa[0], qa[1]);
        for k in qa {
            assert_ne!(k, EventKind::MesAdapterEvent.as_str());
            assert_ne!(k, EventKind::StockMovementRecorded.as_str());
            assert_ne!(k, EventKind::WorkOrderCreated.as_str());
            assert_ne!(k, EventKind::WorkOrderStateChanged.as_str());
            assert_ne!(k, EventKind::RoutingOpStateChanged.as_str());
        }
    }

    /// S234 / PR-230 / ADR-0064 — the two Dispatch-board kinds use the
    /// `mes.*` prefix family alongside the Inventory / WO / QA kinds.
    /// MUST NOT start with `invoice.` or `system.` so the per-OUTGOING-
    /// invoice export bundle's `invoice.*` glob never sweeps dispatch
    /// traffic and existing `system.*` consumers stay narrow.
    #[test]
    fn s234_dispatch_kinds_use_mes_prefix() {
        for k in [EventKind::DispatchCreated, EventKind::DispatchShipped] {
            let s = k.as_str();
            assert!(s.starts_with("mes."), "{s} must start with mes.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
        }
        // Exact storage strings per ADR-0064 §6 table.
        assert_eq!(EventKind::DispatchCreated.as_str(), "mes.dispatch_created");
        assert_eq!(EventKind::DispatchShipped.as_str(), "mes.dispatch_shipped");
    }

    /// S234 / PR-230 / ADR-0064 — the two Dispatch-board kinds are
    /// distinct from each other and from every prior `mes.*` kind.
    /// Catches a future refactor accidentally collapsing two `mes.*`
    /// kinds onto one storage string.
    #[test]
    fn s234_dispatch_kinds_are_distinct() {
        let dsp = [
            EventKind::DispatchCreated.as_str(),
            EventKind::DispatchShipped.as_str(),
        ];
        assert_ne!(dsp[0], dsp[1]);
        for k in dsp {
            assert_ne!(k, EventKind::MesAdapterEvent.as_str());
            assert_ne!(k, EventKind::StockMovementRecorded.as_str());
            assert_ne!(k, EventKind::WorkOrderCreated.as_str());
            assert_ne!(k, EventKind::WorkOrderStateChanged.as_str());
            assert_ne!(k, EventKind::RoutingOpStateChanged.as_str());
            assert_ne!(k, EventKind::QaInspectionCreated.as_str());
            assert_ne!(k, EventKind::QaInspectionDecided.as_str());
        }
    }

    /// S220 / PR-217 — the two new kinds must be distinct from every
    /// prior cycle/restoration kind. Same fork-discipline posture as
    /// the other `*_is_distinct_from` tests.
    #[test]
    fn s220_kinds_are_distinct() {
        assert_ne!(
            EventKind::RestoreBuyerBackfillCycleCompleted.as_str(),
            EventKind::ExtNavPartnerManualLink.as_str()
        );
        assert_ne!(
            EventKind::RestoreBuyerBackfillCycleCompleted.as_str(),
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str()
        );
        assert_ne!(
            EventKind::RestoreBuyerBackfillCycleCompleted.as_str(),
            EventKind::QuoteIntakePollCompleted.as_str()
        );
        assert_ne!(
            EventKind::ExtNavPartnerManualLink.as_str(),
            EventKind::InvoiceRestoredFromNav.as_str()
        );
    }
}
