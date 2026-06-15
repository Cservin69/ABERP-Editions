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

    /// S261 / PR-250 — the AGGREGATE batch-summary entry for ONE
    /// operator-confirmed restore-from-NAV wizard run. Distinct from
    /// the per-row `InvoiceRestoredFromNav` entries the same run emits:
    /// those are the idempotency source-of-truth + per-invoice lineage
    /// (one per freshly-restored invoice); THIS is the human-facing
    /// "the operator rebuilt this DB from NAV for year N, here are the
    /// totals + checksum" landmark — exactly one per confirmed run.
    ///
    /// Payload (`aberp::audit_payloads::RestoreFromNavRunPayload`)
    /// carries the F8 idempotency key, the `year`, the `invoice_count`
    /// (distinct NAV invoice numbers seen for the year), the freshly-
    /// inserted `partner_count` / `product_count`, the run completion
    /// `ts`, and the `checksum` — SHA-256 of the sorted + deduplicated
    /// NAV invoice-number list. The checksum pins WHAT NAV held (not
    /// what the local DB was missing) so two runs against the same NAV
    /// state yield the identical value, recomputable independently from
    /// a NAV digest dump.
    ///
    /// `system.`-prefixed: a recovery batch is NOT a per-OUTGOING-
    /// invoice lifecycle event, so the per-invoice export bundle's
    /// `invoice.*` glob MUST NEVER sweep it — same posture as
    /// `InvoiceRestoredFromNav` and the other `system.` restore kinds.
    /// F12 four-edit ritual fires once.
    RestoreFromNavRun,

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

    /// S236 / PR-230b — a pre-allocation invoice draft was staged.
    /// Distinct from `InvoiceDraftCreated`: this variant fires when
    /// a draft row is inserted into `invoice_draft` (no
    /// `sequence_number` allocated, no slot burned per ADR-0009 §169);
    /// `InvoiceDraftCreated` continues to fire when the regulated
    /// `invoice` row is inserted via `allocate_in_tx` (sequence
    /// reserved, the Draft → Ready transition in the existing
    /// allocator).
    ///
    /// Carries `draft_id` (`drf_<ULID>`), `tenant_id`, `partner_id`,
    /// the operator/adapter `actor` string, the F8 idempotency key,
    /// and optionally the `source_dispatch_id` so a future audit walk
    /// can reconstruct "this draft was spawned by dispatch dsp_X
    /// against partner ptr_Y on behalf of WO wo_Z". The chain
    /// continues at promotion time via the operator-issued
    /// `InvoiceSequenceReserved` + `InvoiceDraftCreated` pair, which
    /// references the draft id in their idempotency key suffix
    /// (`derive_from(draft.drf_id, "issue")`).
    ///
    /// `invoice.` prefix family because the entry is keyed by a
    /// `drf_<ULID>` id; the per-invoice export bundle filters by the
    /// promoted invoice's `inv_<ULID>` id and so does NOT sweep
    /// staged-then-deleted drafts. The Stage 3 dispatch tx fires
    /// this entry alongside `mes.dispatch_shipped` — the prefix
    /// difference is deliberate: `dispatch_shipped` is the operator's
    /// physical-shipping audit row, `invoice.staged` is the billing
    /// strand's pre-allocation audit row.
    ///
    /// F12 four-edit ritual fires once.
    InvoiceStaged,

    /// S239 / PR-233 — a pre-allocation invoice draft was deleted by
    /// the operator. Distinct from `InvoiceStaged` (which fires at
    /// creation) and from `InvoiceMarkedAbandoned` (which fires for an
    /// already-allocated `invoice` row stuck in an in-flight NAV state
    /// per PR-8 / ADR-0009 §"Operator unblock"). The deletion event
    /// closes the audit gap S237 §🟡 #13 surfaced: a draft can be
    /// removed from `invoice_draft` but pre-PR-233 the audit ledger
    /// recorded nothing — `InvoiceStaged`-without-downstream was the
    /// only "deleted" signal, which is insufficient for forensic
    /// "who deleted which draft when" queries.
    ///
    /// Carries `draft_id` (`drf_<ULID>`), `tenant_id`, `partner_id`,
    /// the optional `source_dispatch_id` (Some(_) when the deleted
    /// draft was spawned by a dispatch — the `dispatches.spawned_invoice_id`
    /// pointer is NULLed in the same transaction per the S237 §🔴 #1
    /// fix), the `actor` string, and the F8 idempotency key
    /// (`draft_delete:<drf_id>` — unique by construction since a
    /// `drf_<ULID>` can be deleted at most once).
    ///
    /// `invoice.` prefix family because the entry is keyed by a
    /// `drf_<ULID>` id; same per-OUTGOING-invoice export bundle
    /// exclusion rationale as `InvoiceStaged` (drafts have no
    /// `inv_<ULID>` so the bundle's id-filter never matches a draft
    /// deletion row).
    ///
    /// F12 four-edit ritual fires once.
    InvoiceDraftDeleted,

    /// S255 / PR-244 — operator clicked "Create draft invoice" on a
    /// quote staged by [[quote-intake-crate-s210]]. Distinct from
    /// `InvoiceStaged` (which also fires from `create_draft_in_tx` in
    /// the same transaction): this is the QUOTE-side event whose
    /// payload anchors the pickup chain at the `quote_id` key so the
    /// idempotency walk + the "→ Draft #N" SPA link can answer "has
    /// this quote been picked up yet?" without joining on the
    /// `invoice_draft` table. `InvoiceStaged` answers "what does this
    /// draft contain"; `InvoicePickedUpFromQuote` answers "which quote
    /// became this draft."
    ///
    /// Carries `quote_id`, `draft_id` (`drf_<ULID>`), `tenant_id`,
    /// `partner_id`, `partner_created` (`true` iff the resolver minted
    /// a fresh `prt_<ULID>` because no existing partner matched on
    /// legal_name+address), the `actor` string, and the F8 idempotency
    /// key (`quote_pickup:<quote_id>` — unique per quote, regardless
    /// of how many drafts a re-pickup after S239 delete produces; the
    /// per-pickup uniqueness rides on a `:retry<N>` suffix the route
    /// appends when the prior draft is gone).
    ///
    /// `invoice.` prefix family. The audit walker keys the dedup on
    /// `quote_id` because that is the durable identifier; `draft_id`
    /// is the consequence, not the cause.
    ///
    /// F12 four-edit ritual fires once.
    InvoicePickedUpFromQuote,

    /// S256 / PR-245 — the quote-intake daemon completed one poll cycle
    /// AND wrote an audit entry for it **regardless of outcome**. This
    /// is the v2 per-cycle heartbeat kind: it supersedes the
    /// conditional [`EventKind::QuoteIntakePollCompleted`] (S210), which
    /// only fired when `fetched > 0 || failure || error` and so left the
    /// Settings → Quote Intake panel reading "No daemon cycle has
    /// emitted an audit entry yet" on a healthy-but-idle daemon. Per the
    /// module-header convention ("bumping a payload schema renames the
    /// kind; the old kind remains valid for historical entries") this is
    /// a clean schema-version bump — `QuoteIntakePollCompleted` is
    /// retained for parsing pre-S256 rows but no longer emitted.
    ///
    /// Payload (`aberp_quote_intake::QuoteIntakePollPayload`, unchanged)
    /// carries the idempotency key, cycle trigger, counts, `elapsed_ms`,
    /// and an optional `error`. `system.` prefix — never sweeps a
    /// per-OUTGOING-invoice export bundle.
    ///
    /// F12 four-edit ritual fires once.
    QuoteIntakePollAttempted,

    /// S256 / PR-245 — one approved quote was freshly staged into
    /// `quote_intake_log` during a poll cycle. ONE entry per quote
    /// ingested (NOT per cycle). Carries the customer's source-of-truth
    /// `quote_id` (the reference UUID from the storefront `metadata`) so
    /// an arrival is traceable end-to-end, plus the minted `invoice_id`
    /// and the `intake_at` timestamp. This is the read-side signal the
    /// SPA badge + arrival toast key on: the un-picked-up count
    /// increments when one of these lands.
    ///
    /// Payload (`aberp_quote_intake::QuoteIntakeRowAddedPayload`).
    /// `system.` prefix — a staging event for a sister-service quote,
    /// not a regulated invoice-lifecycle entry; never sweeps a
    /// per-OUTGOING-invoice export bundle.
    ///
    /// F12 four-edit ritual fires once.
    QuoteIntakeRowAdded,

    /// S256 / PR-245 — a poll cycle aborted because the storefront HTTP
    /// call failed (transport error / non-2xx). Distinct from
    /// [`EventKind::QuoteIntakePollAttempted`] (which still fires for the
    /// same cycle, carrying the free-text `error`): THIS entry carries a
    /// **structured, closed-vocab** `reason`
    /// (`aberp_quote_intake::PollFailureReason`) so the failure can be
    /// dashboarded by class later without parsing free text. A 401
    /// `unauthorized` reason additionally tells the operator-facing
    /// Settings panel to surface the "re-paste bearer" prompt (the
    /// daemon pauses rather than hammering a rotated bearer).
    ///
    /// Payload (`aberp_quote_intake::QuoteIntakePollFailedPayload`).
    /// `system.` prefix — never sweeps a per-OUTGOING-invoice export
    /// bundle.
    ///
    /// F12 four-edit ritual fires once.
    QuoteIntakePollFailed,

    /// S257 / PR-246 — an operator added a new MES adapter through the
    /// Settings → Adapters page. The adapter is built from the typed
    /// config, started, registered into the live registry, and
    /// persisted into the tenant `[[mes.adapters]]` TOML slot — this
    /// entry records the durable config (kind / adapter_id /
    /// friendly_name / host / port). `mes.` prefix — manufacturing-
    /// domain configuration, the namespace neighbour of
    /// [`EventKind::MesAdapterEvent`]; never sweeps a per-OUTGOING-
    /// invoice export bundle.
    ///
    /// Payload: `AdapterConfigAuditPayload` (aberp binary `mes_manager`).
    /// F12 four-edit ritual fires once.
    AdapterAdded,

    /// S257 / PR-246 — an operator edited an existing MES adapter's
    /// host / port / friendly name. The old adapter is stopped +
    /// deregistered and a fresh one started in its place (the hot-
    /// restart cycle); the TOML slot is rewritten. Carries the NEW
    /// durable config. `mes.` prefix — see [`EventKind::AdapterAdded`].
    ///
    /// Payload: `AdapterConfigAuditPayload` (aberp binary `mes_manager`).
    /// F12 four-edit ritual fires once.
    AdapterUpdated,

    /// S257 / PR-246 — an operator deleted an MES adapter. The adapter
    /// is stopped + deregistered and its `[[mes.adapters]]` entry
    /// removed. Carries the removed adapter's last durable config so
    /// the deletion is reconstructable from the ledger. `mes.` prefix
    /// — see [`EventKind::AdapterAdded`].
    ///
    /// Payload: `AdapterConfigAuditPayload` (aberp binary `mes_manager`).
    /// F12 four-edit ritual fires once.
    AdapterRemoved,

    /// S258 / PR-247 — a registered MES adapter changed health state
    /// (e.g. `healthy → unhealthy` when a CNC's MTConnect agent stops
    /// responding). Detected at the Workshop dashboard's poll cadence by
    /// diffing the live `AdapterRegistry` health against an in-memory
    /// per-adapter baseline; the FIRST sight of an adapter after process
    /// boot seeds the baseline silently (boot-grace — a restart never
    /// re-emits an already-degraded adapter), so every entry here records
    /// a genuine in-session transition. The durable record lets the wall-
    /// TV SPA recover "when did this adapter start alerting" across page
    /// reloads (the chime's high-water-mark) rather than from in-memory
    /// JS state. `mes.` prefix — manufacturing-domain runtime telemetry,
    /// the namespace neighbour of [`EventKind::MesAdapterEvent`]; never
    /// sweeps a per-OUTGOING-invoice export bundle.
    ///
    /// Payload: `AdapterHealthTransitionPayload` (aberp binary `serve`) —
    /// `{ adapter_id, from_state, to_state, ts }` where the states are the
    /// closed wire-vocab health strings (`healthy`/`degraded`/`unhealthy`/
    /// `starting`/`stopped`).
    /// F12 four-edit ritual fires once.
    AdapterHealthTransitioned,

    /// S266 / PR-255 — an operator created, edited, or deleted a row in
    /// the `quoting_materials` tunable catalogue (the auto-quoting
    /// strand's first DB-backed tuning table; design doc §11). Carries
    /// the CRUD `op` (`create`/`update`/`delete`), the `grade` key, and a
    /// JSON snapshot of the row's durable fields so the change is
    /// reconstructable from the ledger (same per-row-history posture the
    /// seller.toml writers use). FIRST member of the new `quote.*` prefix
    /// family (design doc Appendix). Not invoice-scoped, never carries
    /// NAV bytes, never sweeps a per-OUTGOING-invoice bundle.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    MaterialCatalogueChanged,

    /// S266 / PR-255 — one outbound push attempt of the active material
    /// catalogue to the storefront (`PUT /api/catalogue/materials`;
    /// design doc §4 / §14-C). Emitted per attempt with its outcome
    /// (`ok`/`unauthorized`/`transport`/`unexpected_status`), the pushed
    /// row count, and the trigger (`daemon`/`on_write`). The audit trail
    /// is how the Settings surface learns a 401 paused the daemon
    /// ("re-paste bearer"). `quote.*` prefix family. Not invoice-scoped,
    /// no NAV bytes, never sweeps a per-OUTGOING-invoice bundle.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    MaterialCataloguePushed,

    /// S267 / PR-256 — an operator created, edited, or deleted a row in
    /// the `quoting_complexity_rules` tunable table (the auto-quoting
    /// engine's feature-time / size-bucket rules; design doc §11).
    /// Carries the CRUD `op`, the composite-key fields (`feature_type`,
    /// `size_bucket`, `count_min`), and a JSON snapshot of the row.
    /// `quote.*` prefix family (auto-quoting catalogue / tunables).
    /// Not invoice-scoped, never NAV bytes, never sweeps a per-
    /// OUTGOING-invoice bundle.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    ComplexityRulesChanged,

    /// S267 / PR-256 — an operator created, edited, or deleted a row in
    /// the `quoting_tolerance_multipliers` tunable table (per-tolerance-
    /// range multiplier on machining time + per-feature inspection
    /// minutes; design doc §11). Carries the CRUD `op`, the
    /// `tolerance_range` PK, and a JSON snapshot. `quote.*` prefix
    /// family. Not invoice-scoped, never NAV bytes, never sweeps a per-
    /// OUTGOING-invoice bundle.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    ToleranceMultipliersChanged,

    /// S267 / PR-256 — an operator updated the `quoting_parameters`
    /// singleton row (global knobs: scrap, margin, overhead, setup
    /// amortization, min margin, exotic-material tax; design doc §11
    /// / §6 PO-gate knobs land in a later session). Carries the JSON
    /// snapshot. `quote.*` prefix family (no CRUD `op` — there's only
    /// `update`; the singleton is created at boot by `ensure_schema`).
    /// Not invoice-scoped, never NAV bytes, never sweeps a per-
    /// OUTGOING-invoice bundle.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    ParametersChanged,

    /// S267 / PR-256 — an operator created, edited, or deleted a row in
    /// the `quoting_stock_adjustments` tunable table (per-material ×
    /// per-stock-status signed % price tweak; design doc §11). Carries
    /// the CRUD `op`, the composite-key fields (`grade`, `stock_status`),
    /// and a JSON snapshot. `quote.*` prefix family. Not invoice-scoped,
    /// never NAV bytes, never sweeps a per-OUTGOING-invoice bundle.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    StockAdjustmentsChanged,

    /// S271 / PR-260 — EVE addendum 2 stale-stock guard. The SPA Quotes
    /// list route's recompute pass detected that the material on an
    /// accepted quote has DOWNGRADED stock_status since the quote's
    /// `stock_status_at_accept` snapshot, and the row's persisted
    /// `stock_alert` column transitioned `FALSE → TRUE` in this call.
    /// One entry per transition; sticky — only the operator's REFRESH
    /// token (S272+/PR-261) untriggers the column AND emits a separate
    /// (not-yet-defined) acknowledgement event later.
    ///
    /// Carries the `quote_id`, the `material_grade` PK, the
    /// closed-vocab `snapshot_status` (the value of
    /// `quote_intake_log.stock_status_at_accept` at the moment of
    /// acceptance), and the closed-vocab `current_status` (the live
    /// `quoting_materials.stock_status` for the grade at the moment of
    /// detection). A future operator looking at the audit trail can
    /// reconstruct the WHY of an alert without re-deriving from
    /// catalogue history.
    ///
    /// `quote.*` prefix family alongside the S266/S267 catalogue +
    /// tunables kinds; not invoice-scoped, never carries NAV bytes,
    /// never sweeps a per-OUTGOING-invoice bundle. The audit emit
    /// fires from the SPA list route (`handle_list_quote_intake` in
    /// `apps/aberp/src/serve.rs`), which makes the alert recoverable
    /// from the ledger even if the SPA is closed before the operator
    /// sees the banner.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    QuoteStockAlertTriggered,

    /// S272 / PR-261 — DEAL saga top-level event, written ONCE per
    /// committed DEAL on a `quote_intake_log` row. Anchors the saga
    /// chain (`QuoteSalesOrderCreated` + `QuoteWorkOrderCreated` ride
    /// the same DB transaction so the audit trail is atomic). The
    /// row's `deal_issued_at` column flipped `NULL → Some(ts)` in this
    /// tx — see `apps/aberp/src/quote_deal.rs` and the CAS guard in
    /// `aberp_quote_intake::log_table::mark_deal_issued_in_tx`.
    ///
    /// Carries `quote_id`, `tenant_id`, `sales_order_id`,
    /// `work_order_id`, `deal_token` (the first 8 chars of `quote_id`
    /// the operator typed — kept verbatim for forensic walks),
    /// `refresh_acknowledged` (a bool surfacing whether the operator
    /// consumed an EVE-addendum-2 REFRESH token), `actor`, and
    /// `idempotency_key`. The saga refuses to deal an already-dealt
    /// row (CAS returns 0 rows-updated → 409 `deal_already_issued`),
    /// so re-running the saga on a sticky-TRUE `deal_issued_at` does
    /// NOT re-emit this kind.
    ///
    /// `quote.*` prefix family alongside catalogue, tunables, and
    /// `QuoteStockAlertTriggered`; not invoice-scoped, never NAV
    /// bytes, never swept by the per-OUTGOING-invoice bundle.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    QuoteDealIssued,

    /// S272 / PR-261 — Sales Order placeholder minted by the DEAL
    /// saga in the same tx as [`EventKind::QuoteDealIssued`]. Per
    /// brief pushback #1 the full SO module is named-deferred (no SO
    /// table, no SO CRUD surface); the saga emits this kind so the
    /// audit trail records the `so_<ULID>` against the quote, and a
    /// future SO module's backfill can adopt these audit entries as
    /// its retroactive source of truth.
    ///
    /// Carries `quote_id`, `sales_order_id` (the `so_<ULID>` minted
    /// in-tx), `tenant_id`, and `actor`.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    QuoteSalesOrderCreated,

    /// S272 / PR-261 — Work Order placeholder minted by the DEAL saga
    /// in the same tx as [`EventKind::QuoteDealIssued`]. The
    /// `aberp-work-orders` crate (PR-228) requires `product_id` plus
    /// at least one routing op; the quote intake row carries neither
    /// at this stage of the auto-quoting pipeline, so the saga mints
    /// a `wo_<ULID>` placeholder + emits this kind without inserting
    /// a `work_orders` row. A future cut that plumbs CAD-extracted
    /// product+routing into the quote pipeline can adopt these
    /// placeholders into real WOs.
    ///
    /// Carries `quote_id`, `work_order_id` (the `wo_<ULID>` minted
    /// in-tx), `tenant_id`, and `actor`.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    QuoteWorkOrderCreated,

    /// S273 / PR-262 / ADR-0069 — material moved into `reserved` state
    /// (soft commit). Reserved for the future "indicative quote →
    /// reserve" hook the storefront will trigger when an operator marks
    /// a quote as "high-confidence"; not emitted by any handler today
    /// (the DEAL saga commits directly to `committed`, not via a
    /// `reserved` intermediate). The kind is defined now so the
    /// `inventory.*` prefix family lands as one F12 ritual + one schema
    /// audit, rather than being trickled in across multiple PRs.
    ///
    /// Carries `material_grade`, `tenant_id`, `qty`, `reservation_id`
    /// (the `res_<ULID>`), `quote_id`, `actor`.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    MaterialReserved,

    /// S273 / PR-262 / ADR-0069 — material moved into `committed` state
    /// (hard commit). Emitted by the DEAL saga inside its single tx (the
    /// fourth audit entry alongside `QuoteDealIssued` +
    /// `QuoteSalesOrderCreated` + `QuoteWorkOrderCreated`); represents
    /// the customer-paying transition where `committed_qty` increments
    /// on `inventory_balances` and a new `inventory_reservations` row
    /// lands with `state = 'committed'`.
    ///
    /// Carries `material_grade`, `tenant_id`, `qty`, `reservation_id`
    /// (the `res_<ULID>`), `quote_id`, `actor`, the on-hand /
    /// reserved / committed snapshot AFTER the increment (so a
    /// forensic walk can prove the invariant held), and `idempotency_key`
    /// (`quote_deal:<quote_id>:material`).
    ///
    /// `inventory.*` prefix family — distinct from `quote.*` (catalogue,
    /// tunables, saga) and `mes.*` (product-side stock_movements). Never
    /// invoice-scoped; never swept by the per-OUTGOING-invoice bundle.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    MaterialCommitted,

    /// S273 / PR-262 / ADR-0069 — material moved into `consumed` state
    /// (physically used in production). Reserved for the future workshop-
    /// completion hook the Stage 3 Production module will fire when a
    /// Work Order Completes and the material is physically off the
    /// shelf. Not emitted by any handler today.
    ///
    /// Carries `material_grade`, `tenant_id`, `qty`, `reservation_id`,
    /// `quote_id`, `actor`.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    MaterialConsumed,

    /// S273 / PR-262 / ADR-0069 — reservation released (back to the
    /// sale-able pool). Reserved for the future "quote rejected" /
    /// "DEAL rolled back" hook — the operator-driven path that
    /// decrements `reserved_qty` or `committed_qty` and flips the
    /// reservation row's `state` to `released`. Not emitted by any
    /// handler today.
    ///
    /// Carries `material_grade`, `tenant_id`, `qty`, `reservation_id`,
    /// `quote_id`, `actor`, `reason` (operator-typed text).
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    MaterialReleased,

    /// S279 / PR-265 — first audit row of the pricing pipeline. Emitted
    /// when the ABERP pricing daemon has pulled a `received`-state quote
    /// off the storefront and staged a `quote_pricing_jobs` row in state
    /// `Fetched`. One entry per pulled quote (idempotent via the
    /// `quote_pricing_jobs.quote_id` PK — a re-fetch on an existing row
    /// is a no-op and emits no audit).
    ///
    /// Carries `quote_id` (the storefront UUID), `tenant_id`, `cad_path`
    /// (local tmp path under `quote-artifacts/`), `material_grade`, the
    /// requested `quantity`, the customer email (for forensic-walk
    /// "which customer asked for this price"), `actor` (`system` —
    /// daemon-driven), and the F8 idempotency key.
    ///
    /// `quote.*` prefix family alongside the other auto-quoting kinds.
    /// Pushback against the brief's `quote.pricing.*` three-segment
    /// shape: codebase convention is `prefix.snake_case_name` (one
    /// dot); keeping six kinds in the same shape avoids forking the
    /// audit-string grammar mid-stream.
    ///
    /// Payload: `serde_json::Value` (aberp binary `serve`).
    /// F12 four-edit ritual fires once.
    QuotePricingFetched,

    /// S279 / PR-265 — pricing-pipeline FeatureGraph extracted from
    /// CAD. Emitted when `aberp-cad-extract-wrapper` returned `Ok(_)`
    /// and the `quote_pricing_jobs` row moved `Fetched → Extracting →
    /// Pricing`. Carries `quote_id`, `tenant_id`, `extractor_version`
    /// (from `aberp_cad_extract_wrapper::WRAPPER_VERSION`), the
    /// `feature_graph_hash` (blake3 of the canonical JSON — the
    /// idempotency key against the storefront's priced-writeback
    /// per ADR-0004), and the bounding-box / volume snapshot so a
    /// forensic walk can reconstruct what the engine saw.
    ///
    /// `quote.*` prefix family.
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingExtracted,

    /// S279 / PR-265 — pricing-pipeline QuoteBreakdown produced.
    /// Emitted when `aberp_quote_engine::quote()` returned `Ok(_)` and
    /// the row moved `Pricing → Rendering`. Carries `quote_id`,
    /// `tenant_id`, `engine_version`, `total_price_eur`,
    /// `material_cost_eur`, `machining_cost_eur` (pre-S418 rows:
    /// `labor_cost_eur`), `cad_cam_cost_eur` (S418+), `setup_cost_eur`,
    /// `overhead_eur`, `margin_eur` — every number on the breakdown
    /// (NOT the reasoning_log, which would bloat the ledger; the log
    /// is persisted on the job row's `breakdown_json` column).
    ///
    /// `quote.*` prefix family.
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingPriced,

    /// S279 / PR-265 — pricing-pipeline indicative PDF rendered.
    /// Emitted when `aberp_quote_pdf::render` produced bytes and the
    /// row moved `Rendering → PostingBack`. Carries `quote_id`,
    /// `tenant_id`, `pdf_path` (under `quote-artifacts/<id>/`),
    /// `pdf_size_bytes`, and `pdf_renderer_version`.
    ///
    /// `quote.*` prefix family.
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingRendered,

    /// S279 / PR-265 — pricing-pipeline priced-writeback POST succeeded.
    /// Emitted when `POST /api/quotes/{id}/priced` returned 200 (per
    /// ADR-0004) and the row moved `PostingBack → Posted`. Carries
    /// `quote_id`, `tenant_id`, the canonical `feature_graph_hash`
    /// (echoed for forensic reconciliation against the storefront
    /// `metadata.json.pricing.feature_graph_hash`), an `idempotent`
    /// boolean (true on the storefront's `{ status: "quoted",
    /// idempotent: true }` replay-success shape), the `valid_until`
    /// date stamped on the writeback, and the F8 idempotency key.
    ///
    /// `quote.*` prefix family.
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingPosted,

    /// S279 / PR-265 — pricing-pipeline job failed at any stage. Emitted
    /// when the state machine moves into `Failed`. ONE entry per failure
    /// transition — operator retry on a Failed row re-emits a fresh
    /// `QuotePricingFetched` and the failure history is the audit chain,
    /// not the row.
    ///
    /// Carries `quote_id`, `tenant_id`, `stage` (closed-vocab string —
    /// `"fetch" | "extract" | "price" | "render" | "post"`), `reason`
    /// (operator-readable message, header-injection-safe truncated to
    /// 1000 chars), `actor` (`system` for daemon failures, operator
    /// login for retry-induced failures), and the F8 idempotency key
    /// (`quote_pricing_failed:<quote_id>:<attempt_n>` — `attempt_n`
    /// counts retries so re-failures don't UNIQUE-collide).
    ///
    /// `quote.*` prefix family.
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingFailed,

    /// S281 / PR-266 — storefront-side email-relay request was accepted
    /// by ABERP's `POST /api/internal/send-email` endpoint (per
    /// ADR-0007) and persisted to the `outbound_email_queue` table for
    /// the background drain to send. ONE entry per accepted request
    /// (validation passed, body persisted, attachments written to
    /// disk). The 200 response carries this entry's id back to the
    /// storefront as `audit_id`.
    ///
    /// Carries `submitter` (the token-identified caller — typically
    /// `"storefront"`), `queue_row_id` (UUID of the persisted row),
    /// `recipient_hash` (SHA-256 of the comma-joined `to`-list — full
    /// addresses are NEVER persisted to the audit ledger per ADR-0007
    /// §Audit; the hash lets a forensic walker answer "did we ever
    /// relay to this person at all?" without retaining PII),
    /// `subject` (kept plaintext — operator-visible by design, not
    /// PII-sensitive in our domain), and `byte_size` (rendered message
    /// + attachments).
    ///
    /// **NOT `invoice.`-prefixed.** The relay is sister-service
    /// telemetry, not an outgoing-invoice surface — same posture as
    /// `QuoteIntakePollCompleted`. The `email.*` prefix opens a new
    /// family because the existing `invoice.emailed_sent` is keyed by
    /// `invoice_id` and lives inside the outgoing-invoice export
    /// bundle, whereas an email relay carries no invoice id at all (a
    /// "quote ready" email is a quote-side artefact, not a regulated
    /// invoice). The `email.*` prefix keeps the per-OUTGOING-invoice
    /// `invoice.*` glob from sweeping these rows.
    ///
    /// F12 four-edit ritual fires once for the three sibling kinds.
    EmailRelayQueued,

    /// S281 / PR-266 — outbound email queue row succeeded on the SMTP
    /// transport. Emitted by the background drain after `lettre`'s
    /// `transport.send` returned `Ok(_)` and the row moved
    /// `Sending → Sent`. ONE entry per successful send (re-sends after
    /// SMTP-flake retries fire as one final `EmailRelaySent` for the
    /// terminal success; the retry trail is the `attempt_n` field on
    /// the queue row, not separate audit rows per attempt).
    ///
    /// Carries `submitter`, `queue_row_id`, `recipient_hash`,
    /// `subject`, `byte_size`, and `attempt_n` (1-based; 1 means
    /// "first try succeeded"). NO recipient plaintext, NO body bytes —
    /// same GDPR-minimisation posture as [`EmailRelayQueued`].
    ///
    /// `email.*` prefix family.
    EmailRelaySent,

    /// S281 / PR-266 — outbound email queue row exhausted the retry
    /// budget. Emitted when `attempt_n` reaches the retry cap (5 per
    /// the brief) without a successful SMTP send; the row moves
    /// `Sending → Failed` and stays there until operator action.
    /// ONE entry per terminal failure (not per failed attempt).
    ///
    /// Carries `submitter`, `queue_row_id`, `recipient_hash`,
    /// `subject`, `byte_size`, `attempt_n`, and `last_error` (the
    /// scrubbed-of-secrets detail from the final
    /// `EmailSendError::scrubbed_detail()` — same posture as
    /// `invoice.emailed_sent`'s `error_detail`).
    ///
    /// `email.*` prefix family.
    EmailRelayFailed,

    /// S282 / PR-267 — pricing-pipeline daemon resolved (or failed to
    /// resolve) its Python interpreter at spawn time. Emitted ONCE per
    /// daemon spawn — the audit-trail "code can never silently be wrong
    /// about the venv" guarantee per [[trust-code-not-operator]]. A
    /// forensic walker can answer "did this install ever come up with a
    /// working pipeline, and which fallback layer did it land on?"
    /// without combing through logs.
    ///
    /// Carries `resolution_kind` (closed-vocab string — `"env_override"
    /// | "project_venv" | "alt_venv" | "system_python" | "not_resolved"`,
    /// matching the [`PythonResolution`] enum), `resolved_path` (the
    /// absolute path the daemon will exec, or `null` on `not_resolved`),
    /// and `module_importable` (true iff `python -c "import
    /// aberp_cad_extract"` exited 0 at resolve time).
    ///
    /// `quote.*` prefix family (same family as the other pricing-
    /// pipeline kinds — keeps a forensic query for "everything the
    /// pricing daemon did on this install" inside a single prefix).
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    PipelinePythonResolved,

    /// S286 / PR-268 — pricing-pipeline daemon supervisor caught a Rust-side
    /// panic during a `poll_once` iteration. The supervisor restarted the
    /// daemon (after a 30s back-off) so the rest of ABERP stays alive; this
    /// audit row is the durable forensic record of the recovery.
    ///
    /// Carries `panic_msg` (sanitized — CR/LF/NUL stripped, truncated to 1000
    /// chars; the raw panic payload is whatever `std::panic::set_hook` would
    /// have printed), `restart_count_since_boot` (so a forensic walker can
    /// see "how many times has this daemon restarted in this process
    /// lifetime"), `last_known_quote_id` (the row the daemon was advancing
    /// when it panicked, `null` if not available), and `idempotency_key`
    /// (`quote_pricing_daemon_panicked:<ULID>` — every panic is a fresh row,
    /// so each restart gets its own ULID rather than colliding).
    ///
    /// Caveat — a *C++-level* `libc++abi` termination (e.g. DuckDB FATAL
    /// exception) bypasses Rust's panic machinery entirely and CANNOT be
    /// caught by this supervisor; the process exits. This kind covers the
    /// Rust-panic path only. Defence-in-depth against the C++ class is the
    /// SELECT-first pattern in [`crate::quote_pricing_jobs`] —
    /// [[trust-code-not-operator]].
    ///
    /// `quote.*` prefix family (same family as the other pricing-pipeline
    /// kinds — keeps the forensic-query glob "everything the pricing
    /// pipeline did" inside one prefix).
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingDaemonPanicked,

    /// S288 / PR-269 — one-shot boot-time migration row recording that the
    /// orphan `quote_pricing_jobs_tenant_state_idx` secondary index was
    /// detected on an existing prod DB and dropped. Fires once per upgrade
    /// (the first boot whose `migrate_secondary_index_with_report` returns
    /// `was_present = true`); on subsequent boots the index is gone and no
    /// row fires. Fresh DBs (post-PR-268) never had the index and never
    /// emit.
    ///
    /// The migration's *correctness* is defended by SCHEMA_SQL's
    /// `DROP INDEX IF EXISTS` + a forensic test (see [[s288-redrop-prong]]).
    /// This audit row is the *operator-visible* evidence — a forensic
    /// walker grepping `quote.*` for "what did the pricing pipeline do
    /// on this install" sees the migration row alongside the daemon
    /// resolution, panic, and per-job rows. Without it the migration is
    /// silent (CLAUDE.md rule 12 — "fail loud", or in this case
    /// "succeed loud").
    ///
    /// Carries `tenant_id`, `index_name` (verbatim so future renames are
    /// traceable), `dropped_at` (ISO-8601 UTC), `actor` (`"system"`),
    /// and `idempotency_key` (`quote_pricing_jobs_index_migrated:<tenant>`
    /// — one row per tenant per install, ever; subsequent boots find
    /// the index absent and don't fire, so the UNIQUE constraint is
    /// defence-in-depth, not the primary idempotency guarantee).
    ///
    /// `quote.*` prefix family (same family as the other pricing-pipeline
    /// kinds — keeps the forensic-query glob "everything the pricing
    /// pipeline did" inside one prefix).
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingJobsIndexMigrated,

    /// S290 / PR-271 — companion to [`Self::QuotePricingFailed`]; records the
    /// classifier's verdict (Transient / Permanent / Unknown) for the same
    /// failure transition. Lets the operator audit-ledger-walk all Permanent
    /// failures for review without parsing free-text `reason` strings.
    ///
    /// Emitted alongside `QuotePricingFailed` from the pipeline's failure
    /// path; one row per per-attempt failure. The `Failed` audit row carries
    /// the (stage, reason, attempt_n) triple; this row carries the classified
    /// `failure_kind` so the daemon's "should I auto-retry?" decision is
    /// itself auditable.
    ///
    /// Why a separate kind rather than extending the failed-payload — the
    /// classification is a *daemon decision*, not part of the stage-failure
    /// fact; folding the kind into the `Failed` payload would conflate
    /// "what broke" with "what we decided to do about it" and break the
    /// per-kind forensic glob ("show me every classification decision the
    /// daemon ever made").
    ///
    /// Carries `quote_id`, `tenant_id`, `failure_kind` (closed-vocab
    /// `transient`/`permanent`/`unknown`), `last_error`, `attempt_n`,
    /// `actor` (`"system"`), `idempotency_key`
    /// (`quote_pricing_failure_classified:<quote_id>:<attempt_n>` — one
    /// row per (quote, attempt) pair; subsequent re-failures collide at the
    /// audit-ledger's UNIQUE defence, matching the `QuotePricingFailed`
    /// idempotency posture).
    ///
    /// `quote.*` prefix family (keeps the forensic-glob "everything the
    /// pricing pipeline did" intact).
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingFailureClassified,

    /// S347 / PR-39 (F1+F2) — the per-attempt transport-vs-app verdict for a
    /// single priced-writeback POST against the storefront. Distinct from
    /// [`Self::QuotePricingFailed`]/[`Self::QuotePricingFailureClassified`]
    /// (which are job-level, coarse `transient`/`permanent`/`unknown`): this
    /// row carries the *granular* outcome — `routing_misconfigured`,
    /// `unauthorized`, `non_json_response`, `app_errored`, `timeout`, … —
    /// plus the structured `http_status` + `content_type` + `body_excerpt`
    /// so an operator can tell a CDN misroute (HTML-as-JSON, the 2026-06-11
    /// incident) from an auth failure from a real 5xx without parsing the
    /// free-text reason. Emitted on EVERY writeback attempt (success too)
    /// so the forensic walker sees the full delivery history.
    ///
    /// Carries `quote_id`, `tenant_id`, `outcome` (closed-vocab tag),
    /// `http_status` (nullable — transport failures never reached a
    /// response), `content_type` (nullable), `body_excerpt` (nullable,
    /// bearer-scrubbed, ≤200 chars), `retryable`, `attempt_n`, `actor`
    /// (`"system"`), `idempotency_key`
    /// (`quote_priced_writeback_outcome:<quote_id>:<attempt_n>`).
    ///
    /// `quote.*` prefix family. Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricedWritebackOutcome,

    /// S348 / PR-39 (F1) — the typed transport-vs-app verdict for ONE
    /// `GET /api/quotes?status=received` storefront list-poll cycle in the
    /// pricing pipeline. The S347 Content-Type gate, extended to the poll
    /// site: a 200 `text/html` (CDN serving the SPA shell instead of the
    /// quotes API — the same misroute that produced the 2026-06-11 incident)
    /// is now classified `routing_misconfigured` instead of crashing the
    /// `resp.json()` parse and aborting the cycle with an opaque reason.
    /// Reuses [`WritebackOutcome`]'s closed-vocab tags. Emitted ONLY on a
    /// failed poll (never on a healthy cycle) to avoid the idle-audit-spam
    /// that S335 throttled for `EmailOutboxFetched`.
    ///
    /// Carries `tenant_id`, `outcome` (closed-vocab tag), `http_status`
    /// (nullable), `content_type` (nullable), `body_excerpt` (nullable,
    /// bearer-scrubbed, ≤200 chars), `retryable`, `actor` (`"system"`),
    /// `idempotency_key` (`quote_poll_outcome:<ulid>`). No `quote_id` — a
    /// list poll spans many quotes.
    ///
    /// `quote.*` prefix family. Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePollOutcome,

    /// S350 / PR-39 (U5) — operator inline-edit of a pricing job's
    /// `material_grade`. Closes the F4/U5 dead-end: a row stuck
    /// `Permanent` on `material grade ... is not in the catalogue
    /// snapshot` (default form material `unknown`, legacy grades) had
    /// NO code-level remedy short of the customer re-submitting through
    /// the storefront. When an operator clarifies the grade by phone,
    /// this is the audit-of-record for applying it: the row's grade is
    /// rewritten to a catalogue grade, the job reset to `Fetched`
    /// (re-enters pricing), and `attempt_n` bumped.
    ///
    /// Carries `quote_id`, `tenant_id`, `old_grade`, `new_grade`,
    /// `previous_state` (the JobState the row was in when the operator
    /// edited it), `attempt_n` (post-bump), `operator_user_id` (the
    /// Bearer-subject operator login — who applied the override), `actor`
    /// (the same login, for the ledger actor field), `idempotency_key`
    /// (`quote_material_grade_edited:<quote_id>:<attempt_n>` — the bumped
    /// attempt disambiguates repeat edits the way `QuotePricingFailed`'s
    /// suffix does).
    ///
    /// `quote.*` prefix family. Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingMaterialEdited,

    /// S391/F — operator deletion of a Failed pricing-job row. The
    /// audit-of-record for an operator removing a permanently-Failed row
    /// from the Auto-Quoting panel (e.g. the S379 phantom-retry rows that
    /// can never reach a terminal state). Conservative: the DELETE route
    /// only accepts rows in `Failed` state, so an in-flight job is never
    /// removed out from under the daemon. The row is deleted in the SAME
    /// transaction as this append, so the removal and its audit-of-record
    /// commit atomically; the forensic record survives the row.
    ///
    /// Carries `quote_id`, `tenant_id`, `previous_state` (always
    /// `failed`), `attempt_n` (the row's final attempt counter),
    /// `error_stage` / `error_reason` / `failure_kind` (nullable — the
    /// row's terminal failure context, preserved so a walker sees WHY the
    /// deleted row had failed), `operator_user_id` (the Bearer-subject
    /// operator login who deleted it), `actor` (the same login), and
    /// `idempotency_key` (`quote_pricing_failure_deleted:<quote_id>`).
    ///
    /// `quote.*` prefix family. Payload: `serde_json::Value`. Never
    /// carries NAV XML bytes (app-layer JSON only).
    QuotePricingFailureDeleted,

    /// S354 / PR-42 (U16) — operator accept-on-behalf. The audit-of-record
    /// for marking a quote accepted when the customer accepted
    /// off-channel (phone / e-mail / in person / other) instead of
    /// clicking the DEAL link. Distinct from the customer-owned
    /// typed-ACCEPT path (which the storefront records itself); this is
    /// ABERP's local commitment, emitted on EVERY attempt regardless of
    /// whether the storefront writeback succeeded, so a forensic walker
    /// sees both the successful accept and any failed-sync retries.
    ///
    /// Carries `quote_id`, `tenant_id`, `channel` (closed vocab:
    /// `phone` / `email` / `in_person` / `other`), `note` (operator free
    /// text), `operator_user_id` (the Bearer-subject operator login who
    /// accepted on the customer's behalf), `accepted_at_ms` (the epoch-ms
    /// stamp bound into the HMAC sent to the storefront),
    /// `customer_confirmation_path` (optional operator-supplied path to a
    /// CC screenshot / scanned confirmation), `outcome` (the
    /// `WritebackOutcome` tag of the storefront POST: `success` /
    /// `routing_misconfigured` / `app_rejected` / …), `retry_available`
    /// (`true` for any non-`success` outcome — the storefront state was
    /// not advanced so the operator may re-attempt; `false` once
    /// synced), `writeback_http_status` (nullable),
    /// `writeback_body_excerpt` (nullable, ≤200 chars), `actor`,
    /// `idempotency_key`
    /// (`quote_operator_accepted:<quote_id>:<accepted_at_ms>` — the
    /// per-attempt timestamp disambiguates a retried accept the way the
    /// per-stage `attempt_n` suffix does elsewhere).
    ///
    /// `quote.*` prefix family. Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuotePricingOperatorAccepted,

    /// S403 — operator REFUSE-with-reason. The audit-of-record for an
    /// operator declining to fulfil an accepted auto-quote (the DEAL
    /// step's negative counterpart: when stock / production capacity
    /// can't satisfy the order). Distinct from `QuoteDealIssued` (the
    /// positive path) and from the customer-owned typed-ACCEPT. Refusing
    /// flips the local `quote_intake_log.intake_state` to `refused`
    /// (CAS-guarded, single-use), queues the bilingual customer
    /// notification e-mail in the same tx (atomic per
    /// [[hulye-biztos]]), and best-effort writes back the storefront
    /// `rejected` status. NO draft invoice is staged or issued.
    ///
    /// Carries `quote_id`, `tenant_id`, `reason` (operator free text,
    /// ≥5 chars, validated server-side per [[trust-code-not-operator]]),
    /// `operator_user_id` (the Bearer-subject operator who refused),
    /// `refused_at` (RFC3339), `customer_email_present` (bool — whether
    /// a recipient resolved for the notification e-mail; logged loud so
    /// a missing address surfaces per CLAUDE.md #12), `actor`,
    /// `idempotency_key` (`quote_operator_refused:<quote_id>`).
    ///
    /// `quote.*` prefix family. Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once.
    QuoteOperatorRefused,

    /// S307 / PR-276 — one entry per email-outbox poll cycle. Fires
    /// once per successful `GET /api/internal/email-queue` against the
    /// storefront, regardless of whether the cycle returned 0 entries
    /// or N. The payload's `fetched_count` lets a forensic walker
    /// answer "did this install poll at all during window X, and how
    /// much was in the queue at each cycle?" — the audit-trail analogue
    /// of the SPA's `last_poll_ts` indicator.
    ///
    /// Polling cadence is 5s by default
    /// (`email_outbox_poll_daemon::POLL_TICK_SECS_DEFAULT`).
    ///
    /// **S311 / F13 + F18 widening.** Pre-S311 only successful cycles
    /// landed here. The S309 review found that left silent-401 token-
    /// rotation gaps invisible in the audit ledger — the SPA's volatile
    /// `last_error_detail` was the only signal. Now: every cycle attempt
    /// fires this event. Successful cycles carry `fetched_count: N`
    /// with `error_class`/`error_detail` absent on the wire; errored
    /// cycles carry `fetched_count: 0` with `error_class` (`"auth_failed"
    /// | "network" | "decode" | "other"`) and a scrubbed `error_detail`.
    ///
    /// Carries `fetched_count`, `since_cursor` (the ISO timestamp
    /// passed as `?since=`, or `null` on first fetch), `cycle_at`
    /// (ISO-8601 UTC of the cycle wall-clock; the cycle's own forensic
    /// timestamp, distinct from the audit-ledger insertion timestamp),
    /// and the two optional error fields above.
    ///
    /// `quote.*` prefix family — same family as the pricing-pipeline
    /// kinds so a forensic walker grepping `quote.*` on a prod ledger
    /// surfaces "everything the auto-quoting pipeline did, including
    /// the outbound emails it polled and delivered". The brief
    /// explicitly named this prefix despite the surface being email-
    /// shaped (the email-relay strand uses `email.*` instead — this is
    /// a deliberate split because the outbox flow is part of the auto-
    /// quoting pipeline whereas the S281 relay is sister-service
    /// push from any surface).
    ///
    /// Payload: `serde_json::Value`.
    /// F12 four-edit ritual fires once for the four sibling kinds.
    EmailOutboxFetched,

    /// S307 / PR-276 — one entry per email-outbox entry claim. Fires
    /// after the daemon's `POST /api/internal/email-queue/{id}/claim`
    /// returns `200` (claimed by us). A `409` (already claimed by a
    /// hypothetical concurrent ABERP) does NOT fire this event — the
    /// row stays the other claimer's responsibility. Per ADR-0009 the
    /// single-tenant deployment means contention is overkill defence,
    /// not a frequent path; one row per claim keeps the audit trail
    /// linear.
    ///
    /// Carries `submitter` (the storefront-supplied `submitter` field
    /// from the queue entry — `"submission_received" | "priced_ready" |
    /// "accept_confirmation" | "other"`), `queue_row_id` (the
    /// storefront-side ULID), `recipient_hash` (SHA-256 of the
    /// to ∪ cc list per the email-relay hashing rule — NO plaintext
    /// recipients), `subject` (verbatim), `byte_size` (decoded total).
    ///
    /// `quote.*` prefix family.
    EmailOutboxClaimed,

    /// S307 / PR-276 — terminal success row per outbox entry. Fires after
    /// the SMTP `transport.send` returns `Ok(_)` AND the storefront
    /// write-back POST `/api/internal/email-queue/{id}/sent` returns
    /// `2xx`. If SMTP succeeded but the write-back failed, this event
    /// does NOT fire (the daemon retries the write-back on the next
    /// cycle; the row stays in `claimed/` storefront-side until the
    /// write-back lands — duplicate-send risk is acceptable per
    /// [[trust-code-not-operator]] vs. silent lying about delivery).
    ///
    /// Carries the same per-entry fields as
    /// [`Self::EmailOutboxClaimed`] plus `attempt_n` (always `1` in v1
    /// — the daemon does not retry SMTP within a cycle; a failed SMTP
    /// send moves the row to `failed/` and stays there).
    ///
    /// `quote.*` prefix family.
    EmailOutboxSent,

    /// S307 / PR-276 — terminal failure row per outbox entry. Fires after
    /// the SMTP `transport.send` returns `Err(_)` AND the storefront
    /// write-back POST `/api/internal/email-queue/{id}/failed` returns
    /// `2xx`. v1 ships no SMTP retry within a cycle — one attempt then
    /// terminal-fail per [[trust-code-not-operator]] (a failed SMTP
    /// row is operator-visible in the SPA panel rather than masked by
    /// background retry).
    ///
    /// Carries the same per-entry fields as
    /// [`Self::EmailOutboxClaimed`] plus `error_class` (closed-vocab
    /// short token; `"smtp_transport" | "writeback" | "compose" |
    /// "other"`) and `error_detail` (scrubbed-of-secrets cause string,
    /// ≤ 1000 chars).
    ///
    /// `quote.*` prefix family.
    EmailOutboxFailed,

    /// S325 / PR-25 — EVE addendum-2 customer-PDF re-render ENQUEUED.
    /// Fires read-side when the operator's Quotes-tab load observes a
    /// `quote_intake_log.stock_alert` FALSE → TRUE transition and the
    /// quote-id is pushed into the in-memory
    /// [`crate::quote_pdf_rerender_queue`] (one row per confirmed flip —
    /// the queue's HashSet idempotency + the sticky-TRUE recompute mean a
    /// second operator view does NOT re-enqueue, so this never
    /// double-fires for one transition).
    ///
    /// Carries `quote_id`, `material_grade`, `snapshot_status` (the
    /// stock-status the customer accepted under), `current_status` (the
    /// degraded live catalogue status that tripped the alert).
    ///
    /// `quote.*` prefix family.
    QuotePdfRerenderEnqueued,

    /// S325 / PR-25 — terminal success: the re-render daemon re-rendered
    /// `priced.pdf` with the stock-alert band and the storefront `/priced`
    /// re-post returned a success shape (`{rerendered:true}`,
    /// `{idempotent:true}`, or a `409` treated as already-flipped). The
    /// customer now sees the addendum-2 banner.
    ///
    /// Carries `quote_id`, `feature_graph_hash` (unchanged — the geometry
    /// did not move, only the stock overlay), `outcome` (closed-vocab
    /// `"rerendered" | "idempotent" | "already_flipped_409"`),
    /// `pdf_byte_size`.
    ///
    /// `quote.*` prefix family.
    QuotePdfRerendered,

    /// S325 / PR-25 — the re-render daemon could not deliver the
    /// re-rendered PDF. Carries `quote_id`, `failure_kind` (closed-vocab
    /// `"transient" | "permanent" | "unknown"`, mirroring
    /// [`crate::quote_pricing_jobs::FailureKind`]), `error_class`
    /// (short token: `"http_5xx" | "http_4xx" | "transport" |
    /// "artifacts_missing" | "render" | "other"`) and `error_detail`
    /// (scrubbed cause string, ≤ 1000 chars). Transient failures are
    /// re-enqueued for the next cycle; permanent/unknown drop out of the
    /// queue (fail-loud, not silent) so the operator sees the audit row.
    ///
    /// `quote.*` prefix family.
    QuotePdfRerenderFailed,

    /// S355 / PR-43 (ADR-0073) — a digital identity was registered for an
    /// operator. FIRST member of the new `personnel.*` prefix family — the
    /// defense-grade access-trail strand of the aerospace pivot
    /// (`[[defense-aerospace-pivot]]`). Pairs with the S344
    /// [`aberp_digital_id::DigitalIdProvider`] seam: when a provider mints an
    /// operator's `DigitalId`, this is the audit-of-record that the identity
    /// now exists on this install (the Part-11 / DFARS "who can act" anchor).
    ///
    /// Payload (`serde_json::Value`): `operator_user_id` (the provider-issued
    /// identity id), `provider_name` (which backend issued it — `mock` today,
    /// a real CAC / eID backend later), `registered_at_ms` (epoch-ms stamp of
    /// registration).
    ///
    /// `personnel.*` prefix family — NOT `invoice.*` / `system.*` / `mes.*` /
    /// `quote.*` / `inventory.*` / `email.*`. A new prefix keeps the access-
    /// trail surface segregated so a forensic walker can glob `personnel.*`
    /// for "every identity / signature / access decision on this install"
    /// without sweeping fiscal, manufacturing, or quoting traffic — and the
    /// per-OUTGOING-invoice export bundle's `invoice.*` glob (ADR-0009 §8)
    /// never sweeps a personnel row by construction.
    ///
    /// S355 ships the kind only; firing sites land in S356+. F12 four-edit
    /// ritual fires once for the four sibling kinds.
    PersonnelIdRegistered,

    /// S355 / PR-43 (ADR-0073) — an electronic signature was applied to a
    /// record under a registered digital identity. The Part-11 §11.50
    /// signature-manifestation anchor: a durable, hash-chained record that
    /// operator X signed record Y with algorithm Z at time T. Distinct from
    /// the S344 `Signed<T>` audit-payload wrapper (which carries the signer
    /// reference INLINE on an arbitrary event) — this kind is the standalone
    /// "a signature ceremony happened" landmark a forensic walker can glob.
    ///
    /// Payload (`serde_json::Value`): `operator_user_id` (signer identity),
    /// `signed_record_kind` (what KIND of record was signed — e.g. an
    /// invoice / work-order / inspection discriminator), `signed_record_id`
    /// (the signed record's id), `signature_algorithm` (the load-bearing
    /// algorithm tag — `mock-hmac-sha256` today, a real `ecdsa-p256` /
    /// CAC-PKCS7 later; a verifier checks it before recomputing), and
    /// `signed_at_ms` (epoch-ms stamp bound into the signature).
    ///
    /// `personnel.*` prefix family — same segregation rationale as
    /// [`Self::PersonnelIdRegistered`].
    PersonnelSignatureApplied,

    /// S355 / PR-43 (ADR-0073) — access to a CUI-marked or export-controlled
    /// resource was GRANTED to an operator. The affirmative half of the
    /// access-control audit pair: NIST SP 800-171 AC-3.1.1 ("limit system
    /// access to authorized users") demands a durable record of WHO was let
    /// into WHAT and WHY. Without this row a defense-grade access trail can
    /// only show denials, not the (more sensitive) grants.
    ///
    /// Payload (`serde_json::Value`): `operator_user_id` (who was granted access),
    /// `resource_kind` (the controlled-resource discriminator — e.g. a CUI
    /// document / export-controlled drawing class), `resource_id` (the
    /// specific resource), `granted_by` (the authorizing operator / role —
    /// the two-person-integrity anchor), and `reason` (the operator-supplied
    /// justification — required so a grant is never unexplained).
    ///
    /// `personnel.*` prefix family — same segregation rationale as
    /// [`Self::PersonnelIdRegistered`].
    PersonnelAccessGranted,

    /// S355 / PR-43 (ADR-0073) — access to a CUI-marked or export-controlled
    /// resource was DENIED to an operator. The negative half of the
    /// access-control audit pair (NIST SP 800-171 AC-3.1.1 / AU-3.3.1).
    /// Recording denials loud is the CLAUDE.md rule-12 posture applied to
    /// access control: a silently-swallowed denial (e.g. a foreign-person
    /// export-screening block) is the worst-class failure for a defense
    /// access trail — the audit row makes "we refused, and here's why"
    /// permanent and reviewable.
    ///
    /// Payload (`serde_json::Value`): `operator_user_id` (who was denied),
    /// `resource_kind` (the controlled-resource discriminator), `resource_id`
    /// (the specific resource), and `denied_reason` (the closed-vocab /
    /// operator-readable cause — e.g. `export_screening_failed`,
    /// `insufficient_clearance`, `cui_not_authorized`).
    ///
    /// `personnel.*` prefix family — same segregation rationale as
    /// [`Self::PersonnelIdRegistered`].
    PersonnelAccessDenied,

    /// S357 / PR-44 (ADR-0074) — a material certificate (mill cert / 3.1
    /// CoC / Certificate of Analysis / heat-treatment cert) was attached to
    /// a `quoting_materials` catalogue row. FIRST member of the new
    /// `material.*` prefix family — the material-traceability strand of the
    /// aerospace pivot (`[[defense-aerospace-pivot]]`). This is the AS9100D
    /// §7.5.9 "control of monitoring and measuring equipment" / §8.5.2
    /// identification-and-traceability *record* anchor: a durable,
    /// hash-chained note that a cert document now backs this grade.
    ///
    /// Deliberately a RECORD event (a cert was filed), NOT a state
    /// transition — see ADR-0074 for why it is split from
    /// [`Self::MaterialHeatLotAssigned`]. A grade may accrue many certs over
    /// its life; each attach is its own append-only landmark.
    ///
    /// Payload (`serde_json::Value`): `material_id` (the `quoting_materials`
    /// grade / row key the cert backs), `cert_kind` (the certificate-class
    /// discriminator — e.g. `mill_cert` / `cofa` / `heat_treatment`),
    /// `cert_url` (where the cert document is retained), `attached_at_ms`
    /// (epoch-ms stamp of the attach), `operator_user_id` (who filed
    /// it — the accountability anchor), and an optional `lot_id` (the
    /// specific lot the cert covers, when the attach is lot-scoped rather
    /// than grade-wide).
    ///
    /// `material.*` prefix family — NOT `invoice.*` / `system.*` / `mes.*` /
    /// `quote.*` / `inventory.*` / `email.*` / `personnel.*`. A new prefix
    /// keeps the traceability surface globbable on its own
    /// (`material.*` = "every cert / lot-heat assignment on this install")
    /// without sweeping fiscal, manufacturing, quoting, or access-trail
    /// traffic — and the per-OUTGOING-invoice export bundle's `invoice.*`
    /// glob (ADR-0009 §8) never sweeps a material-traceability row by
    /// construction.
    ///
    /// S357 ships the kind only; firing sites land in a later session. F12
    /// four-edit ritual fires once for the two sibling `material.*` kinds.
    MaterialCertAttached,

    /// S357 / PR-44 (ADR-0074) — a lot + heat was assigned to a material
    /// instance for traceability. The DFARS 252.225-7008-style "specialty
    /// metals" / AS9100D §8.5.2 traceability *state transition*: this
    /// material instance is now bound to mill heat H of lot L (optionally
    /// from a named supplier). Distinct from [`Self::MaterialCertAttached`]
    /// (which merely files a document) — this kind records the load-bearing
    /// identity binding a part's traceability chain resolves through.
    ///
    /// Carries the validated identity types from
    /// [`aberp_compliance::lot_heat`] (`LotId` / `HeatId`); the firing site
    /// (later session) constructs them, so an invalid lot/heat string can
    /// never reach the ledger.
    ///
    /// Payload (`serde_json::Value`): `material_id` (the `quoting_materials`
    /// grade / row key), `lot_id` (validated `LotId` string), `heat_id`
    /// (validated `HeatId` string), an optional `source_supplier` (the AVL
    /// supplier the lot was sourced from, when known), `assigned_at_ms`
    /// (epoch-ms stamp), and `operator_user_id` (who made the
    /// binding).
    ///
    /// `material.*` prefix family — same segregation rationale as
    /// [`Self::MaterialCertAttached`].
    MaterialHeatLotAssigned,

    /// S358 / PR-45 (ADR-0075) — a serial number was assigned to an
    /// individual part. FIRST member of the new `part.*` prefix family — the
    /// per-unit serialization strand of the aerospace pivot
    /// (`[[defense-aerospace-pivot]]`). This is the MIL-STD-130N / DoD 5000.64
    /// "item unique identification" *record* anchor for the serial half: a
    /// durable, hash-chained note that part P now carries serial S.
    ///
    /// Deliberately split from [`Self::PartUidMarked`] — see ADR-0075 for why.
    /// A serial is *assigned* (a logical fact, possibly at order entry or work-
    /// order release) before the UID is *physically marked* on the metal; the
    /// two facts happen at different times, by different operators, and one can
    /// occur without the other. This kind records the assignment only.
    ///
    /// Payload (`serde_json::Value`): `part_id` (the part instance key the
    /// serial belongs to), `serial_number` (the assigned serial), `assigned_at_ms`
    /// (epoch-ms stamp of the assignment), `operator_user_id` (who made
    /// the assignment — the accountability anchor), and optional
    /// `related_invoice_id` / `related_work_order_id` (the fiscal / production
    /// document the serialization was triggered by, when known).
    ///
    /// `part.*` prefix family — NOT `invoice.*` / `system.*` / `mes.*` /
    /// `quote.*` / `inventory.*` / `email.*` / `personnel.*` / `material.*`. A
    /// new prefix keeps the per-unit serialization surface globbable on its own
    /// (`part.*` = "every serial assignment / UID mark on this install")
    /// without sweeping fiscal, manufacturing, quoting, access-trail, or
    /// material-traceability traffic — and the per-OUTGOING-invoice export
    /// bundle's `invoice.*` glob (ADR-0009 §8) never sweeps a part-serialization
    /// row by construction.
    ///
    /// S358 ships the kind only; firing sites land in a later session. F12
    /// four-edit ritual fires once for the two sibling `part.*` kinds.
    PartSerialAssigned,

    /// S358 / PR-45 (ADR-0075) — the MIL-STD-130N Item Unique Identifier (UID)
    /// was physically marked on a part. The *state transition* counterpart to
    /// [`Self::PartSerialAssigned`]: the part now bears its machine-readable
    /// IUID (a Construct-1 or Construct-2 Type-1/Type-2 IRI per MIL-STD-130N),
    /// data-matrix-marked on the metal. Distinct from the serial assignment
    /// (a logical fact) — this kind records that the mark physically exists and
    /// whether it satisfies MIL-STD-130N, the load-bearing fact a DoD-IUID
    /// auditor resolves a part's pedigree through.
    ///
    /// Carries the IRI string built from the validated UID types in
    /// [`aberp_compliance::uid`] (`Iuid` / `IuidConstruct1` / `IuidConstruct2`);
    /// the firing site (later session) constructs them, so a malformed IAC /
    /// enterprise id can never reach the ledger.
    ///
    /// Payload (`serde_json::Value`): `part_id` (the part instance key),
    /// `uid_iri` (the rendered MIL-STD-130N IRI), `uid_construct_code` (the
    /// construct discriminator — e.g. `construct_1` / `construct_2`),
    /// `mil_std_130_compliant` (bool — whether the mark passed the standard's
    /// format gate), `marked_at_ms` (epoch-ms stamp), and `operator_user_id`
    /// (who marked it).
    ///
    /// `part.*` prefix family — same segregation rationale as
    /// [`Self::PartSerialAssigned`].
    PartUidMarked,

    /// S359 / PR-46 (ADR-0076) — a drawing / spec / technical document was
    /// export-classified. FIRST member of the new `export.*` prefix family —
    /// the tenth — the export-control strand of the aerospace pivot
    /// (`[[defense-aerospace-pivot]]`). This is the ITAR (22 CFR §§ 120-130) /
    /// EAR (15 CFR §§ 730-774) jurisdiction *record* anchor: a durable,
    /// hash-chained note that artifact A now carries a determined
    /// classification + jurisdiction.
    ///
    /// The `jurisdiction` value is one of the
    /// [`aberp_compliance::export_control::Jurisdiction`] regimes
    /// (`ITAR` / `EAR` / `EAR99` / `NOT_CONTROLLED` / `UNKNOWN`); the firing
    /// site (later session) renders it through that typed enum so a free-text
    /// regime can never reach the ledger. Mis-classification is a felony, so
    /// the determination itself comes from a licensed classification service —
    /// never inferred here (the `MockExportControlProvider` answers
    /// `NotClassified` until a real customer demands real backends, per
    /// `[[mock-everything-principle]]`).
    ///
    /// Payload (`serde_json::Value`): `entity_kind` (what was classified — e.g.
    /// `"drawing"` / `"spec"` / `"document"`), `entity_id` (the artifact key),
    /// `eccn` (optional — the EAR Commerce-Control-List number when EAR-listed),
    /// `usml_category` (optional — the USML category when ITAR-controlled),
    /// `jurisdiction` (the regime string), `operator_user_id` (who made
    /// the determination — the accountability anchor), and `classified_at_ms`
    /// (epoch-ms stamp).
    ///
    /// `export.*` prefix family — NOT `invoice.*` / `system.*` / `mes.*` /
    /// `quote.*` / `inventory.*` / `email.*` / `personnel.*` / `material.*` /
    /// `part.*`. A new prefix keeps the export-control surface globbable on its
    /// own (`export.*` = "every classification / access-check / shipment on this
    /// install") without sweeping fiscal, manufacturing, quoting, access-trail,
    /// material-traceability, or per-unit-serialization traffic — and the
    /// per-OUTGOING-invoice export bundle's `invoice.*` glob (ADR-0009 §8) never
    /// sweeps an export-control row by construction.
    ///
    /// S359 ships the kind only; firing sites land in a later session (no
    /// drawing/spec workflow exists yet). F12 four-edit ritual fires once for
    /// the three sibling `export.*` kinds.
    ExportClassificationSet,

    /// S359 / PR-46 (ADR-0076) — access to an export-controlled artifact was
    /// checked. The *decision* counterpart to
    /// [`Self::ExportClassificationSet`]: a durable record that operator O
    /// requested artifact A and was granted or denied. ITAR's "U.S. persons
    /// only" deemed-export rule (22 CFR § 120.62) makes the access-decision
    /// trail load-bearing — a foreign-person access to ITAR technical data is
    /// itself an export — so every check is recorded, not just the denials.
    ///
    /// Payload (`serde_json::Value`): `entity_kind` (artifact kind), `entity_id`
    /// (artifact key), `operator_user_id` (who asked), `decision`
    /// (`"granted"` / `"denied"`), `reason` (the rule that drove the verdict),
    /// and `checked_at_ms` (epoch-ms stamp).
    ///
    /// `export.*` prefix family — same segregation rationale as
    /// [`Self::ExportClassificationSet`].
    ExportAccessCheck,

    /// S359 / PR-46 (ADR-0076) — an export-controlled shipment left. The
    /// *physical-export* event of the family: a durable record that controlled
    /// goods crossed to a recipient party / country under a stated
    /// authorization. The shipment record is the artifact a BIS / DDTC auditor
    /// resolves an export's lawfulness through (was a licence / exception
    /// cited; was the destination embargoed).
    ///
    /// Payload (`serde_json::Value`): `shipment_id` (the shipment key),
    /// `exporter_party_id` (the exporter of record), `recipient_party_id` (the
    /// consignee), `recipient_country` (ISO 3166-1 alpha-2 destination),
    /// `ecn_or_authorization` (optional — the licence / licence-exception / ECCN
    /// cited), `shipped_at_ms` (epoch-ms stamp), and `operator_user_id`
    /// (who released the shipment).
    ///
    /// `export.*` prefix family — same segregation rationale as
    /// [`Self::ExportClassificationSet`].
    ExportShipmentLogged,

    /// S360 / PR-47 (ADR-0077) — a CUI marking banner was applied to a
    /// document / drawing / spec. FIRST member of the new `cui.*` prefix
    /// family — the eleventh — the Controlled-Unclassified-Information strand
    /// of the aerospace pivot (`[[defense-aerospace-pivot]]`). This is the
    /// 32 CFR Part 2002 *marking record* anchor: a durable, hash-chained note
    /// that artifact A now carries a determined CUI/classification banner.
    ///
    /// The `cui_marking_str` value is the rendered DoD banner the firing site
    /// (later session) produces through
    /// [`aberp_compliance::cui::CuiMarking::to_banner_str`] — a typed marking
    /// renders the string, so a free-text banner can never reach the ledger.
    ///
    /// Payload (`serde_json::Value`): `entity_kind` (what was marked — e.g.
    /// `"drawing"` / `"spec"` / `"document"`), `entity_id` (the artifact key),
    /// `cui_marking_str` (the rendered banner, e.g. `"CUI//SP-CTI//NOFORN"` —
    /// CTI is a CUI Specified category, so the banner carries the `SP-` prefix),
    /// `operator_user_id` (who applied it — the accountability anchor),
    /// and `applied_at_ms` (epoch-ms stamp).
    ///
    /// `cui.*` prefix family — NOT `invoice.*` / `system.*` / `mes.*` /
    /// `quote.*` / `inventory.*` / `email.*` / `personnel.*` / `material.*` /
    /// `part.*` / `export.*`. A new prefix keeps the CUI-handling surface
    /// globbable on its own (`cui.*` = "every marking / access on this
    /// install") without sweeping fiscal, manufacturing, quoting, access-trail,
    /// material-traceability, per-unit-serialization, or export-control
    /// traffic — and the per-OUTGOING-invoice export bundle's `invoice.*` glob
    /// (ADR-0009 §8) never sweeps a CUI row by construction.
    ///
    /// No PII at rest: the payload records WHICH artifact was marked and WHO
    /// marked it (operator id — an opaque accountability handle), never the
    /// controlled content itself. S360 ships the kind only; firing sites land
    /// in a later session (no document workflow exists yet). F12 four-edit
    /// ritual fires once for the two sibling `cui.*` kinds.
    CuiMarkingApplied,

    /// S360 / PR-47 (ADR-0077) — access to a CUI-marked artifact was checked.
    /// The *decision* counterpart to [`Self::CuiMarkingApplied`]: a durable
    /// record that operator O requested CUI artifact A and was granted or
    /// denied. CUI's "lawful government purpose" basic-handling rule
    /// (32 CFR § 2002.4) makes the access-decision trail load-bearing — an
    /// improper disclosure of CUI is a reportable event — so every check is
    /// recorded, not just the denials (the `personnel.access_*` /
    /// `export.access_check` posture specialised to a CUI-marked artifact).
    ///
    /// Payload (`serde_json::Value`): `entity_kind` (artifact kind), `entity_id`
    /// (artifact key), `operator_user_id` (who asked), `decision`
    /// (`"granted"` / `"denied"`), `reason` (the rule that drove the verdict),
    /// and `accessed_at_ms` (epoch-ms stamp).
    ///
    /// `cui.*` prefix family — same segregation rationale as
    /// [`Self::CuiMarkingApplied`].
    CuiAccessEvent,

    /// S361 / PR-48 (ADR-0078) — a DoD Defense Priorities Allocation System
    /// (DPAS) priority rating was assigned to an approved supplier. FIRST
    /// member of the new `supplier.*` prefix family — the twelfth — the
    /// Approved-Vendor-List strand of the aerospace pivot
    /// (`[[defense-aerospace-pivot]]`). DPAS (15 CFR § 700 / FAR 11.6) lets a
    /// rated defense order compel a supplier to prioritise it over unrated
    /// commercial work; recording *which* rating a supplier is approved to
    /// service is the AVL's accountable anchor for that obligation.
    ///
    /// The `dpas_rating` value is the rendered rating the firing site (later
    /// session) produces through [`aberp_compliance::avl::DpasRating::as_str`]
    /// — a typed rating renders the string, so a free-text rating can never
    /// reach the ledger.
    ///
    /// Payload (`serde_json::Value`): `partner_id` (the AVL supplier this
    /// rating attaches to), `dpas_rating` (the rendered 15 CFR 700.12 rating,
    /// e.g. `"DO-A1"` / `"DX-A7"`; an unrated supplier is recorded by omitting
    /// the field, not a sentinel string), `operator_user_id` (who assigned it —
    /// the accountability anchor), and `set_at_ms` (epoch-ms stamp).
    ///
    /// `supplier.*` prefix family — NOT `invoice.*` / `system.*` / `mes.*` /
    /// `quote.*` / `inventory.*` / `email.*` / `personnel.*` / `material.*` /
    /// `part.*` / `export.*` / `cui.*`. A new prefix keeps the AVL surface
    /// globbable on its own (`supplier.*` = "every DPAS / screening decision on
    /// this install") without sweeping fiscal, manufacturing, quoting,
    /// access-trail, material-traceability, per-unit-serialization, CUI, or
    /// export-classification traffic — and the per-OUTGOING-invoice export
    /// bundle's `invoice.*` glob (ADR-0009 §8) never sweeps a supplier row by
    /// construction.
    ///
    /// No PII at rest: the payload records WHICH partner was rated and WHO
    /// rated it (operator id — an opaque accountability handle). S361 ships the
    /// kind only; firing sites land in a later session (no AVL CRUD surface
    /// exists yet). F12 four-edit ritual fires once for the two sibling
    /// `supplier.*` kinds.
    SupplierDpasPrioritySet,

    /// S361 / PR-48 (ADR-0078) — an approved supplier was screened against the
    /// export-control denied-party lists. The *decision* counterpart to
    /// [`Self::SupplierDpasPrioritySet`]: a durable record that supplier S was
    /// run against the BIS Entity List / OFAC SDN / State DDTC debarred lists
    /// (EAR § 744 / consolidated screening) and the screen returned clear, a
    /// hit, or an inconclusive result. A denied-party hit blocks transacting,
    /// so the screening-decision trail is load-bearing — it evidences the AVL
    /// did its EAR § 744 diligence (the `personnel.access_*` /
    /// `export.access_check` posture specialised to a supplier screen).
    ///
    /// Payload (`serde_json::Value`): `partner_id` (the screened supplier),
    /// `screening_result` (`"clear"` / `"hit"` / `"inconclusive"` — the
    /// [`aberp_compliance::avl::ExportScreeningStatus`] outcome vocab),
    /// `screening_source` (which list / service answered — e.g.
    /// `"mock-bis-csl"`), `screened_at_ms` (epoch-ms stamp),
    /// `operator_user_id` (who ran it), and an optional `hit_details`
    /// (the list / reason string, present only on a hit / inconclusive — never
    /// on a clear).
    ///
    /// `supplier.*` prefix family — same segregation rationale as
    /// [`Self::SupplierDpasPrioritySet`].
    SupplierExportScreened,

    /// S362 / PR-49 (ADR-0079) — a cyber incident affecting (or potentially
    /// affecting) a covered defense information system was detected. FIRST and
    /// only member of the new `incident.*` prefix family — the thirteenth — the
    /// cyber-incident-reporting strand of the aerospace pivot
    /// (`[[defense-aerospace-pivot]]`). DFARS 252.204-7012(c)(1) requires a
    /// contractor to report a cyber incident that affects Controlled Defense
    /// Information (CDI) or the contractor's ability to perform requirements
    /// designated operationally critical **within 72 hours of discovery** to
    /// the DoD (via the DIBNet / SPRS portal). The *detection* of an incident is
    /// the accountable anchor that starts that 72-hour clock — recording *when*
    /// it was detected, *who* reported it, *what* it touched, and *whether* CDI
    /// / CUI is implicated is the artifact that evidences the reporting
    /// obligation was triggered and tracked.
    ///
    /// Payload (`serde_json::Value`): `detected_at_ms` (epoch-ms discovery stamp
    /// — the 72-hour clock's start), `operator_user_id` (who logged it — an
    /// opaque accountability handle, never PII), `severity` (the rendered
    /// [`aberp_compliance::incident::IncidentSeverity::as_str`] form —
    /// `"informational"` / `"low"` / `"medium"` / `"high"` / `"critical"`),
    /// `scope_description` (a free-text scope summary — NOT raw log dumps, per
    /// the no-PII / no-controlled-content-at-rest posture), `cdi_affected`
    /// (bool — Controlled Defense Information per DFARS), `cui_affected` (bool —
    /// CUI per 32 CFR Part 2002), `ocs_affected` (bool — the incident affects
    /// the contractor's ability to perform requirements designated
    /// **operationally critical support** per 252.204-7012(c)(1)(i)(B)),
    /// `exfiltration_suspected` (bool),
    /// `affected_systems` (a string array of system identifiers),
    /// `detection_source` (the rendered
    /// [`aberp_compliance::incident::DetectionSource::as_str`] form — `"siem"` /
    /// `"user_report"` / `"vendor_notification"` / `"audit"` / `"other"`), an
    /// optional `mitigation_notes`, and an optional `dod_72h_report_due_at_ms`
    /// (present when `cdi_affected` **or** `ocs_affected` is true — `detected_at_ms` + 72h, the DFARS
    /// reporting deadline, computed by
    /// [`aberp_compliance::incident::dod_72h_report_due_at_ms`]).
    ///
    /// `incident.*` prefix family — NOT `invoice.*` / `system.*` / `mes.*` /
    /// `quote.*` / `inventory.*` / `email.*` / `personnel.*` / `material.*` /
    /// `part.*` / `export.*` / `cui.*` / `supplier.*`. A new prefix keeps the
    /// cyber-incident surface globbable on its own (`incident.*` = "every
    /// cyber-incident detection on this install") without sweeping fiscal,
    /// manufacturing, quoting, access-trail, material-traceability,
    /// per-unit-serialization, CUI, export-classification, or supplier-AVL
    /// traffic — and the per-OUTGOING-invoice export bundle's `invoice.*` glob
    /// (ADR-0009 §8) never sweeps an incident row by construction.
    ///
    /// No PII / no controlled content at rest: the payload records a
    /// `scope_description` summary (not raw log dumps), an opaque
    /// `operator_user_id` (not PII), and system *identifiers* (not their
    /// contents). S362 ships the kind only; the SPRS report submission, the SIEM
    /// integration, the incident-entry UI, and the 72-hour deadline alerting all
    /// land in later sessions (mock-first, no firing site exists yet). F12
    /// four-edit ritual fires for this single `incident.*` kind.
    IncidentCyberDetected,

    /// S394 — the operator changed the invoice-numbering template
    /// (`[seller.numbering]` in seller.toml) via `PUT /api/seller/numbering`.
    /// The audit-of-record for "who set the next invoice number to what,
    /// and when". Emitted AFTER the seller.toml write succeeds; one entry
    /// per save. Most relevant field is `start_value` — the
    /// operator-configured counter floor that (S394) `allocate_in_tx`
    /// honours on every allocation, not just the first-INSERT seed. So a
    /// walker can correlate a sequence jump ("issued 41, then 56") with
    /// the override that caused it.
    ///
    /// Carries `tenant_id`, `old_start_value`, `new_start_value`,
    /// `reset_policy`, `rendered_preview` (the template rendered at
    /// `new_start_value`, e.g. `INV-2026/00056`), `actor`, and
    /// `changed_at`. `system.*` prefix family (config/lifecycle, alongside
    /// `system.first_prod_launch_acknowledged`); never carries NAV XML
    /// bytes (app-layer JSON only). F12 four-edit ritual fires for this
    /// single kind.
    NumberingTemplateChanged,

    /// S426 / ADR-0082 — a validated logical DuckDB snapshot was taken
    /// (`EXPORT DATABASE` → import-validate → finalize). Payload
    /// (`SnapshotCreatedPayload`) carries the seq, UTC timestamp, source-DB
    /// SHA-256, byte size, and the smoke counts. `snapshot.*` prefix
    /// family — a system/operations event, never NAV-scoped, app-layer
    /// JSON only (no NAV XML bytes), so it never enters a per-invoice
    /// export bundle.
    SnapshotCreated,

    /// S426 / ADR-0082 — a snapshot failed its built-in validation
    /// (re-import + smoke + hash-chain verify). Payload
    /// (`SnapshotValidationFailedPayload`) carries the seq and the failure
    /// reason. The last-good snapshot is preserved. `snapshot.*` family.
    SnapshotValidationFailed,

    /// S426 / ADR-0082 — a database was restored from a snapshot via
    /// `IMPORT DATABASE`. Payload (`SnapshotRestoredPayload`) carries the
    /// source snapshot seq and the restore target path. `snapshot.*`
    /// family.
    SnapshotRestored,

    /// S426 / ADR-0082 — retention pruned one or more snapshots. Payload
    /// (`SnapshotPrunedPayload`) carries the pruned seqs and the retained
    /// count. `snapshot.*` family.
    SnapshotPruned,
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
            EventKind::RestoreFromNavRun => "system.restore_from_nav_run",
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
            EventKind::InvoiceStaged => "invoice.staged",
            EventKind::InvoiceDraftDeleted => "invoice.draft_deleted",
            EventKind::InvoicePickedUpFromQuote => "invoice.picked_up_from_quote",
            EventKind::QuoteIntakePollAttempted => "system.quote_intake_poll_attempted",
            EventKind::QuoteIntakeRowAdded => "system.quote_intake_row_added",
            EventKind::QuoteIntakePollFailed => "system.quote_intake_poll_failed",
            EventKind::AdapterAdded => "mes.adapter_added",
            EventKind::AdapterUpdated => "mes.adapter_updated",
            EventKind::AdapterRemoved => "mes.adapter_removed",
            EventKind::AdapterHealthTransitioned => "mes.adapter_health_transitioned",
            EventKind::MaterialCatalogueChanged => "quote.material_catalogue_changed",
            EventKind::MaterialCataloguePushed => "quote.material_catalogue_pushed",
            EventKind::ComplexityRulesChanged => "quote.complexity_rules_changed",
            EventKind::ToleranceMultipliersChanged => "quote.tolerance_multipliers_changed",
            EventKind::ParametersChanged => "quote.parameters_changed",
            EventKind::StockAdjustmentsChanged => "quote.stock_adjustments_changed",
            EventKind::QuoteStockAlertTriggered => "quote.stock_alert_triggered",
            EventKind::QuoteDealIssued => "quote.deal_issued",
            EventKind::QuoteSalesOrderCreated => "quote.sales_order_created",
            EventKind::QuoteWorkOrderCreated => "quote.work_order_created",
            EventKind::MaterialReserved => "inventory.material_reserved",
            EventKind::MaterialCommitted => "inventory.material_committed",
            EventKind::MaterialConsumed => "inventory.material_consumed",
            EventKind::MaterialReleased => "inventory.material_released",
            EventKind::QuotePricingFetched => "quote.pricing_fetched",
            EventKind::QuotePricingExtracted => "quote.pricing_extracted",
            EventKind::QuotePricingPriced => "quote.pricing_priced",
            EventKind::QuotePricingRendered => "quote.pricing_rendered",
            EventKind::QuotePricingPosted => "quote.pricing_posted",
            EventKind::QuotePricingFailed => "quote.pricing_failed",
            EventKind::EmailRelayQueued => "email.relay_queued",
            EventKind::EmailRelaySent => "email.relay_sent",
            EventKind::EmailRelayFailed => "email.relay_failed",
            EventKind::PipelinePythonResolved => "quote.pipeline_python_resolved",
            EventKind::QuotePricingDaemonPanicked => "quote.pricing_daemon_panicked",
            EventKind::QuotePricingJobsIndexMigrated => "quote.pricing_jobs_index_migrated",
            EventKind::QuotePricingFailureClassified => "quote.pricing_failure_classified",
            EventKind::QuotePricedWritebackOutcome => "quote.priced_writeback_outcome",
            EventKind::QuotePricingMaterialEdited => "quote.material_grade_edited",
            EventKind::QuotePricingFailureDeleted => "quote.pricing_failure_deleted",
            EventKind::QuotePricingOperatorAccepted => "quote.operator_accepted",
            EventKind::QuoteOperatorRefused => "quote.operator_refused",
            EventKind::QuotePollOutcome => "quote.poll_outcome",
            EventKind::EmailOutboxFetched => "quote.email_outbox_fetched",
            EventKind::EmailOutboxClaimed => "quote.email_outbox_claimed",
            EventKind::EmailOutboxSent => "quote.email_outbox_sent",
            EventKind::EmailOutboxFailed => "quote.email_outbox_failed",
            EventKind::QuotePdfRerenderEnqueued => "quote.pdf_rerender_enqueued",
            EventKind::QuotePdfRerendered => "quote.pdf_rerendered",
            EventKind::QuotePdfRerenderFailed => "quote.pdf_rerender_failed",
            EventKind::PersonnelIdRegistered => "personnel.id_registered",
            EventKind::PersonnelSignatureApplied => "personnel.signature_applied",
            EventKind::PersonnelAccessGranted => "personnel.access_granted",
            EventKind::PersonnelAccessDenied => "personnel.access_denied",
            EventKind::MaterialCertAttached => "material.cert_attached",
            EventKind::MaterialHeatLotAssigned => "material.heat_lot_assigned",
            EventKind::PartSerialAssigned => "part.serial_assigned",
            EventKind::PartUidMarked => "part.uid_marked",
            EventKind::ExportClassificationSet => "export.classification_set",
            EventKind::ExportAccessCheck => "export.access_check",
            EventKind::ExportShipmentLogged => "export.shipment_logged",
            EventKind::CuiMarkingApplied => "cui.marking_applied",
            EventKind::CuiAccessEvent => "cui.access_event",
            EventKind::SupplierDpasPrioritySet => "supplier.dpas_priority_set",
            EventKind::SupplierExportScreened => "supplier.export_screened",
            EventKind::IncidentCyberDetected => "incident.cyber_detected",
            EventKind::NumberingTemplateChanged => "system.numbering_template_changed",
            EventKind::SnapshotCreated => "snapshot.created",
            EventKind::SnapshotValidationFailed => "snapshot.validation_failed",
            EventKind::SnapshotRestored => "snapshot.restored",
            EventKind::SnapshotPruned => "snapshot.pruned",
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
            "system.restore_from_nav_run" => Ok(EventKind::RestoreFromNavRun),
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
            "invoice.staged" => Ok(EventKind::InvoiceStaged),
            "invoice.draft_deleted" => Ok(EventKind::InvoiceDraftDeleted),
            "invoice.picked_up_from_quote" => Ok(EventKind::InvoicePickedUpFromQuote),
            "system.quote_intake_poll_attempted" => Ok(EventKind::QuoteIntakePollAttempted),
            "system.quote_intake_row_added" => Ok(EventKind::QuoteIntakeRowAdded),
            "system.quote_intake_poll_failed" => Ok(EventKind::QuoteIntakePollFailed),
            "mes.adapter_added" => Ok(EventKind::AdapterAdded),
            "mes.adapter_updated" => Ok(EventKind::AdapterUpdated),
            "mes.adapter_removed" => Ok(EventKind::AdapterRemoved),
            "mes.adapter_health_transitioned" => Ok(EventKind::AdapterHealthTransitioned),
            "quote.material_catalogue_changed" => Ok(EventKind::MaterialCatalogueChanged),
            "quote.material_catalogue_pushed" => Ok(EventKind::MaterialCataloguePushed),
            "quote.complexity_rules_changed" => Ok(EventKind::ComplexityRulesChanged),
            "quote.tolerance_multipliers_changed" => Ok(EventKind::ToleranceMultipliersChanged),
            "quote.parameters_changed" => Ok(EventKind::ParametersChanged),
            "quote.stock_adjustments_changed" => Ok(EventKind::StockAdjustmentsChanged),
            "quote.stock_alert_triggered" => Ok(EventKind::QuoteStockAlertTriggered),
            "quote.deal_issued" => Ok(EventKind::QuoteDealIssued),
            "quote.sales_order_created" => Ok(EventKind::QuoteSalesOrderCreated),
            "quote.work_order_created" => Ok(EventKind::QuoteWorkOrderCreated),
            "inventory.material_reserved" => Ok(EventKind::MaterialReserved),
            "inventory.material_committed" => Ok(EventKind::MaterialCommitted),
            "inventory.material_consumed" => Ok(EventKind::MaterialConsumed),
            "inventory.material_released" => Ok(EventKind::MaterialReleased),
            "quote.pricing_fetched" => Ok(EventKind::QuotePricingFetched),
            "quote.pricing_extracted" => Ok(EventKind::QuotePricingExtracted),
            "quote.pricing_priced" => Ok(EventKind::QuotePricingPriced),
            "quote.pricing_rendered" => Ok(EventKind::QuotePricingRendered),
            "quote.pricing_posted" => Ok(EventKind::QuotePricingPosted),
            "quote.pricing_failed" => Ok(EventKind::QuotePricingFailed),
            "email.relay_queued" => Ok(EventKind::EmailRelayQueued),
            "email.relay_sent" => Ok(EventKind::EmailRelaySent),
            "email.relay_failed" => Ok(EventKind::EmailRelayFailed),
            "quote.pipeline_python_resolved" => Ok(EventKind::PipelinePythonResolved),
            "quote.pricing_daemon_panicked" => Ok(EventKind::QuotePricingDaemonPanicked),
            "quote.pricing_jobs_index_migrated" => Ok(EventKind::QuotePricingJobsIndexMigrated),
            "quote.pricing_failure_classified" => Ok(EventKind::QuotePricingFailureClassified),
            "quote.priced_writeback_outcome" => Ok(EventKind::QuotePricedWritebackOutcome),
            "quote.material_grade_edited" => Ok(EventKind::QuotePricingMaterialEdited),
            "quote.pricing_failure_deleted" => Ok(EventKind::QuotePricingFailureDeleted),
            "quote.operator_accepted" => Ok(EventKind::QuotePricingOperatorAccepted),
            "quote.operator_refused" => Ok(EventKind::QuoteOperatorRefused),
            "quote.poll_outcome" => Ok(EventKind::QuotePollOutcome),
            "quote.email_outbox_fetched" => Ok(EventKind::EmailOutboxFetched),
            "quote.email_outbox_claimed" => Ok(EventKind::EmailOutboxClaimed),
            "quote.email_outbox_sent" => Ok(EventKind::EmailOutboxSent),
            "quote.email_outbox_failed" => Ok(EventKind::EmailOutboxFailed),
            "quote.pdf_rerender_enqueued" => Ok(EventKind::QuotePdfRerenderEnqueued),
            "quote.pdf_rerendered" => Ok(EventKind::QuotePdfRerendered),
            "quote.pdf_rerender_failed" => Ok(EventKind::QuotePdfRerenderFailed),
            "personnel.id_registered" => Ok(EventKind::PersonnelIdRegistered),
            "personnel.signature_applied" => Ok(EventKind::PersonnelSignatureApplied),
            "personnel.access_granted" => Ok(EventKind::PersonnelAccessGranted),
            "personnel.access_denied" => Ok(EventKind::PersonnelAccessDenied),
            "material.cert_attached" => Ok(EventKind::MaterialCertAttached),
            "material.heat_lot_assigned" => Ok(EventKind::MaterialHeatLotAssigned),
            "part.serial_assigned" => Ok(EventKind::PartSerialAssigned),
            "part.uid_marked" => Ok(EventKind::PartUidMarked),
            "export.classification_set" => Ok(EventKind::ExportClassificationSet),
            "export.access_check" => Ok(EventKind::ExportAccessCheck),
            "export.shipment_logged" => Ok(EventKind::ExportShipmentLogged),
            "cui.marking_applied" => Ok(EventKind::CuiMarkingApplied),
            "cui.access_event" => Ok(EventKind::CuiAccessEvent),
            "supplier.dpas_priority_set" => Ok(EventKind::SupplierDpasPrioritySet),
            "supplier.export_screened" => Ok(EventKind::SupplierExportScreened),
            "incident.cyber_detected" => Ok(EventKind::IncidentCyberDetected),
            "system.numbering_template_changed" => Ok(EventKind::NumberingTemplateChanged),
            "snapshot.created" => Ok(EventKind::SnapshotCreated),
            "snapshot.validation_failed" => Ok(EventKind::SnapshotValidationFailed),
            "snapshot.restored" => Ok(EventKind::SnapshotRestored),
            "snapshot.pruned" => Ok(EventKind::SnapshotPruned),
            _ => Err("unknown EventKind storage string"),
        }
    }
}

impl EventKind {
    /// Every `EventKind` variant, in `as_str` declaration order.
    ///
    /// Hand-maintained (no proc-macro per S364 scope). Adding a variant
    /// forces edits to [`EventKind::as_str`] and
    /// [`EventKind::from_storage_str`] (compile error / round-trip test);
    /// the convention — pinned by `all_kinds_count_is_pinned` below — is
    /// to add it here too. `ALL_KINDS_COUNT` then changes, which trips
    /// the `const _` drift assertions in `aberp-verify::extract_nav_xml`
    /// and `export_invoice_bundle::extract_nav_xml` so a contributor must
    /// re-review the NAV-leakage gate for the new variant (ADR-0081).
    pub const ALL_KINDS: &[EventKind] = &[
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
        EventKind::RestoreFromNavRun,
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
        EventKind::InvoiceStaged,
        EventKind::InvoiceDraftDeleted,
        EventKind::InvoicePickedUpFromQuote,
        EventKind::QuoteIntakePollAttempted,
        EventKind::QuoteIntakeRowAdded,
        EventKind::QuoteIntakePollFailed,
        EventKind::AdapterAdded,
        EventKind::AdapterUpdated,
        EventKind::AdapterRemoved,
        EventKind::AdapterHealthTransitioned,
        EventKind::MaterialCatalogueChanged,
        EventKind::MaterialCataloguePushed,
        EventKind::ComplexityRulesChanged,
        EventKind::ToleranceMultipliersChanged,
        EventKind::ParametersChanged,
        EventKind::StockAdjustmentsChanged,
        EventKind::QuoteStockAlertTriggered,
        EventKind::QuoteDealIssued,
        EventKind::QuoteSalesOrderCreated,
        EventKind::QuoteWorkOrderCreated,
        EventKind::MaterialReserved,
        EventKind::MaterialCommitted,
        EventKind::MaterialConsumed,
        EventKind::MaterialReleased,
        EventKind::QuotePricingFetched,
        EventKind::QuotePricingExtracted,
        EventKind::QuotePricingPriced,
        EventKind::QuotePricingRendered,
        EventKind::QuotePricingPosted,
        EventKind::QuotePricingFailed,
        EventKind::EmailRelayQueued,
        EventKind::EmailRelaySent,
        EventKind::EmailRelayFailed,
        EventKind::PipelinePythonResolved,
        EventKind::QuotePricingDaemonPanicked,
        EventKind::QuotePricingJobsIndexMigrated,
        EventKind::QuotePricingFailureClassified,
        EventKind::QuotePricedWritebackOutcome,
        EventKind::QuotePricingMaterialEdited,
        EventKind::QuotePricingFailureDeleted,
        EventKind::QuotePricingOperatorAccepted,
        EventKind::QuoteOperatorRefused,
        EventKind::QuotePollOutcome,
        EventKind::EmailOutboxFetched,
        EventKind::EmailOutboxClaimed,
        EventKind::EmailOutboxSent,
        EventKind::EmailOutboxFailed,
        EventKind::QuotePdfRerenderEnqueued,
        EventKind::QuotePdfRerendered,
        EventKind::QuotePdfRerenderFailed,
        EventKind::PersonnelIdRegistered,
        EventKind::PersonnelSignatureApplied,
        EventKind::PersonnelAccessGranted,
        EventKind::PersonnelAccessDenied,
        EventKind::MaterialCertAttached,
        EventKind::MaterialHeatLotAssigned,
        EventKind::PartSerialAssigned,
        EventKind::PartUidMarked,
        EventKind::ExportClassificationSet,
        EventKind::ExportAccessCheck,
        EventKind::ExportShipmentLogged,
        EventKind::CuiMarkingApplied,
        EventKind::CuiAccessEvent,
        EventKind::SupplierDpasPrioritySet,
        EventKind::SupplierExportScreened,
        EventKind::IncidentCyberDetected,
        EventKind::NumberingTemplateChanged,
        EventKind::SnapshotCreated,
        EventKind::SnapshotValidationFailed,
        EventKind::SnapshotRestored,
        EventKind::SnapshotPruned,
    ];

    /// Count of [`EventKind::ALL_KINDS`]. Pinned by the NAV-leakage
    /// gates so a variant addition forces a deliberate re-review
    /// (ADR-0081).
    pub const ALL_KINDS_COUNT: usize = Self::ALL_KINDS.len();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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
            EventKind::RestoreFromNavRun,
            EventKind::MesAdapterEvent,
            EventKind::StockMovementRecorded,
            EventKind::WorkOrderCreated,
            EventKind::WorkOrderStateChanged,
            EventKind::RoutingOpStateChanged,
            EventKind::QaInspectionCreated,
            EventKind::QaInspectionDecided,
            EventKind::DispatchCreated,
            EventKind::DispatchShipped,
            EventKind::InvoiceStaged,
            EventKind::InvoiceDraftDeleted,
            EventKind::InvoicePickedUpFromQuote,
            EventKind::QuoteIntakePollAttempted,
            EventKind::QuoteIntakeRowAdded,
            EventKind::QuoteIntakePollFailed,
            EventKind::AdapterAdded,
            EventKind::AdapterUpdated,
            EventKind::AdapterRemoved,
            EventKind::AdapterHealthTransitioned,
            EventKind::MaterialCatalogueChanged,
            EventKind::MaterialCataloguePushed,
            EventKind::ComplexityRulesChanged,
            EventKind::ToleranceMultipliersChanged,
            EventKind::ParametersChanged,
            EventKind::StockAdjustmentsChanged,
            EventKind::QuoteStockAlertTriggered,
            EventKind::QuoteDealIssued,
            EventKind::QuoteSalesOrderCreated,
            EventKind::QuoteWorkOrderCreated,
            EventKind::MaterialReserved,
            EventKind::MaterialCommitted,
            EventKind::MaterialConsumed,
            EventKind::MaterialReleased,
            EventKind::QuotePricingFetched,
            EventKind::QuotePricingExtracted,
            EventKind::QuotePricingPriced,
            EventKind::QuotePricingRendered,
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
            EventKind::EmailRelayQueued,
            EventKind::EmailRelaySent,
            EventKind::EmailRelayFailed,
            EventKind::PipelinePythonResolved,
            EventKind::QuotePricingDaemonPanicked,
            EventKind::QuotePricingJobsIndexMigrated,
            EventKind::QuotePricingFailureClassified,
            EventKind::QuotePricedWritebackOutcome,
            EventKind::QuotePollOutcome,
            EventKind::QuotePricingMaterialEdited,
            EventKind::QuotePricingFailureDeleted,
            EventKind::QuotePricingOperatorAccepted,
            EventKind::QuoteOperatorRefused,
            EventKind::EmailOutboxFetched,
            EventKind::EmailOutboxClaimed,
            EventKind::EmailOutboxSent,
            EventKind::EmailOutboxFailed,
            EventKind::QuotePdfRerenderEnqueued,
            EventKind::QuotePdfRerendered,
            EventKind::QuotePdfRerenderFailed,
            EventKind::PersonnelIdRegistered,
            EventKind::PersonnelSignatureApplied,
            EventKind::PersonnelAccessGranted,
            EventKind::PersonnelAccessDenied,
            EventKind::MaterialCertAttached,
            EventKind::MaterialHeatLotAssigned,
            EventKind::PartSerialAssigned,
            EventKind::PartUidMarked,
            EventKind::ExportClassificationSet,
            EventKind::ExportAccessCheck,
            EventKind::ExportShipmentLogged,
            EventKind::CuiMarkingApplied,
            EventKind::CuiAccessEvent,
            EventKind::SupplierDpasPrioritySet,
            EventKind::SupplierExportScreened,
            EventKind::IncidentCyberDetected,
            EventKind::NumberingTemplateChanged,
            EventKind::SnapshotCreated,
            EventKind::SnapshotValidationFailed,
            EventKind::SnapshotRestored,
            EventKind::SnapshotPruned,
        ];
        for v in &variants {
            let s = v.as_str();
            let parsed = EventKind::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?} -> {e}"));
            assert_eq!(&parsed, v, "round-trip mismatch for {s:?}");
        }
        // Double-entry: this hand-list and `EventKind::ALL_KINDS` are two
        // independently-maintained enumerations. Tying them here means a
        // contributor who adds a variant to one but forgets the other is
        // caught at test time. That is what makes `ALL_KINDS_COUNT` a
        // trustworthy drift signal for the NAV-leakage gates (ADR-0081):
        // it only stays put if BOTH lists stayed put. Compared as sets —
        // the two lists need not share an ordering, only their membership.
        let round_trip_set: BTreeSet<&str> = variants.iter().map(|k| k.as_str()).collect();
        let all_kinds_set: BTreeSet<&str> =
            EventKind::ALL_KINDS.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            round_trip_set, all_kinds_set,
            "EventKind::ALL_KINDS drifted from the round-trip variant list — \
             add the new variant to both (and re-review the NAV-leakage gates \
             per ADR-0081)"
        );
    }

    /// Pin the variant count. A bump here is the deliberate signal that a
    /// new `EventKind` landed; the `const _` drift assertions in
    /// `aberp-verify` and `export_invoice_bundle` carry the same number,
    /// so all three must be updated together — forcing a re-review of the
    /// NAV-leakage gate for the new variant (ADR-0081).
    #[test]
    fn all_kinds_count_is_pinned() {
        assert_eq!(
            EventKind::ALL_KINDS_COUNT,
            106,
            "EventKind count changed — update this pin AND the matching \
             `const _` drift assertions in aberp-verify::extract_nav_xml and \
             export_invoice_bundle::extract_nav_xml, re-reviewing the new \
             variant's NAV decision (ADR-0081)"
        );
    }

    /// `ALL_KINDS` entries are distinct (no accidental duplicate, which
    /// would make `ALL_KINDS_COUNT` over-count and mask a missing variant).
    #[test]
    fn all_kinds_has_no_duplicates() {
        let mut seen = BTreeSet::new();
        for k in EventKind::ALL_KINDS {
            assert!(
                seen.insert(k.as_str()),
                "duplicate in ALL_KINDS: {}",
                k.as_str()
            );
        }
        assert_eq!(seen.len(), EventKind::ALL_KINDS_COUNT);
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

    /// S266 / PR-255: the two material-catalogue kinds open the new
    /// `quote.*` prefix family (auto-quoting strand, design doc
    /// Appendix). They are NOT invoice-scoped, so the on-disk strings
    /// MUST carry `quote.` and NOT `invoice.` — otherwise the per-
    /// OUTGOING-invoice export bundle's `invoice.*` glob would sweep a
    /// catalogue-CRUD or catalogue-push entry into an invoice's evidence
    /// bundle. Same loud-fail rationale as the `system.` pins above.
    #[test]
    fn s266_material_catalogue_kinds_use_quote_prefix() {
        assert_eq!(
            EventKind::MaterialCatalogueChanged.as_str(),
            "quote.material_catalogue_changed"
        );
        assert_eq!(
            EventKind::MaterialCataloguePushed.as_str(),
            "quote.material_catalogue_pushed"
        );
        for k in [
            EventKind::MaterialCatalogueChanged,
            EventKind::MaterialCataloguePushed,
        ] {
            assert!(k.as_str().starts_with("quote."), "{k:?} lost quote. prefix");
            assert!(
                !k.as_str().starts_with("invoice."),
                "{k:?} must not use invoice. prefix"
            );
        }
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

    /// S261 / PR-250 — the aggregate restore-batch-summary kind is a
    /// recovery operation, NOT a per-OUTGOING-invoice lifecycle event.
    /// Same `system.` prefix posture as `InvoiceRestoredFromNav`. MUST
    /// NOT carry an `invoice.` prefix or the per-OUTGOING-invoice export
    /// bundle's `invoice.*` glob would sweep a DR batch-summary entry
    /// into an evidence bundle that is supposed to carry per-invoice
    /// regulated entries only.
    #[test]
    fn s261_restore_from_nav_run_uses_system_prefix() {
        assert_eq!(
            EventKind::RestoreFromNavRun.as_str(),
            "system.restore_from_nav_run"
        );
        assert!(EventKind::RestoreFromNavRun.as_str().starts_with("system."));
        assert!(!EventKind::RestoreFromNavRun
            .as_str()
            .starts_with("invoice."));
    }

    /// S261 / PR-250 — the aggregate restore-batch kind is a DISTINCT
    /// discriminator from the per-row `InvoiceRestoredFromNav` kind (the
    /// two are emitted by the same run but answer different questions:
    /// per-invoice lineage vs batch summary) and from the AP-side kinds.
    /// Same fork-discipline posture as the other `*_is_distinct_from`
    /// pins — collapsing them would corrupt the "how many invoices did
    /// run K restore" count by conflating per-row and per-batch entries.
    #[test]
    fn s261_restore_from_nav_run_is_distinct() {
        assert_ne!(
            EventKind::RestoreFromNavRun.as_str(),
            EventKind::InvoiceRestoredFromNav.as_str()
        );
        assert_ne!(
            EventKind::RestoreFromNavRun.as_str(),
            EventKind::RestoreBuyerBackfillCycleCompleted.as_str()
        );
        assert_ne!(
            EventKind::RestoreFromNavRun.as_str(),
            EventKind::IncomingInvoiceSyncCycleCompleted.as_str()
        );
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

    /// S236 / PR-230b — `InvoiceStaged` uses the `invoice.` prefix
    /// even though it is fired from the Stage 3 dispatch transaction.
    /// Rationale per ADR-0064 §6 + the variant's docs: the entry
    /// represents the billing strand's pre-allocation state for a
    /// future invoice; it lives in the same prefix family as
    /// `InvoiceDraftCreated` / `InvoiceSequenceReserved` so an
    /// audit-walker following an invoice's chain finds the staging
    /// row alongside the allocation row. The per-OUTGOING-invoice
    /// export bundle's `invoice.*` glob does NOT pollute because the
    /// staging payload is keyed by `drf_<ULID>` not `inv_<ULID>`;
    /// staged-then-deleted drafts never get an invoice id and so
    /// never match an export filter.
    #[test]
    fn s236_invoice_staged_uses_invoice_prefix() {
        assert_eq!(EventKind::InvoiceStaged.as_str(), "invoice.staged");
        assert!(EventKind::InvoiceStaged.as_str().starts_with("invoice."));
        assert!(!EventKind::InvoiceStaged.as_str().starts_with("mes."));
        assert!(!EventKind::InvoiceStaged.as_str().starts_with("system."));
    }

    /// S236 / PR-230b — `InvoiceStaged` MUST be distinct from
    /// `InvoiceDraftCreated` (which fires when `allocate_in_tx` burns
    /// a sequence slot per ADR-0009 §3); the two storage strings are
    /// the load-bearing discriminator at audit-walk time between
    /// "draft staged, no slot burned" and "Ready row inserted with
    /// allocated sequence". Same fork-discipline posture as
    /// `s228_mes_adapter_event_is_distinct` /
    /// `s234_dispatch_kinds_are_distinct`.
    #[test]
    fn s236_invoice_staged_is_distinct() {
        assert_ne!(
            EventKind::InvoiceStaged.as_str(),
            EventKind::InvoiceDraftCreated.as_str()
        );
        assert_ne!(
            EventKind::InvoiceStaged.as_str(),
            EventKind::InvoiceSequenceReserved.as_str()
        );
        assert_ne!(
            EventKind::InvoiceStaged.as_str(),
            EventKind::DispatchShipped.as_str()
        );
        assert_ne!(
            EventKind::InvoiceStaged.as_str(),
            EventKind::DispatchCreated.as_str()
        );
    }

    /// S239 / PR-233 — `InvoiceDraftDeleted` uses the `invoice.`
    /// prefix family for the same rationale as `InvoiceStaged`: the
    /// entry is keyed by a `drf_<ULID>` (a pre-allocation draft id),
    /// not an `inv_<ULID>`, so the per-OUTGOING-invoice export bundle's
    /// `invoice.*` glob never sweeps a draft-deletion row into a
    /// downstream invoice's evidence bundle. The deletion event closes
    /// the audit-trail gap S237 §🟡 #13 named.
    #[test]
    fn s239_invoice_draft_deleted_uses_invoice_prefix() {
        assert_eq!(
            EventKind::InvoiceDraftDeleted.as_str(),
            "invoice.draft_deleted"
        );
        assert!(EventKind::InvoiceDraftDeleted
            .as_str()
            .starts_with("invoice."));
        assert!(!EventKind::InvoiceDraftDeleted.as_str().starts_with("mes."));
        assert!(!EventKind::InvoiceDraftDeleted
            .as_str()
            .starts_with("system."));
    }

    /// S239 / PR-233 — `InvoiceDraftDeleted` MUST be distinct from
    /// every prior kind in the draft / invoice lifecycle, especially
    /// `InvoiceStaged` (the create-side companion), `InvoiceMarkedAbandoned`
    /// (semantically close — both signal "this invoice will not
    /// complete the chain" — but applies to ALLOCATED invoices stuck
    /// in NAV, not pre-allocation drafts), and the storno/modify
    /// chain entries. Same fork-discipline posture as
    /// `s236_invoice_staged_is_distinct` / `pr_13_annulment_kinds_are_distinct_from_invoice_kinds`.
    #[test]
    fn s239_invoice_draft_deleted_is_distinct() {
        assert_ne!(
            EventKind::InvoiceDraftDeleted.as_str(),
            EventKind::InvoiceStaged.as_str()
        );
        assert_ne!(
            EventKind::InvoiceDraftDeleted.as_str(),
            EventKind::InvoiceMarkedAbandoned.as_str()
        );
        assert_ne!(
            EventKind::InvoiceDraftDeleted.as_str(),
            EventKind::InvoiceDraftCreated.as_str()
        );
        assert_ne!(
            EventKind::InvoiceDraftDeleted.as_str(),
            EventKind::InvoiceStornoIssued.as_str()
        );
        assert_ne!(
            EventKind::InvoiceDraftDeleted.as_str(),
            EventKind::DispatchShipped.as_str()
        );
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

    /// S256 / PR-245 — the three new quote-intake hardening kinds all
    /// use the `system.` prefix (sister-service staging traffic, never
    /// a per-OUTGOING-invoice surface). Same prefix-pin posture as
    /// `s210_quote_intake_poll_completed_uses_system_prefix`.
    #[test]
    fn s256_quote_intake_kinds_use_system_prefix() {
        for k in [
            EventKind::QuoteIntakePollAttempted,
            EventKind::QuoteIntakeRowAdded,
            EventKind::QuoteIntakePollFailed,
        ] {
            let s = k.as_str();
            assert!(s.starts_with("system."), "{s} must start with system.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("mes."), "{s} must not start with mes.");
        }
        assert_eq!(
            EventKind::QuoteIntakePollAttempted.as_str(),
            "system.quote_intake_poll_attempted"
        );
        assert_eq!(
            EventKind::QuoteIntakeRowAdded.as_str(),
            "system.quote_intake_row_added"
        );
        assert_eq!(
            EventKind::QuoteIntakePollFailed.as_str(),
            "system.quote_intake_poll_failed"
        );
    }

    /// S256 / PR-245 — the three new kinds are pairwise-distinct AND
    /// distinct from the superseded `QuoteIntakePollCompleted` (S210)
    /// and the S255 pickup kind. `QuoteIntakePollAttempted` is the v2
    /// rename of the cycle kind — it MUST NOT collide with the v1
    /// storage string or historical-row parsing would mis-route.
    #[test]
    fn s256_quote_intake_kinds_are_distinct() {
        let new = [
            EventKind::QuoteIntakePollAttempted.as_str(),
            EventKind::QuoteIntakeRowAdded.as_str(),
            EventKind::QuoteIntakePollFailed.as_str(),
        ];
        assert_ne!(new[0], new[1]);
        assert_ne!(new[0], new[2]);
        assert_ne!(new[1], new[2]);
        for k in new {
            assert_ne!(k, EventKind::QuoteIntakePollCompleted.as_str());
            assert_ne!(k, EventKind::InvoicePickedUpFromQuote.as_str());
            assert_ne!(k, EventKind::IncomingInvoiceSyncCycleCompleted.as_str());
        }
    }

    /// S257 / PR-246 — the three adapter-config CRUD kinds use the
    /// `mes.` prefix (manufacturing-domain configuration, namespace
    /// neighbour of `mes.adapter_event`), NOT `system.` or `invoice.`.
    /// Same prefix-pin posture as the S250 MES kinds.
    #[test]
    fn s257_adapter_config_kinds_use_mes_prefix() {
        for k in [
            EventKind::AdapterAdded,
            EventKind::AdapterUpdated,
            EventKind::AdapterRemoved,
        ] {
            let s = k.as_str();
            assert!(s.starts_with("mes."), "{s} must start with mes.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
        }
        assert_eq!(EventKind::AdapterAdded.as_str(), "mes.adapter_added");
        assert_eq!(EventKind::AdapterUpdated.as_str(), "mes.adapter_updated");
        assert_eq!(EventKind::AdapterRemoved.as_str(), "mes.adapter_removed");
    }

    /// S258 / PR-247 — the adapter-health-transition kind uses the `mes.`
    /// prefix (manufacturing-domain runtime telemetry, namespace
    /// neighbour of `mes.adapter_event`), NOT `system.` or `invoice.`.
    /// Same prefix-pin posture as the S257 adapter-config kinds.
    #[test]
    fn s258_adapter_health_transitioned_uses_mes_prefix() {
        let s = EventKind::AdapterHealthTransitioned.as_str();
        assert_eq!(s, "mes.adapter_health_transitioned");
        assert!(s.starts_with("mes."), "{s} must start with mes.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert!(!s.starts_with("system."), "{s} must not start with system.");
    }

    /// S258 / PR-247 — the health-transition kind is distinct from the
    /// adapter-config CRUD kinds (S257) AND from the runtime
    /// `mes.adapter_event` telemetry kind whose storage string it
    /// neighbours. A collision would mis-route a health transition into
    /// the config-CRUD or per-event bucket on parse.
    #[test]
    fn s258_adapter_health_transitioned_is_distinct() {
        let k = EventKind::AdapterHealthTransitioned.as_str();
        assert_ne!(k, EventKind::AdapterAdded.as_str());
        assert_ne!(k, EventKind::AdapterUpdated.as_str());
        assert_ne!(k, EventKind::AdapterRemoved.as_str());
        assert_ne!(k, EventKind::MesAdapterEvent.as_str());
    }

    /// S257 / PR-246 — the three adapter-config kinds are pairwise-
    /// distinct AND distinct from the pre-existing `mes.adapter_event`
    /// runtime kind (whose storage string they neighbour but must not
    /// collide with — a collision would mis-route runtime adapter
    /// telemetry into the config-CRUD bucket on parse).
    #[test]
    fn s257_adapter_config_kinds_are_distinct() {
        let new = [
            EventKind::AdapterAdded.as_str(),
            EventKind::AdapterUpdated.as_str(),
            EventKind::AdapterRemoved.as_str(),
        ];
        assert_ne!(new[0], new[1]);
        assert_ne!(new[0], new[2]);
        assert_ne!(new[1], new[2]);
        for k in new {
            assert_ne!(k, EventKind::MesAdapterEvent.as_str());
        }
    }

    /// S267 / PR-256 — the four new tunables-CRUD kinds extend the
    /// `quote.*` prefix family the S266 material-catalogue kinds opened
    /// (auto-quoting strand, design doc Appendix). They are NOT
    /// invoice-scoped, so the on-disk strings MUST carry `quote.` and
    /// NOT `invoice.` / `system.` / `mes.` — otherwise the per-OUTGOING-
    /// invoice export bundle's `invoice.*` glob would sweep a tunables-
    /// CRUD entry into an invoice's evidence bundle. Same loud-fail
    /// rationale as the S266 pin above.
    #[test]
    fn s267_tunables_kinds_use_quote_prefix() {
        let cases: [(EventKind, &str); 4] = [
            (
                EventKind::ComplexityRulesChanged,
                "quote.complexity_rules_changed",
            ),
            (
                EventKind::ToleranceMultipliersChanged,
                "quote.tolerance_multipliers_changed",
            ),
            (EventKind::ParametersChanged, "quote.parameters_changed"),
            (
                EventKind::StockAdjustmentsChanged,
                "quote.stock_adjustments_changed",
            ),
        ];
        for (k, expected) in cases {
            assert_eq!(k.as_str(), expected);
            let s = k.as_str();
            assert!(s.starts_with("quote."), "{s} must start with quote.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
            assert!(!s.starts_with("mes."), "{s} must not start with mes.");
        }
    }

    /// S267 / PR-256 — the four new tunables-CRUD storage strings are
    /// pairwise-distinct AND distinct from the two pre-existing
    /// `quote.material_catalogue_*` strings (S266) they neighbour. A
    /// collision would mis-route a per-row CRUD entry to the wrong
    /// tunables history bucket on parse.
    #[test]
    fn s267_tunables_kinds_are_distinct() {
        let new = [
            EventKind::ComplexityRulesChanged.as_str(),
            EventKind::ToleranceMultipliersChanged.as_str(),
            EventKind::ParametersChanged.as_str(),
            EventKind::StockAdjustmentsChanged.as_str(),
        ];
        for i in 0..new.len() {
            for j in (i + 1)..new.len() {
                assert_ne!(new[i], new[j], "{} collides with {}", new[i], new[j]);
            }
        }
        for k in new {
            assert_ne!(k, EventKind::MaterialCatalogueChanged.as_str());
            assert_ne!(k, EventKind::MaterialCataloguePushed.as_str());
        }
    }

    /// S271 / PR-260 — `QuoteStockAlertTriggered` extends the `quote.*`
    /// prefix family the S266/S267 kinds opened (EVE addendum 2 stale-
    /// stock guard, design doc Appendix A). It is NOT invoice-scoped, so
    /// the on-disk string MUST carry `quote.` and NOT `invoice.` /
    /// `system.` / `mes.` — same loud-fail rationale as the S266 + S267
    /// pins above (a misprefix would either silently sweep the entry
    /// into a per-OUTGOING-invoice bundle or split the
    /// auto-quoting-strand history across two prefixes).
    #[test]
    fn s271_stock_alert_kind_uses_quote_prefix() {
        let k = EventKind::QuoteStockAlertTriggered;
        assert_eq!(k.as_str(), "quote.stock_alert_triggered");
        let s = k.as_str();
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert!(!s.starts_with("system."), "{s} must not start with system.");
        assert!(!s.starts_with("mes."), "{s} must not start with mes.");
    }

    /// S271 / PR-260 — `QuoteStockAlertTriggered` must be distinct from
    /// every other auto-quoting-strand `quote.*` kind (S266 catalogue
    /// + S267 tunables). A collision would mis-route an EVE-addendum-2
    /// stale-stock event into a catalogue-CRUD bucket on parse.
    #[test]
    fn s271_stock_alert_kind_is_distinct_from_other_quote_kinds() {
        let alert = EventKind::QuoteStockAlertTriggered.as_str();
        for neighbour in [
            EventKind::MaterialCatalogueChanged.as_str(),
            EventKind::MaterialCataloguePushed.as_str(),
            EventKind::ComplexityRulesChanged.as_str(),
            EventKind::ToleranceMultipliersChanged.as_str(),
            EventKind::ParametersChanged.as_str(),
            EventKind::StockAdjustmentsChanged.as_str(),
        ] {
            assert_ne!(
                alert, neighbour,
                "{alert} must be distinct from quote.* neighbour {neighbour}"
            );
        }
    }

    /// S273 / PR-262 / ADR-0069 — the four material-state-machine kinds
    /// open a NEW `inventory.*` prefix family, distinct from:
    ///
    ///   * `quote.*` (catalogue, tunables, DEAL saga) — quote-strand
    ///     concerns; material balances are downstream of a DEAL but are
    ///     a separate domain (an inventory adjustment is not a quote
    ///     event).
    ///   * `mes.*` (product-side `stock_movements`) — those track
    ///     finished-goods + WIP; material balances track raw stock
    ///     keyed on `quoting_materials.grade`. Different table, different
    ///     state machine, different audit family.
    ///   * `invoice.*` / `system.*` — same per-OUTGOING-invoice bundle
    ///     glob trap as the S266/S267/S271 pins above. A misprefix would
    ///     either sweep material-commit traffic into an invoice's
    ///     evidence bundle (S166-style) or fork the inventory history
    ///     across two prefixes (S267-style).
    ///
    /// Loud-fail pin so a future contributor renaming `inventory.*` →
    /// `quote.*` (a tempting collapse — the DEAL saga emits BOTH) is
    /// caught at test time, not when a forensic walk reads two
    /// prefixes for one history.
    #[test]
    fn s273_material_state_kinds_use_inventory_prefix() {
        let cases: [(EventKind, &str); 4] = [
            (EventKind::MaterialReserved, "inventory.material_reserved"),
            (EventKind::MaterialCommitted, "inventory.material_committed"),
            (EventKind::MaterialConsumed, "inventory.material_consumed"),
            (EventKind::MaterialReleased, "inventory.material_released"),
        ];
        for (k, expected) in cases {
            assert_eq!(k.as_str(), expected);
            let s = k.as_str();
            assert!(
                s.starts_with("inventory."),
                "{s} must start with inventory."
            );
            assert!(!s.starts_with("quote."), "{s} must not start with quote.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
            assert!(!s.starts_with("mes."), "{s} must not start with mes.");
        }
    }

    /// S273 / PR-262 / ADR-0069 — the four new storage strings are
    /// pairwise-distinct AND distinct from `mes.stock_movement_recorded`
    /// (the closest neighbour conceptually — also stock-tracking, but
    /// product-side, not material-side). A collision would mis-route a
    /// material commit into the product-stock-movement history bucket.
    #[test]
    fn s273_material_state_kinds_are_distinct() {
        let new = [
            EventKind::MaterialReserved.as_str(),
            EventKind::MaterialCommitted.as_str(),
            EventKind::MaterialConsumed.as_str(),
            EventKind::MaterialReleased.as_str(),
        ];
        for i in 0..new.len() {
            for j in (i + 1)..new.len() {
                assert_ne!(new[i], new[j], "{} collides with {}", new[i], new[j]);
            }
        }
        for k in new {
            assert_ne!(k, EventKind::StockMovementRecorded.as_str());
            assert_ne!(k, EventKind::QuoteDealIssued.as_str());
        }
    }

    /// S279 / PR-265 — the six new pricing-pipeline kinds extend the
    /// `quote.*` prefix family (alongside S266 catalogue, S267 tunables,
    /// S271 stock-alert, S272 DEAL saga). Pushback against the brief's
    /// `quote.pricing.*` three-segment shape: codebase convention is
    /// `prefix.snake_case_name` (single dot). Loud-fail pin so a future
    /// edit collapsing the prefix or re-introducing the two-dot shape
    /// fails at test time.
    #[test]
    fn s279_pricing_kinds_use_quote_prefix() {
        let cases: [(EventKind, &str); 6] = [
            (EventKind::QuotePricingFetched, "quote.pricing_fetched"),
            (EventKind::QuotePricingExtracted, "quote.pricing_extracted"),
            (EventKind::QuotePricingPriced, "quote.pricing_priced"),
            (EventKind::QuotePricingRendered, "quote.pricing_rendered"),
            (EventKind::QuotePricingPosted, "quote.pricing_posted"),
            (EventKind::QuotePricingFailed, "quote.pricing_failed"),
        ];
        for (k, expected) in cases {
            assert_eq!(k.as_str(), expected);
            let s = k.as_str();
            assert!(s.starts_with("quote."), "{s} must start with quote.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
            assert!(!s.starts_with("mes."), "{s} must not start with mes.");
            assert!(
                !s.starts_with("inventory."),
                "{s} must not start with inventory."
            );
        }
    }

    /// S279 / PR-265 — the six pricing-pipeline storage strings are
    /// pairwise-distinct AND distinct from every other `quote.*`
    /// neighbour (catalogue, tunables, stock-alert, DEAL-saga trio).
    /// A collision would mis-route a pricing event into a neighbour
    /// bucket on parse — e.g. `pricing_failed` mis-spelled as
    /// `stock_alert_triggered` would mute the alert badge.
    #[test]
    fn s279_pricing_kinds_are_distinct() {
        let new = [
            EventKind::QuotePricingFetched.as_str(),
            EventKind::QuotePricingExtracted.as_str(),
            EventKind::QuotePricingPriced.as_str(),
            EventKind::QuotePricingRendered.as_str(),
            EventKind::QuotePricingPosted.as_str(),
            EventKind::QuotePricingFailed.as_str(),
        ];
        for i in 0..new.len() {
            for j in (i + 1)..new.len() {
                assert_ne!(new[i], new[j], "{} collides with {}", new[i], new[j]);
            }
        }
        for k in new {
            assert_ne!(k, EventKind::MaterialCatalogueChanged.as_str());
            assert_ne!(k, EventKind::MaterialCataloguePushed.as_str());
            assert_ne!(k, EventKind::ComplexityRulesChanged.as_str());
            assert_ne!(k, EventKind::ToleranceMultipliersChanged.as_str());
            assert_ne!(k, EventKind::ParametersChanged.as_str());
            assert_ne!(k, EventKind::StockAdjustmentsChanged.as_str());
            assert_ne!(k, EventKind::QuoteStockAlertTriggered.as_str());
            assert_ne!(k, EventKind::QuoteDealIssued.as_str());
            assert_ne!(k, EventKind::QuoteSalesOrderCreated.as_str());
            assert_ne!(k, EventKind::QuoteWorkOrderCreated.as_str());
        }
    }

    /// S281 / PR-266 — the three email-relay storage strings open the
    /// new `email.*` prefix family. Distinct from every prior family
    /// (`invoice.`, `system.`, `mes.`, `quote.`, `inventory.`) so the
    /// per-OUTGOING-invoice export bundle's `invoice.*` glob never
    /// sweeps a relay row. The existing `invoice.emailed_sent` is a
    /// per-invoice surface and stays where it is; the relay surface
    /// carries no invoice id and lives in its own family.
    #[test]
    fn s281_email_relay_kinds_use_email_prefix() {
        let cases: [(EventKind, &str); 3] = [
            (EventKind::EmailRelayQueued, "email.relay_queued"),
            (EventKind::EmailRelaySent, "email.relay_sent"),
            (EventKind::EmailRelayFailed, "email.relay_failed"),
        ];
        for (k, expected) in cases {
            assert_eq!(k.as_str(), expected);
            let s = k.as_str();
            assert!(s.starts_with("email."), "{s} must start with email.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
            assert!(!s.starts_with("quote."), "{s} must not start with quote.");
            assert!(!s.starts_with("mes."), "{s} must not start with mes.");
            assert!(
                !s.starts_with("inventory."),
                "{s} must not start with inventory."
            );
        }
    }

    /// S281 / PR-266 — the three email-relay storage strings are
    /// pairwise-distinct AND distinct from the existing
    /// `invoice.emailed_sent` (a different surface — per-invoice send,
    /// not sister-service relay). A future contributor collapsing
    /// `EmailRelaySent` onto `InvoiceEmailedSent` would lose the
    /// submitter / queue_row_id discriminator; pin to prevent.
    #[test]
    fn s281_email_relay_kinds_are_distinct() {
        let new = [
            EventKind::EmailRelayQueued.as_str(),
            EventKind::EmailRelaySent.as_str(),
            EventKind::EmailRelayFailed.as_str(),
        ];
        for i in 0..new.len() {
            for j in (i + 1)..new.len() {
                assert_ne!(new[i], new[j], "{} collides with {}", new[i], new[j]);
            }
        }
        for k in new {
            assert_ne!(k, EventKind::InvoiceEmailedSent.as_str());
        }
    }

    /// S282 / PR-267 — the new pipeline-python-resolve kind lives in the
    /// `quote.*` family alongside its S279 pricing siblings (one prefix
    /// per forensic query "everything the pricing daemon did"). Single-
    /// dot shape, not the brief's `quote.pipeline.*` two-dot shape;
    /// matches the codebase convention.
    #[test]
    fn s282_pipeline_python_resolved_uses_quote_prefix() {
        let s = EventKind::PipelinePythonResolved.as_str();
        assert_eq!(s, "quote.pipeline_python_resolved");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert!(!s.starts_with("system."), "{s} must not start with system.");
        assert!(!s.starts_with("mes."), "{s} must not start with mes.");
        assert!(
            !s.starts_with("inventory."),
            "{s} must not start with inventory."
        );
        assert!(!s.starts_with("email."), "{s} must not start with email.");
    }

    /// S282 / PR-267 — pipeline-python-resolve is distinct from every
    /// S279 pricing sibling. A collision would mis-route a venv-resolve
    /// row into one of the per-job state buckets (`Fetched`/`Extracted`/
    /// `Failed` etc.) on parse — silently muting the audit-trail
    /// `[[trust-code-not-operator]]` guarantee the kind was added for.
    #[test]
    fn s282_pipeline_python_resolved_is_distinct() {
        let s = EventKind::PipelinePythonResolved.as_str();
        for sibling in [
            EventKind::QuotePricingFetched,
            EventKind::QuotePricingExtracted,
            EventKind::QuotePricingPriced,
            EventKind::QuotePricingRendered,
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    /// S286 / PR-268 — the new daemon-panic kind lives in the `quote.*`
    /// family alongside the six S279 pricing-pipeline siblings. The brief's
    /// hotfix posture: "everything the pricing pipeline did, including the
    /// supervisor catching a panic" fits one forensic-glob query.
    #[test]
    fn s286_pricing_daemon_panicked_uses_quote_prefix() {
        let s = EventKind::QuotePricingDaemonPanicked.as_str();
        assert_eq!(s, "quote.pricing_daemon_panicked");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert!(!s.starts_with("system."), "{s} must not start with system.");
        assert!(!s.starts_with("mes."), "{s} must not start with mes.");
        assert!(
            !s.starts_with("inventory."),
            "{s} must not start with inventory."
        );
        assert!(!s.starts_with("email."), "{s} must not start with email.");
    }

    /// S286 / PR-268 — daemon-panicked is distinct from every S279 pricing
    /// sibling AND from the S282 pipeline-python-resolved kind. A collision
    /// would mis-route a panic-recovery row into one of the per-job state
    /// buckets, silently muting the panic banner the SPA renders on
    /// `recent_panic_count > 0` — the exact failure mode CLAUDE.md rule 12
    /// names.
    #[test]
    fn s286_pricing_daemon_panicked_is_distinct() {
        let s = EventKind::QuotePricingDaemonPanicked.as_str();
        for sibling in [
            EventKind::QuotePricingFetched,
            EventKind::QuotePricingExtracted,
            EventKind::QuotePricingPriced,
            EventKind::QuotePricingRendered,
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
            EventKind::PipelinePythonResolved,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    /// S288 / PR-269 — the boot-time index-migration kind sits in the
    /// `quote.*` family alongside its siblings. The hotfix-on-hotfix
    /// posture: a forensic walker grepping `quote.*` on a prod ledger
    /// must surface the migration row that proves "this install's
    /// pricing-pipeline DB has had the orphan secondary index removed".
    #[test]
    fn s288_pricing_jobs_index_migrated_uses_quote_prefix() {
        let s = EventKind::QuotePricingJobsIndexMigrated.as_str();
        assert_eq!(s, "quote.pricing_jobs_index_migrated");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert!(!s.starts_with("system."), "{s} must not start with system.");
        assert!(!s.starts_with("mes."), "{s} must not start with mes.");
        assert!(
            !s.starts_with("inventory."),
            "{s} must not start with inventory."
        );
        assert!(!s.starts_with("email."), "{s} must not start with email.");
    }

    /// S288 / PR-269 — index-migrated is distinct from every pricing-
    /// pipeline sibling. A collision would mis-route the one-shot
    /// migration row into a per-job bucket and (worse) leave the SPA's
    /// "what venv / what migrations ran on this install" forensic view
    /// silently incomplete.
    #[test]
    fn s288_pricing_jobs_index_migrated_is_distinct() {
        let s = EventKind::QuotePricingJobsIndexMigrated.as_str();
        for sibling in [
            EventKind::QuotePricingFetched,
            EventKind::QuotePricingExtracted,
            EventKind::QuotePricingPriced,
            EventKind::QuotePricingRendered,
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
            EventKind::PipelinePythonResolved,
            EventKind::QuotePricingDaemonPanicked,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    /// S290 / PR-271 — the classifier-verdict kind sits in the `quote.*`
    /// family alongside its pricing-pipeline siblings. A forensic walker
    /// grepping `quote.*` for "everything the pricing pipeline did on this
    /// install" must surface the classifier's verdict next to the failure
    /// it explains.
    #[test]
    fn s290_pricing_failure_classified_uses_quote_prefix() {
        let s = EventKind::QuotePricingFailureClassified.as_str();
        assert_eq!(s, "quote.pricing_failure_classified");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert!(!s.starts_with("system."), "{s} must not start with system.");
        assert!(!s.starts_with("mes."), "{s} must not start with mes.");
        assert!(
            !s.starts_with("inventory."),
            "{s} must not start with inventory."
        );
        assert!(!s.starts_with("email."), "{s} must not start with email.");
    }

    /// S290 / PR-271 — classifier-verdict kind is distinct from every
    /// pricing-pipeline sibling. A collision would mis-route the verdict
    /// row into a per-job stage bucket and silently mute the operator-
    /// visible "operator retry required" badge.
    #[test]
    fn s290_pricing_failure_classified_is_distinct() {
        let s = EventKind::QuotePricingFailureClassified.as_str();
        for sibling in [
            EventKind::QuotePricingFetched,
            EventKind::QuotePricingExtracted,
            EventKind::QuotePricingPriced,
            EventKind::QuotePricingRendered,
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
            EventKind::PipelinePythonResolved,
            EventKind::QuotePricingDaemonPanicked,
            EventKind::QuotePricingJobsIndexMigrated,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    /// S307 / PR-276 — the four email-outbox poll-daemon kinds live in
    /// the `quote.*` family. The brief explicitly chose `quote.*` over
    /// `email.*` because the outbox is part of the auto-quoting
    /// pipeline strand (submission-received / priced-ready / accept-
    /// confirmation are all quote-flow emails). The S281 `email.relay_*`
    /// kinds stay in their own `email.*` family — that surface is the
    /// deprecated push-based path, not part of the polling pipeline.
    #[test]
    fn s307_email_outbox_kinds_use_quote_prefix() {
        let cases: [(EventKind, &str); 4] = [
            (EventKind::EmailOutboxFetched, "quote.email_outbox_fetched"),
            (EventKind::EmailOutboxClaimed, "quote.email_outbox_claimed"),
            (EventKind::EmailOutboxSent, "quote.email_outbox_sent"),
            (EventKind::EmailOutboxFailed, "quote.email_outbox_failed"),
        ];
        for (k, expected) in cases {
            let s = k.as_str();
            assert_eq!(s, expected);
            assert!(s.starts_with("quote."), "{s} must start with quote.");
            assert!(
                !s.starts_with("invoice."),
                "{s} must not start with invoice."
            );
            assert!(!s.starts_with("system."), "{s} must not start with system.");
            assert!(!s.starts_with("mes."), "{s} must not start with mes.");
            assert!(
                !s.starts_with("inventory."),
                "{s} must not start with inventory."
            );
            assert!(!s.starts_with("email."), "{s} must not start with email.");
        }
    }

    /// S307 / PR-276 — the four email-outbox kinds are pairwise-distinct
    /// AND distinct from every pricing-pipeline sibling. A collision
    /// would mis-route a per-cycle / per-entry outbox row into a pricing-
    /// stage bucket and silently mute the SPA panel's "last poll" /
    /// "in flight" / "recent failures" surfaces — exactly the failure
    /// mode CLAUDE.md rule 12 ("fail loud") guards against. Additionally
    /// distinct from the S281 `email.relay_*` kinds (different surface,
    /// different prefix family, but a future contributor collapsing the
    /// strings would lose the polling-vs-push discriminator).
    #[test]
    fn s307_email_outbox_kinds_are_distinct() {
        let new = [
            EventKind::EmailOutboxFetched.as_str(),
            EventKind::EmailOutboxClaimed.as_str(),
            EventKind::EmailOutboxSent.as_str(),
            EventKind::EmailOutboxFailed.as_str(),
        ];
        // Pairwise distinct.
        for i in 0..new.len() {
            for j in (i + 1)..new.len() {
                assert_ne!(new[i], new[j], "{} collides with {}", new[i], new[j]);
            }
        }
        // Distinct from every pricing-pipeline sibling.
        for k in new {
            for sibling in [
                EventKind::QuotePricingFetched,
                EventKind::QuotePricingExtracted,
                EventKind::QuotePricingPriced,
                EventKind::QuotePricingRendered,
                EventKind::QuotePricingPosted,
                EventKind::QuotePricingFailed,
                EventKind::PipelinePythonResolved,
                EventKind::QuotePricingDaemonPanicked,
                EventKind::QuotePricingJobsIndexMigrated,
                EventKind::QuotePricingFailureClassified,
                EventKind::EmailRelayQueued,
                EventKind::EmailRelaySent,
                EventKind::EmailRelayFailed,
            ] {
                assert_ne!(
                    k,
                    sibling.as_str(),
                    "{k} collides with sibling {}",
                    sibling.as_str()
                );
            }
        }
    }

    /// S325 / PR-25 — F12 prefix ritual for the three PDF-re-render kinds.
    /// Customer-facing PDF re-render is a `quote.*` surface (it rides the
    /// same auto-quoting strand as the pricing pipeline), so the on-disk
    /// strings MUST carry the `quote.` prefix and nothing else.
    #[test]
    fn s325_pdf_rerender_kinds_use_quote_prefix() {
        let cases: [(EventKind, &str); 3] = [
            (
                EventKind::QuotePdfRerenderEnqueued,
                "quote.pdf_rerender_enqueued",
            ),
            (EventKind::QuotePdfRerendered, "quote.pdf_rerendered"),
            (
                EventKind::QuotePdfRerenderFailed,
                "quote.pdf_rerender_failed",
            ),
        ];
        for (k, expected) in cases {
            let s = k.as_str();
            assert_eq!(s, expected);
            assert!(s.starts_with("quote."), "{s} must start with quote.");
            assert!(!s.starts_with("invoice."), "{s} must not start invoice.");
            assert!(!s.starts_with("system."), "{s} must not start system.");
            assert!(!s.starts_with("mes."), "{s} must not start mes.");
            assert!(!s.starts_with("inventory."), "{s} must not start inv.");
            assert!(!s.starts_with("email."), "{s} must not start email.");
        }
    }

    /// S325 / PR-25 — the three re-render kinds round-trip
    /// `as_str` → `from_storage_str` and are pairwise-distinct AND
    /// distinct from every pricing-pipeline + email-outbox sibling. A
    /// collision would mis-bucket a re-render audit row into a pricing
    /// stage and silently mute the customer-delivery trail (CLAUDE.md
    /// rule 12 — fail loud).
    #[test]
    fn s325_pdf_rerender_kinds_distinct_and_round_trip() {
        let new = [
            EventKind::QuotePdfRerenderEnqueued,
            EventKind::QuotePdfRerendered,
            EventKind::QuotePdfRerenderFailed,
        ];
        for v in &new {
            assert_eq!(
                &EventKind::from_storage_str(v.as_str()).expect("round-trip"),
                v
            );
        }
        let strs: Vec<&str> = new.iter().map(EventKind::as_str).collect();
        for i in 0..strs.len() {
            for j in (i + 1)..strs.len() {
                assert_ne!(strs[i], strs[j], "{} collides with {}", strs[i], strs[j]);
            }
        }
        for k in &strs {
            for sibling in [
                EventKind::QuotePricingRendered,
                EventKind::QuotePricingPosted,
                EventKind::QuotePricingFailed,
                EventKind::QuoteStockAlertTriggered,
                EventKind::EmailOutboxSent,
                EventKind::EmailOutboxFailed,
            ] {
                assert_ne!(
                    *k,
                    sibling.as_str(),
                    "{k} collides with sibling {}",
                    sibling.as_str()
                );
            }
        }
    }

    /// S347 / PR-39 (F1+F2) — F12 ritual for the priced-writeback-outcome
    /// kind: round-trips, carries the `quote.` prefix, and is distinct from
    /// every pricing sibling so a granular transport verdict can never be
    /// mis-bucketed as a coarse job-failure row (CLAUDE.md rule 12).
    #[test]
    fn s347_priced_writeback_outcome_kind_round_trips_and_is_distinct() {
        let k = EventKind::QuotePricedWritebackOutcome;
        let s = k.as_str();
        assert_eq!(s, "quote.priced_writeback_outcome");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
        for sibling in [
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
            EventKind::QuotePricingFailureClassified,
            EventKind::QuotePricingRendered,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    /// S348 / PR-39 (F1) — F12 ritual for the list-poll-outcome kind:
    /// round-trips, carries the `quote.` prefix, and is distinct from the
    /// per-writeback verdict + every pricing sibling so a poll-cycle misroute
    /// can never be mis-bucketed as a per-quote writeback row (CLAUDE.md
    /// rule 12).
    #[test]
    fn s348_poll_outcome_kind_round_trips_and_is_distinct() {
        let k = EventKind::QuotePollOutcome;
        let s = k.as_str();
        assert_eq!(s, "quote.poll_outcome");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
        for sibling in [
            EventKind::QuotePricedWritebackOutcome,
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
            EventKind::QuotePricingFailureClassified,
            EventKind::QuotePricingRendered,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    /// S350 / PR-39 (U5) — operator material-grade edit kind round-trips
    /// and is distinct from every pricing-pipeline sibling. The on-disk
    /// string is `quote.material_grade_edited` (operator-readable when a
    /// forensic walker greps `quote.*` for "who changed what on this
    /// row"); a collision with a per-stage kind would mis-route the
    /// override into the wrong bucket and hide who applied a phone-
    /// clarified material grade.
    #[test]
    fn s350_material_edited_kind_round_trips_and_is_distinct() {
        let k = EventKind::QuotePricingMaterialEdited;
        let s = k.as_str();
        assert_eq!(s, "quote.material_grade_edited");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
        for sibling in [
            EventKind::QuotePricingFetched,
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
            EventKind::QuotePricingFailureClassified,
            EventKind::QuotePricedWritebackOutcome,
            EventKind::QuotePollOutcome,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    /// S354 / PR-42 (U16) — the operator accept-on-behalf kind. Storage
    /// string is `quote.operator_accepted` (operator-readable when a
    /// forensic walker greps `quote.*` for "who accepted this quote, by
    /// what channel"); a collision with the customer-owned accept or any
    /// per-stage kind would mis-route the off-channel acceptance into the
    /// wrong bucket and hide who marked it accepted.
    #[test]
    fn s354_operator_accepted_kind_round_trips_and_is_distinct() {
        let k = EventKind::QuotePricingOperatorAccepted;
        let s = k.as_str();
        assert_eq!(s, "quote.operator_accepted");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
        for sibling in [
            EventKind::QuotePricingFetched,
            EventKind::QuotePricingPosted,
            EventKind::QuotePricingFailed,
            EventKind::QuotePricingMaterialEdited,
            EventKind::QuotePricedWritebackOutcome,
            EventKind::QuotePollOutcome,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    /// S403 — operator REFUSE-with-reason. Storage string is
    /// `quote.operator_refused`, the negative counterpart to
    /// `quote.operator_accepted`. A collision with the accept kind or any
    /// `quote.deal_*` kind would mis-route the refusal (hiding WHY an
    /// accepted order was turned down), so pin distinctness explicitly.
    #[test]
    fn s403_operator_refused_kind_round_trips_and_is_distinct() {
        let k = EventKind::QuoteOperatorRefused;
        let s = k.as_str();
        assert_eq!(s, "quote.operator_refused");
        assert!(s.starts_with("quote."), "{s} must start with quote.");
        assert!(
            !s.starts_with("invoice."),
            "{s} must not start with invoice."
        );
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
        for sibling in [
            EventKind::QuotePricingOperatorAccepted,
            EventKind::QuoteDealIssued,
            EventKind::QuotePricingFailed,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with {}",
                sibling.as_str()
            );
        }
    }

    // ── S355 / PR-43 (ADR-0073) — personnel.* access-trail family ──────────
    //
    // Four kinds open the new `personnel.*` prefix family: identity
    // registration, e-signature application, and the access grant/deny pair.
    // Per the brief each variant gets a focused round-trip test, each payload
    // shape is pinned by a serialization test, the family shares one
    // prefix-and-distinctness test, and a no-NAV-bytes pin lives with the
    // exhaustive-match consumers (`aberp-verify`, `export_invoice_bundle`).

    #[test]
    fn s355_personnel_id_registered_round_trips() {
        let k = EventKind::PersonnelIdRegistered;
        let s = k.as_str();
        assert_eq!(s, "personnel.id_registered");
        assert!(
            s.starts_with("personnel."),
            "{s} must start with personnel."
        );
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s355_personnel_signature_applied_round_trips() {
        let k = EventKind::PersonnelSignatureApplied;
        let s = k.as_str();
        assert_eq!(s, "personnel.signature_applied");
        assert!(
            s.starts_with("personnel."),
            "{s} must start with personnel."
        );
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s355_personnel_access_granted_round_trips() {
        let k = EventKind::PersonnelAccessGranted;
        let s = k.as_str();
        assert_eq!(s, "personnel.access_granted");
        assert!(
            s.starts_with("personnel."),
            "{s} must start with personnel."
        );
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s355_personnel_access_denied_round_trips() {
        let k = EventKind::PersonnelAccessDenied;
        let s = k.as_str();
        assert_eq!(s, "personnel.access_denied");
        assert!(
            s.starts_with("personnel."),
            "{s} must start with personnel."
        );
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    /// S355 — the four `personnel.*` kinds share the new prefix family, carry
    /// NO other prefix, and are pairwise-distinct AND distinct from a sample
    /// of every other prefix family. A collision (or a wrong prefix) would
    /// either mis-bucket an access-trail row into a fiscal / manufacturing /
    /// quoting bucket OR let the per-OUTGOING-invoice export bundle's
    /// `invoice.*` glob sweep a personnel row — both are the silent-omission
    /// failure mode CLAUDE.md rule 12 names.
    #[test]
    fn s355_personnel_kinds_use_personnel_prefix_and_are_distinct() {
        let new = [
            EventKind::PersonnelIdRegistered,
            EventKind::PersonnelSignatureApplied,
            EventKind::PersonnelAccessGranted,
            EventKind::PersonnelAccessDenied,
        ];
        for k in &new {
            let s = k.as_str();
            assert!(
                s.starts_with("personnel."),
                "{s} must start with personnel."
            );
            for foreign in [
                "invoice.",
                "system.",
                "mes.",
                "quote.",
                "inventory.",
                "email.",
            ] {
                assert!(!s.starts_with(foreign), "{s} must not start with {foreign}");
            }
        }
        // Pairwise distinct within the family.
        let strs: Vec<&str> = new.iter().map(EventKind::as_str).collect();
        for i in 0..strs.len() {
            for j in (i + 1)..strs.len() {
                assert_ne!(strs[i], strs[j], "{} collides with {}", strs[i], strs[j]);
            }
        }
        // Distinct from a sample sibling per other prefix family.
        for k in &strs {
            for sibling in [
                EventKind::InvoiceDraftCreated,
                EventKind::FirstProdLaunchAcknowledged,
                EventKind::MesAdapterEvent,
                EventKind::QuotePricingOperatorAccepted,
                EventKind::MaterialReserved,
                EventKind::EmailRelaySent,
            ] {
                assert_ne!(
                    *k,
                    sibling.as_str(),
                    "{k} collides with foreign-family {}",
                    sibling.as_str()
                );
            }
        }
    }

    /// S355 — pin the documented `personnel.id_registered` payload shape so a
    /// future firing site (S356+) has a stable contract. The kind stores a
    /// free-form `serde_json::Value` (same posture as every recent `quote.*`
    /// kind); this asserts the documented fields are present with the
    /// documented JSON types after a serialize → parse round-trip.
    #[test]
    fn s355_personnel_id_registered_payload_serializes() {
        let payload = serde_json::json!({
            "operator_user_id": "mock-op-001",
            "provider_name": "mock",
            "registered_at_ms": 1_750_000_000_000_i64,
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["provider_name"].is_string());
        assert!(parsed["registered_at_ms"].is_i64());
    }

    /// S355 — pin the documented `personnel.signature_applied` payload shape.
    /// The `signature_algorithm` tag is load-bearing (a verifier checks it
    /// before recomputing), so the test asserts it is carried as a string.
    #[test]
    fn s355_personnel_signature_applied_payload_serializes() {
        let payload = serde_json::json!({
            "operator_user_id": "mock-op-001",
            "signed_record_kind": "invoice",
            "signed_record_id": "inv_01HZ",
            "signature_algorithm": "mock-hmac-sha256",
            "signed_at_ms": 1_750_000_000_000_i64,
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["signed_record_kind"].is_string());
        assert!(parsed["signed_record_id"].is_string());
        assert!(parsed["signature_algorithm"].is_string());
        assert!(parsed["signed_at_ms"].is_i64());
    }

    /// S355 — pin the documented `personnel.access_granted` payload shape.
    /// `granted_by` (the authorizing operator) and `reason` are the
    /// two-person-integrity + justification anchors and must both survive.
    #[test]
    fn s355_personnel_access_granted_payload_serializes() {
        let payload = serde_json::json!({
            "operator_user_id": "mock-op-001",
            "resource_kind": "cui_document",
            "resource_id": "doc_01HZ",
            "granted_by": "supervisor-007",
            "reason": "assigned to contract line 3",
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["resource_kind"].is_string());
        assert!(parsed["resource_id"].is_string());
        assert!(parsed["granted_by"].is_string());
        assert!(parsed["reason"].is_string());
    }

    /// S355 — pin the documented `personnel.access_denied` payload shape.
    /// `denied_reason` is the loud-fail justification (CLAUDE.md rule 12) and
    /// must be carried on every denial.
    #[test]
    fn s355_personnel_access_denied_payload_serializes() {
        let payload = serde_json::json!({
            "operator_user_id": "mock-op-001",
            "resource_kind": "export_controlled_drawing",
            "resource_id": "dwg_01HZ",
            "denied_reason": "export_screening_failed",
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["resource_kind"].is_string());
        assert!(parsed["resource_id"].is_string());
        assert!(parsed["denied_reason"].is_string());
    }

    // ── S357 / PR-44 (ADR-0074) — material.* traceability family ───────────
    //
    // Two kinds open the new `material.*` prefix family: the cert-attach
    // RECORD event and the heat/lot-assign STATE TRANSITION. Per the brief
    // each variant gets a focused round-trip test, each payload shape is
    // pinned by a serialization test, the family shares one
    // prefix-and-distinctness test, and a no-NAV-bytes pin lives with the
    // exhaustive-match consumers (`aberp-verify`, `export_invoice_bundle`).

    #[test]
    fn s357_material_cert_attached_round_trips() {
        let k = EventKind::MaterialCertAttached;
        let s = k.as_str();
        assert_eq!(s, "material.cert_attached");
        assert!(s.starts_with("material."), "{s} must start with material.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s357_material_heat_lot_assigned_round_trips() {
        let k = EventKind::MaterialHeatLotAssigned;
        let s = k.as_str();
        assert_eq!(s, "material.heat_lot_assigned");
        assert!(s.starts_with("material."), "{s} must start with material.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    /// S357 — the two `material.*` kinds share the new prefix family, carry
    /// NO other prefix, and are distinct from each other AND from a sample of
    /// every other prefix family. A collision (or a wrong prefix) would
    /// either mis-bucket a traceability row into a fiscal / manufacturing /
    /// quoting / access-trail bucket OR let the per-OUTGOING-invoice export
    /// bundle's `invoice.*` glob sweep a material-traceability row — both are
    /// the silent-omission failure mode CLAUDE.md rule 12 names.
    #[test]
    fn s357_material_kinds_use_material_prefix_and_are_distinct() {
        let new = [
            EventKind::MaterialCertAttached,
            EventKind::MaterialHeatLotAssigned,
        ];
        for k in &new {
            let s = k.as_str();
            assert!(s.starts_with("material."), "{s} must start with material.");
            for foreign in [
                "invoice.",
                "system.",
                "mes.",
                "quote.",
                "inventory.",
                "email.",
                "personnel.",
            ] {
                assert!(!s.starts_with(foreign), "{s} must not start with {foreign}");
            }
        }
        // Distinct within the family.
        assert_ne!(
            new[0].as_str(),
            new[1].as_str(),
            "{} collides with {}",
            new[0].as_str(),
            new[1].as_str()
        );
        // Distinct from a sample sibling per other prefix family — including
        // the `Material*`-named-but-not-`material.*`-prefixed kinds, which
        // live under `quote.*` (catalogue) and `inventory.*` (reservation).
        let strs: Vec<&str> = new.iter().map(EventKind::as_str).collect();
        for k in &strs {
            for sibling in [
                EventKind::InvoiceDraftCreated,
                EventKind::FirstProdLaunchAcknowledged,
                EventKind::MesAdapterEvent,
                EventKind::QuotePricingOperatorAccepted,
                EventKind::MaterialCatalogueChanged,
                EventKind::MaterialReserved,
                EventKind::EmailRelaySent,
                EventKind::PersonnelIdRegistered,
            ] {
                assert_ne!(
                    *k,
                    sibling.as_str(),
                    "{k} collides with foreign-family {}",
                    sibling.as_str()
                );
            }
        }
    }

    /// S357 — pin the documented `material.cert_attached` payload shape so a
    /// future firing site has a stable contract. The kind stores a free-form
    /// `serde_json::Value` (same posture as the `personnel.*` and recent
    /// `quote.*` kinds); this asserts the documented fields are present with
    /// the documented JSON types after a serialize → parse round-trip. The
    /// optional `lot_id` is exercised so the lot-scoped attach is pinned too.
    #[test]
    fn s357_material_cert_attached_payload_serializes() {
        let payload = serde_json::json!({
            "material_id": "6061-T6",
            "cert_kind": "mill_cert",
            "cert_url": "https://certs.example/coc/abc123.pdf",
            "attached_at_ms": 1_750_000_000_000_i64,
            "operator_user_id": "mock-op-001",
            "lot_id": "LOT-2026-0042",
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["material_id"].is_string());
        assert!(parsed["cert_kind"].is_string());
        assert!(parsed["cert_url"].is_string());
        assert!(parsed["attached_at_ms"].is_i64());
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["lot_id"].is_string());
    }

    /// S357 — pin the documented `material.heat_lot_assigned` payload shape.
    /// `lot_id` + `heat_id` are the load-bearing traceability anchors (the
    /// firing site validates them through `aberp_compliance::lot_heat`
    /// before they reach the ledger) and must both survive as strings;
    /// `source_supplier` is optional.
    #[test]
    fn s357_material_heat_lot_assigned_payload_serializes() {
        let payload = serde_json::json!({
            "material_id": "Ti-6Al-4V",
            "lot_id": "LOT-2026-0042",
            "heat_id": "HEAT-9F3A",
            "source_supplier": "ACME-METALS-AVL-007",
            "assigned_at_ms": 1_750_000_000_000_i64,
            "operator_user_id": "mock-op-001",
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["material_id"].is_string());
        assert!(parsed["lot_id"].is_string());
        assert!(parsed["heat_id"].is_string());
        assert!(parsed["source_supplier"].is_string());
        assert!(parsed["assigned_at_ms"].is_i64());
        assert!(parsed["operator_user_id"].is_string());
    }

    // ── S358 / PR-45 (ADR-0075) — part.* per-unit serialization family ──────
    //
    // Two kinds open the new `part.*` prefix family (the ninth): the serial-
    // assign RECORD event and the UID-mark STATE TRANSITION. Per the brief each
    // variant gets a focused round-trip test, each payload shape is pinned by a
    // serialization test, the family shares one prefix-and-distinctness test,
    // and a no-NAV-bytes pin lives with the exhaustive-match consumers
    // (`aberp-verify`, `export_invoice_bundle`).

    #[test]
    fn s358_part_serial_assigned_round_trips() {
        let k = EventKind::PartSerialAssigned;
        let s = k.as_str();
        assert_eq!(s, "part.serial_assigned");
        assert!(s.starts_with("part."), "{s} must start with part.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s358_part_uid_marked_round_trips() {
        let k = EventKind::PartUidMarked;
        let s = k.as_str();
        assert_eq!(s, "part.uid_marked");
        assert!(s.starts_with("part."), "{s} must start with part.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    /// S358 — the two `part.*` kinds share the new prefix family, carry NO
    /// other prefix, and are distinct from each other AND from a sample of
    /// every other prefix family. A collision (or a wrong prefix) would either
    /// mis-bucket a serialization row into a fiscal / manufacturing / quoting /
    /// access-trail / material-traceability bucket OR let the per-OUTGOING-
    /// invoice export bundle's `invoice.*` glob sweep a part-serialization row —
    /// both are the silent-omission failure mode CLAUDE.md rule 12 names.
    #[test]
    fn s358_part_kinds_use_part_prefix_and_are_distinct() {
        let new = [EventKind::PartSerialAssigned, EventKind::PartUidMarked];
        for k in &new {
            let s = k.as_str();
            assert!(s.starts_with("part."), "{s} must start with part.");
            for foreign in [
                "invoice.",
                "system.",
                "mes.",
                "quote.",
                "inventory.",
                "email.",
                "personnel.",
                "material.",
            ] {
                assert!(!s.starts_with(foreign), "{s} must not start with {foreign}");
            }
        }
        // Distinct within the family.
        assert_ne!(
            new[0].as_str(),
            new[1].as_str(),
            "{} collides with {}",
            new[0].as_str(),
            new[1].as_str()
        );
        // Distinct from a sample sibling per other prefix family.
        let strs: Vec<&str> = new.iter().map(EventKind::as_str).collect();
        for k in &strs {
            for sibling in [
                EventKind::InvoiceDraftCreated,
                EventKind::FirstProdLaunchAcknowledged,
                EventKind::MesAdapterEvent,
                EventKind::QuotePricingOperatorAccepted,
                EventKind::MaterialCatalogueChanged,
                EventKind::MaterialReserved,
                EventKind::EmailRelaySent,
                EventKind::PersonnelIdRegistered,
                EventKind::MaterialCertAttached,
            ] {
                assert_ne!(
                    *k,
                    sibling.as_str(),
                    "{k} collides with foreign-family {}",
                    sibling.as_str()
                );
            }
        }
    }

    /// S358 — pin the documented `part.serial_assigned` payload shape so a
    /// future firing site has a stable contract. The kind stores a free-form
    /// `serde_json::Value` (same posture as the `material.*` / `personnel.*`
    /// kinds); this asserts the documented fields are present with the
    /// documented JSON types after a serialize → parse round-trip. The optional
    /// `related_invoice_id` / `related_work_order_id` are exercised so the
    /// document-linked assignment is pinned too.
    #[test]
    fn s358_part_serial_assigned_payload_serializes() {
        let payload = serde_json::json!({
            "part_id": "PRT-7781",
            "serial_number": "SN-0001",
            "assigned_at_ms": 1_750_000_000_000_i64,
            "operator_user_id": "mock-op-001",
            "related_invoice_id": "INV-2026-0042",
            "related_work_order_id": "WO-2026-0099",
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["part_id"].is_string());
        assert!(parsed["serial_number"].is_string());
        assert!(parsed["assigned_at_ms"].is_i64());
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["related_invoice_id"].is_string());
        assert!(parsed["related_work_order_id"].is_string());
    }

    /// S358 — pin the documented `part.uid_marked` payload shape. `uid_iri` is
    /// the rendered MIL-STD-130N IRI (the firing site builds it through
    /// `aberp_compliance::uid` before it reaches the ledger);
    /// `mil_std_130_compliant` is the bool format-gate verdict and must survive
    /// as a bool.
    #[test]
    fn s358_part_uid_marked_payload_serializes() {
        let payload = serde_json::json!({
            "part_id": "PRT-7781",
            "uid_iri": "0LH1234567ABC123SN-0001",
            "uid_construct_code": "construct_1",
            "mil_std_130_compliant": true,
            "marked_at_ms": 1_750_000_000_000_i64,
            "operator_user_id": "mock-op-001",
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["part_id"].is_string());
        assert!(parsed["uid_iri"].is_string());
        assert!(parsed["uid_construct_code"].is_string());
        assert!(parsed["mil_std_130_compliant"].is_boolean());
        assert!(parsed["marked_at_ms"].is_i64());
        assert!(parsed["operator_user_id"].is_string());
    }

    // ── S359 / PR-46 (ADR-0076) — export.* export-control family ────────────
    //
    // Three kinds open the new `export.*` prefix family (the tenth): the
    // classification RECORD event, the access-check DECISION event, and the
    // shipment-logged PHYSICAL-EXPORT event. Per the brief each variant gets a
    // focused round-trip test, each payload shape is pinned by a serialization
    // test, the family shares one prefix-and-distinctness test, and a no-NAV-
    // bytes pin lives with the exhaustive-match consumer (`export_invoice_bundle`).

    #[test]
    fn s359_export_classification_set_round_trips() {
        let k = EventKind::ExportClassificationSet;
        let s = k.as_str();
        assert_eq!(s, "export.classification_set");
        assert!(s.starts_with("export."), "{s} must start with export.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s359_export_access_check_round_trips() {
        let k = EventKind::ExportAccessCheck;
        let s = k.as_str();
        assert_eq!(s, "export.access_check");
        assert!(s.starts_with("export."), "{s} must start with export.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s359_export_shipment_logged_round_trips() {
        let k = EventKind::ExportShipmentLogged;
        let s = k.as_str();
        assert_eq!(s, "export.shipment_logged");
        assert!(s.starts_with("export."), "{s} must start with export.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    /// S359 — the three `export.*` kinds share the new prefix family, carry NO
    /// other prefix, and are distinct from each other AND from a sample of
    /// every other prefix family. A collision (or a wrong prefix) would either
    /// mis-bucket an export-control row into a fiscal / manufacturing / quoting
    /// / access-trail / material-traceability / serialization bucket OR let the
    /// per-OUTGOING-invoice export bundle's `invoice.*` glob sweep an export-
    /// control row — both are the silent-omission failure mode CLAUDE.md rule
    /// 12 names.
    #[test]
    fn s359_export_kinds_use_export_prefix_and_are_distinct() {
        let new = [
            EventKind::ExportClassificationSet,
            EventKind::ExportAccessCheck,
            EventKind::ExportShipmentLogged,
        ];
        for k in &new {
            let s = k.as_str();
            assert!(s.starts_with("export."), "{s} must start with export.");
            for foreign in [
                "invoice.",
                "system.",
                "mes.",
                "quote.",
                "inventory.",
                "email.",
                "personnel.",
                "material.",
                "part.",
            ] {
                assert!(!s.starts_with(foreign), "{s} must not start with {foreign}");
            }
        }
        // Distinct within the family.
        let strs: Vec<&str> = new.iter().map(EventKind::as_str).collect();
        for i in 0..strs.len() {
            for j in (i + 1)..strs.len() {
                assert_ne!(strs[i], strs[j], "{} collides with {}", strs[i], strs[j]);
            }
        }
        // Distinct from a sample sibling per other prefix family.
        for k in &strs {
            for sibling in [
                EventKind::InvoiceDraftCreated,
                EventKind::FirstProdLaunchAcknowledged,
                EventKind::MesAdapterEvent,
                EventKind::QuotePricingOperatorAccepted,
                EventKind::MaterialCatalogueChanged,
                EventKind::MaterialReserved,
                EventKind::EmailRelaySent,
                EventKind::PersonnelIdRegistered,
                EventKind::MaterialCertAttached,
                EventKind::PartSerialAssigned,
            ] {
                assert_ne!(
                    *k,
                    sibling.as_str(),
                    "{k} collides with foreign-family {}",
                    sibling.as_str()
                );
            }
        }
    }

    /// S359 — pin the documented `export.classification_set` payload shape so a
    /// future firing site has a stable contract. The kind stores a free-form
    /// `serde_json::Value` (same posture as the `part.*` / `material.*` kinds);
    /// this asserts the documented fields are present with the documented JSON
    /// types after a serialize → parse round-trip. The optional `eccn` /
    /// `usml_category` are exercised so the EAR-listed / ITAR-controlled shapes
    /// are pinned too.
    #[test]
    fn s359_export_classification_set_payload_serializes() {
        let payload = serde_json::json!({
            "entity_kind": "drawing",
            "entity_id": "DWG-7781-A",
            "eccn": "7A994",
            "usml_category": "VIII(h)",
            "jurisdiction": "ITAR",
            "operator_user_id": "mock-op-001",
            "classified_at_ms": 1_750_000_000_000_i64,
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["entity_kind"].is_string());
        assert!(parsed["entity_id"].is_string());
        assert!(parsed["eccn"].is_string());
        assert!(parsed["usml_category"].is_string());
        assert!(parsed["jurisdiction"].is_string());
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["classified_at_ms"].is_i64());
    }

    /// S359 — pin the documented `export.access_check` payload shape. `decision`
    /// must survive as the `"granted"` / `"denied"` string the firing site
    /// writes.
    #[test]
    fn s359_export_access_check_payload_serializes() {
        let payload = serde_json::json!({
            "entity_kind": "spec",
            "entity_id": "SPEC-3001",
            "operator_user_id": "mock-op-002",
            "decision": "denied",
            "reason": "requester not a U.S. person (ITAR 22 CFR 120.62)",
            "checked_at_ms": 1_750_000_000_000_i64,
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["entity_kind"].is_string());
        assert!(parsed["entity_id"].is_string());
        assert!(parsed["operator_user_id"].is_string());
        assert_eq!(parsed["decision"], "denied");
        assert!(parsed["reason"].is_string());
        assert!(parsed["checked_at_ms"].is_i64());
    }

    /// S359 — pin the documented `export.shipment_logged` payload shape. The
    /// optional `ecn_or_authorization` (the cited licence / exception / ECCN) is
    /// exercised so the authorized-export shape is pinned.
    #[test]
    fn s359_export_shipment_logged_payload_serializes() {
        let payload = serde_json::json!({
            "shipment_id": "SHP-2026-0042",
            "exporter_party_id": "PTY-EXPORTER-1",
            "recipient_party_id": "PTY-CONSIGNEE-9",
            "recipient_country": "DE",
            "ecn_or_authorization": "License Exception STA",
            "shipped_at_ms": 1_750_000_000_000_i64,
            "operator_user_id": "mock-op-003",
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["shipment_id"].is_string());
        assert!(parsed["exporter_party_id"].is_string());
        assert!(parsed["recipient_party_id"].is_string());
        assert!(parsed["recipient_country"].is_string());
        assert!(parsed["ecn_or_authorization"].is_string());
        assert!(parsed["shipped_at_ms"].is_i64());
        assert!(parsed["operator_user_id"].is_string());
    }

    // ── S360 / PR-47 (ADR-0077) — cui.* Controlled-Unclassified-Information ──
    //
    // Two kinds open the new `cui.*` prefix family (the eleventh): the
    // marking-applied RECORD event and the access-event DECISION event. Per the
    // brief each variant gets a focused round-trip test, each payload shape is
    // pinned by a serialization test, the family shares one prefix-and-
    // distinctness test, and a no-NAV-bytes pin lives with the exhaustive-match
    // consumer (`export_invoice_bundle`).

    #[test]
    fn s360_cui_marking_applied_round_trips() {
        let k = EventKind::CuiMarkingApplied;
        let s = k.as_str();
        assert_eq!(s, "cui.marking_applied");
        assert!(s.starts_with("cui."), "{s} must start with cui.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s360_cui_access_event_round_trips() {
        let k = EventKind::CuiAccessEvent;
        let s = k.as_str();
        assert_eq!(s, "cui.access_event");
        assert!(s.starts_with("cui."), "{s} must start with cui.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    /// S360 — the two `cui.*` kinds share the new prefix family, carry NO other
    /// prefix, and are distinct from each other AND from a sample of every other
    /// prefix family. A collision (or a wrong prefix) would either mis-bucket a
    /// CUI row into a fiscal / manufacturing / quoting / access-trail /
    /// material-traceability / serialization / export-control bucket OR let the
    /// per-OUTGOING-invoice export bundle's `invoice.*` glob sweep a CUI row —
    /// both are the silent-omission failure mode CLAUDE.md rule 12 names.
    #[test]
    fn s360_cui_kinds_use_cui_prefix_and_are_distinct() {
        let new = [EventKind::CuiMarkingApplied, EventKind::CuiAccessEvent];
        for k in &new {
            let s = k.as_str();
            assert!(s.starts_with("cui."), "{s} must start with cui.");
            for foreign in [
                "invoice.",
                "system.",
                "mes.",
                "quote.",
                "inventory.",
                "email.",
                "personnel.",
                "material.",
                "part.",
                "export.",
            ] {
                assert!(!s.starts_with(foreign), "{s} must not start with {foreign}");
            }
        }
        // Distinct within the family.
        let strs: Vec<&str> = new.iter().map(EventKind::as_str).collect();
        for i in 0..strs.len() {
            for j in (i + 1)..strs.len() {
                assert_ne!(strs[i], strs[j], "{} collides with {}", strs[i], strs[j]);
            }
        }
        // Distinct from a sample sibling per other prefix family.
        for k in &strs {
            for sibling in [
                EventKind::InvoiceDraftCreated,
                EventKind::FirstProdLaunchAcknowledged,
                EventKind::MesAdapterEvent,
                EventKind::QuotePricingOperatorAccepted,
                EventKind::MaterialCatalogueChanged,
                EventKind::MaterialReserved,
                EventKind::EmailRelaySent,
                EventKind::PersonnelIdRegistered,
                EventKind::MaterialCertAttached,
                EventKind::PartSerialAssigned,
                EventKind::ExportClassificationSet,
            ] {
                assert_ne!(
                    *k,
                    sibling.as_str(),
                    "{k} collides with foreign-family {}",
                    sibling.as_str()
                );
            }
        }
    }

    /// S360 — pin the documented `cui.marking_applied` payload shape so a future
    /// firing site has a stable contract. The kind stores a free-form
    /// `serde_json::Value` (same posture as the `export.*` / `part.*` kinds);
    /// this asserts the documented fields are present with the documented JSON
    /// types after a serialize → parse round-trip. `cui_marking_str` is the
    /// rendered DoD banner — the controlled content itself is never carried
    /// (no PII / no classified body at rest).
    #[test]
    fn s360_cui_marking_applied_payload_serializes() {
        let payload = serde_json::json!({
            "entity_kind": "drawing",
            "entity_id": "DWG-7781-A",
            "cui_marking_str": "CUI//SP-CTI//NOFORN",
            "operator_user_id": "mock-op-001",
            "applied_at_ms": 1_750_000_000_000_i64,
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["entity_kind"].is_string());
        assert!(parsed["entity_id"].is_string());
        assert!(parsed["cui_marking_str"].is_string());
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["applied_at_ms"].is_i64());
    }

    /// S360 — pin the documented `cui.access_event` payload shape. `decision`
    /// must survive as the `"granted"` / `"denied"` string the firing site
    /// writes.
    #[test]
    fn s360_cui_access_event_payload_serializes() {
        let payload = serde_json::json!({
            "entity_kind": "spec",
            "entity_id": "SPEC-3001",
            "operator_user_id": "mock-op-002",
            "decision": "denied",
            "reason": "no lawful government purpose on file (32 CFR 2002.4)",
            "accessed_at_ms": 1_750_000_000_000_i64,
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["entity_kind"].is_string());
        assert!(parsed["entity_id"].is_string());
        assert!(parsed["operator_user_id"].is_string());
        assert_eq!(parsed["decision"], "denied");
        assert!(parsed["reason"].is_string());
        assert!(parsed["accessed_at_ms"].is_i64());
    }

    // ── S361 / PR-48 (ADR-0078) — supplier.* Approved-Vendor-List family ──
    //
    // Two kinds open the new `supplier.*` prefix family (the twelfth): the
    // DPAS-priority-set RECORD event and the export-screened DECISION event.
    // Per the brief each variant gets a focused round-trip test, each payload
    // shape is pinned by a serialization test, the family shares one prefix-
    // and-distinctness test, and a no-NAV-bytes pin lives with the exhaustive-
    // match consumer (`export_invoice_bundle`).

    #[test]
    fn s361_supplier_dpas_priority_set_round_trips() {
        let k = EventKind::SupplierDpasPrioritySet;
        let s = k.as_str();
        assert_eq!(s, "supplier.dpas_priority_set");
        assert!(s.starts_with("supplier."), "{s} must start with supplier.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    #[test]
    fn s361_supplier_export_screened_round_trips() {
        let k = EventKind::SupplierExportScreened;
        let s = k.as_str();
        assert_eq!(s, "supplier.export_screened");
        assert!(s.starts_with("supplier."), "{s} must start with supplier.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    /// S361 — the two `supplier.*` kinds share the new prefix family, carry NO
    /// other prefix, and are distinct from each other AND from a sample of
    /// every other prefix family. A collision (or a wrong prefix) would either
    /// mis-bucket an AVL row into a fiscal / manufacturing / quoting /
    /// access-trail / material-traceability / serialization / CUI / export
    /// bucket OR let the per-OUTGOING-invoice export bundle's `invoice.*` glob
    /// sweep an AVL row — both are the silent-omission failure mode CLAUDE.md
    /// rule 12 names.
    #[test]
    fn s361_supplier_kinds_use_supplier_prefix_and_are_distinct() {
        let new = [
            EventKind::SupplierDpasPrioritySet,
            EventKind::SupplierExportScreened,
        ];
        for k in &new {
            let s = k.as_str();
            assert!(s.starts_with("supplier."), "{s} must start with supplier.");
            for foreign in [
                "invoice.",
                "system.",
                "mes.",
                "quote.",
                "inventory.",
                "email.",
                "personnel.",
                "material.",
                "part.",
                "export.",
                "cui.",
            ] {
                assert!(!s.starts_with(foreign), "{s} must not start with {foreign}");
            }
        }
        // Distinct within the family.
        let strs: Vec<&str> = new.iter().map(EventKind::as_str).collect();
        for i in 0..strs.len() {
            for j in (i + 1)..strs.len() {
                assert_ne!(strs[i], strs[j], "{} collides with {}", strs[i], strs[j]);
            }
        }
        // Distinct from a sample sibling per other prefix family.
        for k in &strs {
            for sibling in [
                EventKind::InvoiceDraftCreated,
                EventKind::FirstProdLaunchAcknowledged,
                EventKind::MesAdapterEvent,
                EventKind::QuotePricingOperatorAccepted,
                EventKind::MaterialReserved,
                EventKind::EmailRelaySent,
                EventKind::PersonnelIdRegistered,
                EventKind::MaterialCertAttached,
                EventKind::PartSerialAssigned,
                EventKind::ExportClassificationSet,
                EventKind::CuiMarkingApplied,
            ] {
                assert_ne!(
                    *k,
                    sibling.as_str(),
                    "{k} collides with foreign-family {}",
                    sibling.as_str()
                );
            }
        }
    }

    /// S361 — pin the documented `supplier.dpas_priority_set` payload shape so a
    /// future firing site has a stable contract. The kind stores a free-form
    /// `serde_json::Value` (same posture as the `export.*` / `cui.*` kinds);
    /// this asserts the documented fields are present with the documented JSON
    /// types after a serialize → parse round-trip. `dpas_rating` is the rendered
    /// [`aberp_compliance::avl::DpasRating`] string — no PII at rest.
    #[test]
    fn s361_supplier_dpas_priority_set_payload_serializes() {
        let payload = serde_json::json!({
            "partner_id": "partner-4711",
            "dpas_rating": "DX-A7",
            "operator_user_id": "mock-op-001",
            "set_at_ms": 1_750_000_000_000_i64,
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["partner_id"].is_string());
        assert!(parsed["dpas_rating"].is_string());
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["set_at_ms"].is_i64());
    }

    /// S361 — pin the documented `supplier.export_screened` payload shape.
    /// `screening_result` must survive as the `"clear"` / `"hit"` /
    /// `"inconclusive"` string the firing site writes; `hit_details` is optional
    /// (present only on a hit / inconclusive).
    #[test]
    fn s361_supplier_export_screened_payload_serializes() {
        let payload = serde_json::json!({
            "partner_id": "partner-4711",
            "screening_result": "hit",
            "screening_source": "mock-bis-csl",
            "screened_at_ms": 1_750_000_000_000_i64,
            "operator_user_id": "mock-op-002",
            "hit_details": "BIS Entity List partial-name match (manual review)",
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["partner_id"].is_string());
        assert_eq!(parsed["screening_result"], "hit");
        assert!(parsed["screening_source"].is_string());
        assert!(parsed["screened_at_ms"].is_i64());
        assert!(parsed["operator_user_id"].is_string());
        assert!(parsed["hit_details"].is_string());
    }

    // ── S362 / PR-49 (ADR-0079) — incident.* cyber-incident-reporting family ──
    //
    // One kind opens the new `incident.*` prefix family (the thirteenth): the
    // cyber-incident DETECTION event that starts the DFARS 252.204-7012(c)(1)
    // 72-hour reporting clock. A focused round-trip test, a payload-shape pin
    // (all fields populated), and a prefix-and-distinctness test. The no-NAV-
    // bytes pin lives with the exhaustive-match consumer (`export_invoice_bundle`).

    #[test]
    fn s362_incident_cyber_detected_round_trips() {
        let k = EventKind::IncidentCyberDetected;
        let s = k.as_str();
        assert_eq!(s, "incident.cyber_detected");
        assert!(s.starts_with("incident."), "{s} must start with incident.");
        assert_eq!(
            EventKind::from_storage_str(s).expect("round-trip"),
            k,
            "round-trip mismatch for {s}"
        );
    }

    /// S362 — the lone `incident.*` kind opens the new prefix family, carries NO
    /// other prefix, and is distinct from a sample of every other prefix family.
    /// A wrong prefix would either mis-bucket a cyber-incident row into a fiscal
    /// / manufacturing / quoting / access-trail / material-traceability /
    /// serialization / CUI / export / supplier bucket OR let the per-OUTGOING-
    /// invoice export bundle's `invoice.*` glob sweep an incident row — both are
    /// the silent-omission failure mode CLAUDE.md rule 12 names.
    #[test]
    fn s362_incident_kind_uses_incident_prefix_and_is_distinct() {
        let k = EventKind::IncidentCyberDetected;
        let s = k.as_str();
        assert!(s.starts_with("incident."), "{s} must start with incident.");
        for foreign in [
            "invoice.",
            "system.",
            "mes.",
            "quote.",
            "inventory.",
            "email.",
            "personnel.",
            "material.",
            "part.",
            "export.",
            "cui.",
            "supplier.",
        ] {
            assert!(!s.starts_with(foreign), "{s} must not start with {foreign}");
        }
        // Distinct from a sample sibling per other prefix family.
        for sibling in [
            EventKind::InvoiceDraftCreated,
            EventKind::FirstProdLaunchAcknowledged,
            EventKind::MesAdapterEvent,
            EventKind::QuotePricingOperatorAccepted,
            EventKind::MaterialReserved,
            EventKind::EmailRelaySent,
            EventKind::PersonnelIdRegistered,
            EventKind::MaterialCertAttached,
            EventKind::PartSerialAssigned,
            EventKind::ExportClassificationSet,
            EventKind::CuiMarkingApplied,
            EventKind::SupplierDpasPrioritySet,
        ] {
            assert_ne!(
                s,
                sibling.as_str(),
                "{s} collides with foreign-family {}",
                sibling.as_str()
            );
        }
    }

    /// S362 — pin the documented `incident.cyber_detected` payload shape with
    /// ALL fields populated (including the two optionals `mitigation_notes` and
    /// `dod_72h_report_due_at_ms`) so a future firing site has a stable
    /// contract. The kind stores a free-form `serde_json::Value` (same posture
    /// as the `export.*` / `cui.*` / `supplier.*` kinds); this asserts the
    /// documented fields are present with the documented JSON types after a
    /// serialize → parse round-trip. `severity` / `detection_source` are the
    /// rendered [`aberp_compliance::incident`] enum strings; `scope_description`
    /// is a summary, never raw log dumps — no PII / controlled content at rest.
    #[test]
    fn s362_incident_cyber_detected_payload_serializes() {
        let detected_at_ms = 1_750_000_000_000_i64;
        let payload = serde_json::json!({
            "detected_at_ms": detected_at_ms,
            "operator_user_id": "mock-op-007",
            "severity": "high",
            "scope_description": "Anomalous outbound traffic from CAD workstation segment",
            "cdi_affected": true,
            "ocs_affected": false,
            "cui_affected": true,
            "exfiltration_suspected": false,
            "affected_systems": ["cad-ws-04", "file-srv-02"],
            "detection_source": "siem",
            "mitigation_notes": "Segment isolated; credentials rotated.",
            "dod_72h_report_due_at_ms": detected_at_ms + 72 * 60 * 60 * 1000,
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(parsed["detected_at_ms"].is_i64());
        assert!(parsed["operator_user_id"].is_string());
        assert_eq!(parsed["severity"], "high");
        assert!(parsed["scope_description"].is_string());
        assert_eq!(parsed["cdi_affected"], true);
        assert_eq!(parsed["ocs_affected"], false);
        assert_eq!(parsed["cui_affected"], true);
        assert_eq!(parsed["exfiltration_suspected"], false);
        assert!(parsed["affected_systems"].is_array());
        assert_eq!(parsed["affected_systems"][0], "cad-ws-04");
        assert_eq!(parsed["detection_source"], "siem");
        assert!(parsed["mitigation_notes"].is_string());
        assert!(parsed["dod_72h_report_due_at_ms"].is_i64());
        // The 72-hour deadline is exactly 72h after detection.
        assert_eq!(
            parsed["dod_72h_report_due_at_ms"].as_i64().unwrap() - detected_at_ms,
            72 * 60 * 60 * 1000
        );
    }
}
