//! Typed read-side queries over the audit ledger.
//!
//! PR-8 introduces the operator-unblock commands `retry-submission` and
//! `mark-abandoned`. Both share the same precondition: the invoice must
//! be in the `SubmissionStuck` posture per ADR-0009 §5 — meaning a
//! prior `InvoiceSubmissionResponse` exists (so we have a NAV
//! `transaction_id`), no `InvoiceMarkedAbandoned` has yet been recorded
//! for it, and the most-recent `InvoiceAckStatus` (if any) is
//! non-terminal (`RECEIVED` / `PROCESSING`) — i.e. the poll loop did
//! not reach `SAVED` or `ABORTED`.
//!
//! The "no completed terminal poll yet" check is the source of truth
//! for stuck-ness per the PR-7-B-3 A5/A6 design assumption: the
//! `submission_state` fact lives in the audit ledger, not in a billing
//! column. Both PR-8 commands consult this helper rather than each
//! re-implementing the rules — divergence between two copies is
//! exactly the failure mode CLAUDE.md rule 7 names ("surface
//! conflicts, don't average them").
//!
//! # PR-19 / ADR-0032 §4 — state-2 Pending extension
//!
//! Under the two-tx posture ADR-0032 §1 names, an invoice can sit
//! in a third recoverable state: an `InvoiceSubmissionAttempt` exists
//! (TX1 committed) but no `InvoiceSubmissionResponse` and no
//! `InvoiceMarkedAbandoned` (TX2 never committed — wire broke, process
//! crashed mid-flight, or NAV returned an error which produced an
//! `InvoiceSubmissionAttemptFailed` entry instead of a Response). This
//! is state-2 Pending. The precondition walker classifies it as
//! `Stuck(StuckStage::Pending)` so the same `retry-submission` /
//! `mark-abandoned` commands can recover it. State-3 (the existing
//! Response-without-terminal-ack posture) classifies as
//! `Stuck(StuckStage::AwaitingAck)`.
//!
//! # Why a separate module
//!
//! `submit_invoice.rs` and `poll_ack.rs` each carry their own audit
//! reads inline (a single lookup of the `transaction_id`). PR-8's
//! preconditions are not just one lookup — they walk every
//! `EventKind` that could move the invoice out of `Stuck` (a
//! finalize, a rejection, an abandonment) plus the prior
//! `InvoiceAckStatus` entries. Inlining all of that into each
//! orchestration module would duplicate ~50 LoC of decode-and-filter
//! per command, and would mean the F12 trap re-asserts itself silently
//! the next time a new `EventKind` lands that the precondition must
//! consider.

use aberp_audit_ledger::{Entry, EventKind, Ledger};
use aberp_billing::IdempotencyKey;
use anyhow::{anyhow, Context, Result};

use crate::audit_payloads;

/// Which stage of stuck-ness the invoice is in. PR-19 / ADR-0032 §4
/// introduces the two-stage distinction; pre-PR-19 the only stage
/// was `AwaitingAck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckStage {
    /// **State-2 (PR-19 / ADR-0032 §4).** An `InvoiceSubmissionAttempt`
    /// exists for this invoice but no `InvoiceSubmissionResponse`
    /// (and no `InvoiceMarkedAbandoned`). The Attempt was committed
    /// in TX1 but TX2 never committed — either the wire broke
    /// mid-flight, the process crashed before TX2, or NAV returned
    /// an error which produced an `InvoiceSubmissionAttemptFailed`
    /// entry instead of a Response.
    ///
    /// Recovery: `retry-submission` produces a fresh Attempt /
    /// Response (or AttemptFailed) pair. The Layer-2
    /// `queryInvoiceCheck` surface (ADR-0009 §5; named-deferred per
    /// ADR-0032 §"Open questions") would let the retry disambiguate
    /// "NAV already has this submission" from "the wire broke before
    /// NAV saw it" — until it lands, the state-2 retry produces a
    /// potential duplicate submission to NAV (loud-warned in the
    /// operator-visible summary per ADR-0032 §"Adversarial review"
    /// #2 + CLAUDE.md rule 12).
    Pending,
    /// **State-3 (the pre-PR-19 / PR-8 / ADR-0009 §5 surface).** An
    /// `InvoiceSubmissionResponse` exists for this invoice but no
    /// terminal `InvoiceAckStatus` (`SAVED` or `ABORTED`) and no
    /// `InvoiceMarkedAbandoned`. NAV accepted the submission but
    /// the ack poll either did not reach a terminal status or
    /// never ran.
    ///
    /// Recovery: `retry-submission` produces a fresh Attempt /
    /// Response pair (NAV will return a fresh `transaction_id`;
    /// Layer-2 `queryInvoiceCheck` would also disambiguate here if
    /// the prior Response actually finalized at NAV's side after
    /// ABERP gave up polling — same residual as state-2).
    AwaitingAck,
}

/// The audit-ledger view of a stuck invoice's precondition state.
/// Returned by [`stuck_precondition`] when the invoice IS stuck;
/// carries every field the PR-8 commands need to write their
/// own audit entries (the prior `transaction_id` and last ack status)
/// alongside the `IdempotencyKey` that links to the original issuance
/// per F8.
///
/// # PR-19 / ADR-0032 §4 — Option-shaped fields
///
/// `prior_transaction_id` becomes `Option<String>` to support state-2
/// `Pending` (where no prior `InvoiceSubmissionResponse` exists yet,
/// so no `transaction_id`). State-3 `AwaitingAck` retains the
/// `Some(transaction_id)` shape the pre-PR-19 callers depended on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckPrecondition {
    /// Which stage of stuck-ness the invoice is in. Drives the
    /// operator-visible summary and the audit-payload field
    /// values the orchestration writes. PR-19 / ADR-0032 §4.
    pub stage: StuckStage,
    /// The NAV `transaction_id` from the most-recent
    /// `InvoiceSubmissionResponse` for this invoice. `Some(...)` for
    /// state-3 `AwaitingAck`; `None` for state-2 `Pending` (no prior
    /// Response exists).
    pub prior_transaction_id: Option<String>,
    /// String form (`"RECEIVED"` / `"PROCESSING"`) of the most-recent
    /// `InvoiceAckStatus` payload's `ack_status` field. `None` if no
    /// ack entry exists yet (state-3 with no poll yet; or state-2,
    /// where no ack poll is possible without a Response). The
    /// operator can still retry/abandon at this point regardless of
    /// stage.
    pub prior_last_ack_status: Option<String>,
    /// The original issuance's idempotency key. The PR-8 audit entries
    /// thread it through per F8 ("every NAV-related entry for an
    /// invoice carries the SAME idempotency_key as the issuance
    /// entries"). For state-3 the key is taken from the prior
    /// `InvoiceSubmissionResponse`; for state-2 from the prior
    /// `InvoiceSubmissionAttempt` — both equal the original
    /// issuance's key per the F8 contract.
    pub idempotency_key: IdempotencyKey,
}

/// Reason the invoice is NOT in the stuck precondition. Carried as a
/// typed error so the caller's printed message and tracing log can
/// distinguish the cases without re-parsing a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotStuckReason {
    /// No `InvoiceSubmissionResponse` for this invoice — `submit-invoice`
    /// was never run successfully against it. The operator should run
    /// `aberp submit-invoice` first, not `retry-submission`.
    NeverSubmitted,
    /// The most-recent `InvoiceAckStatus` for this invoice is `"SAVED"`
    /// — the invoice has already finalized. No retry needed; no
    /// abandonment permitted.
    AlreadyFinalized,
    /// The most-recent `InvoiceAckStatus` for this invoice is
    /// `"ABORTED"` — NAV rejected it. A retry would not change the
    /// outcome (the sequence slot is gap-rejected per ADR-0009 §2);
    /// the operator must issue a corrective new invoice instead.
    /// Abandonment is also not the right command here — the invoice
    /// is already terminally Rejected, not Stuck.
    AlreadyRejected,
    /// An `InvoiceMarkedAbandoned` entry already exists — the operator
    /// previously chose to stop retrying. Both retry and re-abandon
    /// are no-ops; surfaced loud per CLAUDE.md rule 12.
    AlreadyAbandoned,
}

impl NotStuckReason {
    /// Human-readable string for the operator-visible summary +
    /// tracing log lines.
    pub fn as_message(&self) -> &'static str {
        match self {
            NotStuckReason::NeverSubmitted => {
                "no InvoiceSubmissionResponse exists for this invoice — \
                 run `aberp submit-invoice` first"
            }
            NotStuckReason::AlreadyFinalized => {
                "this invoice is already FINALIZED (last ack: SAVED) — \
                 nothing to retry or abandon"
            }
            NotStuckReason::AlreadyRejected => {
                "this invoice is already REJECTED (last ack: ABORTED) — \
                 issue a corrective new invoice rather than retrying"
            }
            NotStuckReason::AlreadyAbandoned => {
                "this invoice was previously marked abandoned — \
                 no further operator commands accepted"
            }
        }
    }
}

/// Result of [`stuck_precondition`]. The success case carries the
/// precondition state; the failure case carries the typed reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StuckOutcome {
    Stuck(StuckPrecondition),
    NotStuck(NotStuckReason),
}

/// Resolve the stuck precondition for `invoice_id` from the audit
/// ledger. Loud-fail surface for the orchestration callers.
///
/// Walks the ledger's entries in seq order (oldest → newest) and
/// classifies the state. The classification rules in one place
/// (PR-19 / ADR-0032 §4):
///
///   1. If any `InvoiceMarkedAbandoned` exists for this invoice →
///      `NotStuck(AlreadyAbandoned)`. (Terminal-by-operator-decision.)
///   2. Else: find the most-recent `InvoiceSubmissionResponse`.
///      - If present, walk acks for terminal status:
///        - `"SAVED"`     → `NotStuck(AlreadyFinalized)`
///        - `"ABORTED"`   → `NotStuck(AlreadyRejected)`
///        - any other     → `Stuck(StuckStage::AwaitingAck, ...)`
///                          carrying the ack string as
///                          `prior_last_ack_status`.
///        - no ack entry  → `Stuck(StuckStage::AwaitingAck, ...)`
///                          with `prior_last_ack_status = None`.
///      - If absent, fall through to step 3.
///   3. Else: find the most-recent `InvoiceSubmissionAttempt`.
///      - If present → `Stuck(StuckStage::Pending, prior_transaction_id=None,
///        prior_last_ack_status=None, idempotency_key=from Attempt)`.
///      - If absent → `NotStuck(NeverSubmitted)`.
///
/// The `IdempotencyKey` on the returned `StuckPrecondition` is taken
/// from the most-recent `InvoiceSubmissionResponse`'s payload for
/// state-3, OR from the most-recent `InvoiceSubmissionAttempt`'s
/// payload for state-2. Both equal the original issuance's key per
/// the F8 contract.
///
/// The presence of an `InvoiceSubmissionAttemptFailed` entry does
/// NOT change classification. An Attempt followed by an AttemptFailed
/// is still state-2 Pending (the operator may retry; multiple
/// failures accumulate in the audit chain as evidence). Per
/// ADR-0032 §4.
pub fn stuck_precondition(ledger: &Ledger, invoice_id: &str) -> Result<StuckOutcome> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries to resolve stuck precondition")?;

    // 1. Terminal-by-operator-decision wins over everything else.
    if has_marked_abandoned(&entries, invoice_id)? {
        return Ok(StuckOutcome::NotStuck(NotStuckReason::AlreadyAbandoned));
    }

    // 2. State-3 path: a Response exists.
    if let Some((prior_transaction_id, idempotency_key)) =
        latest_submission_response(&entries, invoice_id)?
    {
        let prior_last_ack_status = latest_ack_status(&entries, invoice_id)?;
        return match prior_last_ack_status.as_deref() {
            Some("SAVED") => Ok(StuckOutcome::NotStuck(NotStuckReason::AlreadyFinalized)),
            Some("ABORTED") => Ok(StuckOutcome::NotStuck(NotStuckReason::AlreadyRejected)),
            _ => Ok(StuckOutcome::Stuck(StuckPrecondition {
                stage: StuckStage::AwaitingAck,
                prior_transaction_id: Some(prior_transaction_id),
                prior_last_ack_status,
                idempotency_key,
            })),
        };
    }

    // 3. State-2 path (PR-19 / ADR-0032 §4): no Response, but an
    //    Attempt exists → Pending. The Attempt was committed in TX1
    //    but TX2 never committed (wire broke / crash mid-flight) or
    //    TX2 committed an AttemptFailed entry instead of a Response
    //    (NAV-side or transport-side failure surfaced).
    if let Some(idempotency_key) = latest_submission_attempt(&entries, invoice_id)? {
        return Ok(StuckOutcome::Stuck(StuckPrecondition {
            stage: StuckStage::Pending,
            prior_transaction_id: None,
            prior_last_ack_status: None,
            idempotency_key,
        }));
    }

    // 4. No Attempt, no Response → never submitted.
    Ok(StuckOutcome::NotStuck(NotStuckReason::NeverSubmitted))
}

/// True iff any `InvoiceMarkedAbandoned` entry with a payload whose
/// `invoice_id` matches has been written.
fn has_marked_abandoned(entries: &[Entry], invoice_id: &str) -> Result<bool> {
    for entry in entries {
        if entry.kind != EventKind::InvoiceMarkedAbandoned {
            continue;
        }
        // The payload type fully validates the shape; an unparseable
        // entry surfaces as a loud error (it would be tampered or
        // schema-drifted, both ADR-0008 ledger-integrity concerns).
        let payload: audit_payloads::InvoiceMarkedAbandonedPayload =
            serde_json::from_slice(&entry.payload).map_err(|e| {
                anyhow!(
                    "InvoiceMarkedAbandoned audit payload (seq {:?}) failed typed decode: {e}",
                    entry.seq
                )
            })?;
        if payload.invoice_id == invoice_id {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Most-recent (highest-seq) `InvoiceSubmissionResponse` for this
/// invoice id. Returns the prior `transaction_id` + the persisted
/// `idempotency_key`. `None` if no such entry exists.
fn latest_submission_response(
    entries: &[Entry],
    invoice_id: &str,
) -> Result<Option<(String, IdempotencyKey)>> {
    // Walk newest → oldest so the first match wins.
    for entry in entries.iter().rev() {
        if entry.kind != EventKind::InvoiceSubmissionResponse {
            continue;
        }
        let payload: audit_payloads::InvoiceSubmissionResponsePayload =
            serde_json::from_slice(&entry.payload).map_err(|e| {
                anyhow!(
                    "InvoiceSubmissionResponse audit payload (seq {:?}) failed typed decode: {e}",
                    entry.seq
                )
            })?;
        if payload.invoice_id != invoice_id {
            continue;
        }
        let idem =
            IdempotencyKey::from_canonical_string(&payload.idempotency_key).ok_or_else(|| {
                anyhow!(
                    "InvoiceSubmissionResponse idempotency_key '{}' failed parse — \
                     the audit ledger appears tampered or schema-drifted",
                    payload.idempotency_key
                )
            })?;
        return Ok(Some((payload.transaction_id, idem)));
    }
    Ok(None)
}

/// Most-recent (highest-seq) `InvoiceSubmissionAttempt` for this
/// invoice id. Returns the persisted `idempotency_key` only — the
/// state-2 Pending precondition does NOT carry a NAV
/// `transaction_id` (no Response exists yet). `None` if no Attempt
/// entry exists for this invoice (then `stuck_precondition` returns
/// `NotStuck(NeverSubmitted)`).
///
/// PR-19 / ADR-0032 §4 — the state-2 Pending classifier's evidence
/// source.
fn latest_submission_attempt(
    entries: &[Entry],
    invoice_id: &str,
) -> Result<Option<IdempotencyKey>> {
    for entry in entries.iter().rev() {
        if entry.kind != EventKind::InvoiceSubmissionAttempt {
            continue;
        }
        let payload: audit_payloads::InvoiceSubmissionAttemptPayload =
            serde_json::from_slice(&entry.payload).map_err(|e| {
                anyhow!(
                    "InvoiceSubmissionAttempt audit payload (seq {:?}) failed typed decode: {e}",
                    entry.seq
                )
            })?;
        if payload.invoice_id != invoice_id {
            continue;
        }
        let idem =
            IdempotencyKey::from_canonical_string(&payload.idempotency_key).ok_or_else(|| {
                anyhow!(
                    "InvoiceSubmissionAttempt idempotency_key '{}' failed parse — \
                     the audit ledger appears tampered or schema-drifted",
                    payload.idempotency_key
                )
            })?;
        return Ok(Some(idem));
    }
    Ok(None)
}

/// Most-recent (highest-seq) `InvoiceCheckPerformed` outcome for this
/// invoice id. Returns the `outcome` string (`"exists"` / `"absent"` /
/// `"failure"`) or `None` if no such entry exists.
///
/// PR-43 / F49 closure. Read by `mark-abandoned` as a Layer-2-aware
/// guard: when the most-recent outcome is `"exists"`, NAV already
/// has the invoice and abandoning locally would create a silent
/// divergence between ABERP's terminal-abandoned state and NAV's
/// accepted-submission state. The operator's recovery path on Exists
/// is `aberp recover-from-nav` (PR-21 / ADR-0034) followed by
/// `aberp poll-ack`; an explicit `--force-despite-nav-exists` flag
/// on `mark-abandoned` overrides the guard when the operator has
/// out-of-band knowledge that abandonment is correct anyway.
///
/// Per ADR-0033 §6: Layer-2 entries remain informational-only at the
/// `stuck_precondition` classifier (NOT classification-bearing). This
/// helper is a SECOND surface that reads the same evidence for the
/// orthogonal mark-abandoned-guard purpose; the classifier semantic
/// is unchanged.
pub fn latest_check_performed_outcome(
    ledger: &Ledger,
    invoice_id: &str,
) -> Result<Option<String>> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries to resolve latest_check_performed_outcome")?;
    latest_check_performed_outcome_from_entries(&entries, invoice_id)
}

fn latest_check_performed_outcome_from_entries(
    entries: &[Entry],
    invoice_id: &str,
) -> Result<Option<String>> {
    for entry in entries.iter().rev() {
        if entry.kind != EventKind::InvoiceCheckPerformed {
            continue;
        }
        let payload: audit_payloads::InvoiceCheckPerformedPayload =
            serde_json::from_slice(&entry.payload).map_err(|e| {
                anyhow!(
                    "InvoiceCheckPerformed audit payload (seq {:?}) failed typed decode: {e}",
                    entry.seq
                )
            })?;
        if payload.invoice_id == invoice_id {
            return Ok(Some(payload.outcome));
        }
    }
    Ok(None)
}

/// Most-recent (highest-seq) `InvoiceAckStatus` for this invoice id.
/// Returns the parsed `ack_status` string. `None` if no such entry
/// exists for this invoice (the poll loop never ran or only saw
/// errors — both legitimate stuck preconditions).
fn latest_ack_status(entries: &[Entry], invoice_id: &str) -> Result<Option<String>> {
    for entry in entries.iter().rev() {
        if entry.kind != EventKind::InvoiceAckStatus {
            continue;
        }
        let payload: audit_payloads::InvoiceAckStatusPayload =
            serde_json::from_slice(&entry.payload).map_err(|e| {
                anyhow!(
                    "InvoiceAckStatus audit payload (seq {:?}) failed typed decode: {e}",
                    entry.seq
                )
            })?;
        if payload.invoice_id == invoice_id {
            return Ok(Some(payload.ack_status));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    //! Unit tests on the precondition classifier. Build small in-memory
    //! ledgers and verify the four NotStuck reasons + the two Stuck
    //! shapes. The audit-evidence chain itself is exercised by the
    //! existing integration tests; here we pin the classifier's
    //! decision matrix.
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    fn fixture_ledger() -> (Ledger, Actor) {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        (ledger, actor)
    }

    fn write_submission_response(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
        txid: &str,
    ) {
        let payload = audit_payloads::InvoiceSubmissionResponsePayload::new(
            invoice_id,
            idem,
            txid,
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    fn write_ack_status(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        txid: &str,
        status: &str,
    ) {
        let payload = audit_payloads::InvoiceAckStatusPayload::new(
            invoice_id,
            txid,
            status,
            b"<ack/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceAckStatus,
                payload.to_bytes(),
                actor.clone(),
                None,
            )
            .unwrap();
    }

    fn write_marked_abandoned(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
        txid: &str,
    ) {
        let payload = audit_payloads::InvoiceMarkedAbandonedPayload::new(
            invoice_id,
            idem,
            Some(txid.to_string()),
            None,
            "test abandon",
        );
        ledger
            .append(
                EventKind::InvoiceMarkedAbandoned,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    /// PR-19 / ADR-0032 §1 — write an `InvoiceSubmissionAttempt` entry
    /// in isolation (no Response). Used by the state-2 Pending
    /// classifier tests below to simulate the "TX1 committed, TX2 did
    /// not" precondition.
    fn write_submission_attempt(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
    ) {
        let payload = audit_payloads::InvoiceSubmissionAttemptPayload::new(
            invoice_id,
            idem,
            "test",
            b"<request/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    /// PR-19 / ADR-0032 §2 — write an `InvoiceSubmissionAttemptFailed`
    /// entry. Used by the state-2 Pending classifier tests to verify
    /// that the AttemptFailed entry does NOT change classification
    /// (Attempt-with-AttemptFailed-without-Response is still state-2
    /// Pending per ADR-0032 §4).
    fn write_submission_attempt_failed(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
    ) {
        let payload = audit_payloads::InvoiceSubmissionAttemptFailedPayload::new(
            invoice_id,
            idem,
            "test",
            "transport",
            None,
            "test transport failure".to_string(),
            None,
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionAttemptFailed,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    /// PR-20 / ADR-0033 §2 — write an `InvoiceCheckPerformed`
    /// entry for a given outcome (`"exists"` / `"absent"`).
    /// Used by the §6 informational-only pins below to verify
    /// that Layer-2 entries do NOT change `stuck_precondition`
    /// classification.
    fn write_check_performed(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
        outcome: &'static str,
    ) {
        let payload = audit_payloads::InvoiceCheckPerformedPayload::new_for_outcome(
            invoice_id,
            idem,
            "test",
            "INV-default/00042",
            outcome,
            b"<QueryInvoiceCheckRequest/>".to_vec(),
            b"<QueryInvoiceCheckResponse/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceCheckPerformed,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    #[test]
    fn never_submitted_when_no_submission_response() {
        let (ledger, _actor) = fixture_ledger();
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::NeverSubmitted) => {}
            other => panic!("expected NeverSubmitted, got {other:?}"),
        }
    }

    #[test]
    fn stuck_when_submission_response_exists_and_no_ack() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::AwaitingAck);
                assert_eq!(p.prior_transaction_id.as_deref(), Some("TXID-A"));
                assert_eq!(p.prior_last_ack_status, None);
                assert_eq!(p.idempotency_key, idem);
            }
            other => panic!("expected Stuck, got {other:?}"),
        }
    }

    #[test]
    fn stuck_when_last_ack_is_intermediate() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        write_ack_status(&mut ledger, &actor, "inv_A", "TXID-A", "RECEIVED");
        write_ack_status(&mut ledger, &actor, "inv_A", "TXID-A", "PROCESSING");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::AwaitingAck);
                // Latest = PROCESSING (the second ack), not RECEIVED.
                assert_eq!(p.prior_last_ack_status.as_deref(), Some("PROCESSING"));
                assert_eq!(p.prior_transaction_id.as_deref(), Some("TXID-A"));
            }
            other => panic!("expected Stuck, got {other:?}"),
        }
    }

    #[test]
    fn finalized_when_last_ack_is_saved() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        write_ack_status(&mut ledger, &actor, "inv_A", "TXID-A", "PROCESSING");
        write_ack_status(&mut ledger, &actor, "inv_A", "TXID-A", "SAVED");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::AlreadyFinalized) => {}
            other => panic!("expected AlreadyFinalized, got {other:?}"),
        }
    }

    #[test]
    fn rejected_when_last_ack_is_aborted() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        write_ack_status(&mut ledger, &actor, "inv_A", "TXID-A", "ABORTED");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::AlreadyRejected) => {}
            other => panic!("expected AlreadyRejected, got {other:?}"),
        }
    }

    #[test]
    fn abandoned_wins_over_any_other_state() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        // Even if a poll says SAVED, a later abandoned entry is the
        // terminal-by-operator-decision. (In practice this combo
        // should not happen — the orchestration won't write
        // marked_abandoned on a SAVED invoice — but the precondition
        // helper must classify the actual ledger contents, not what
        // the orchestration *should* have done.)
        write_ack_status(&mut ledger, &actor, "inv_A", "TXID-A", "SAVED");
        write_marked_abandoned(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::AlreadyAbandoned) => {}
            other => panic!("expected AlreadyAbandoned, got {other:?}"),
        }
    }

    /// Cross-invoice contamination check. Stuck precondition for
    /// `inv_A` must NOT be influenced by ack/abandon entries for
    /// `inv_B`. Mirrors the same defence-in-depth the
    /// `extract_transaction_id` test in `poll_ack` carries.
    #[test]
    fn precondition_does_not_cross_invoice_ids() {
        let (mut ledger, actor) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        write_submission_response(&mut ledger, &actor, "inv_A", idem_a, "TXID-A");
        write_submission_response(&mut ledger, &actor, "inv_B", idem_b, "TXID-B");
        // B is abandoned; A is not.
        write_marked_abandoned(&mut ledger, &actor, "inv_B", idem_b, "TXID-B");
        // A's classification ignores B's abandon.
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::AwaitingAck);
                assert_eq!(p.prior_transaction_id.as_deref(), Some("TXID-A"));
            }
            other => panic!("expected Stuck for inv_A, got {other:?}"),
        }
        // B is correctly abandoned.
        match stuck_precondition(&ledger, "inv_B").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::AlreadyAbandoned) => {}
            other => panic!("expected AlreadyAbandoned for inv_B, got {other:?}"),
        }
    }

    // ── PR-19 / ADR-0032 §4 — state-2 Pending classifier ────────────

    /// State-2 Pending: an Attempt exists, no Response, no Abandoned.
    /// The classifier returns `Stuck(StuckStage::Pending,
    /// prior_transaction_id=None, ...)` carrying the
    /// idempotency_key from the Attempt payload (F8). CLAUDE.md
    /// rule 9: pins the load-bearing state-2 path against a
    /// regression that collapses state-2 into NeverSubmitted.
    #[test]
    fn pending_when_attempt_exists_and_no_response() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::Pending);
                assert!(p.prior_transaction_id.is_none());
                assert!(p.prior_last_ack_status.is_none());
                assert_eq!(p.idempotency_key, idem);
            }
            other => panic!("expected Stuck(Pending), got {other:?}"),
        }
    }

    /// State-2 Pending survives an AttemptFailed entry. The classifier
    /// does not consult AttemptFailed; multiple failed attempts
    /// accumulate as audit evidence but do not change the stage.
    /// Pins ADR-0032 §4's "AttemptFailed does NOT change
    /// classification" contract against a future refactor that
    /// silently changes the semantics.
    #[test]
    fn pending_when_attempt_followed_by_attempt_failed() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_submission_attempt_failed(&mut ledger, &actor, "inv_A", idem);
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::Pending);
                assert!(p.prior_transaction_id.is_none());
                assert_eq!(p.idempotency_key, idem);
            }
            other => panic!("expected Stuck(Pending), got {other:?}"),
        }
    }

    /// State-2 → State-3 transition: once a Response arrives the
    /// stage flips to AwaitingAck (the precondition walker prefers
    /// Response over Attempt — step 2 in the classifier walks
    /// before step 3). Pins the ordering of the classifier's match
    /// arms against accidental re-ordering.
    #[test]
    fn awaiting_ack_when_attempt_and_response_both_exist() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::AwaitingAck);
                assert_eq!(p.prior_transaction_id.as_deref(), Some("TXID-A"));
            }
            other => panic!("expected Stuck(AwaitingAck), got {other:?}"),
        }
    }

    /// State-2 → AlreadyAbandoned: an Abandoned entry for a state-2
    /// Pending invoice transitions it to AlreadyAbandoned (the
    /// terminal-by-operator-decision branch wins regardless of
    /// stage). Pins ADR-0032 §"Adversarial review" #8's contract
    /// — mark-abandoned works on state-2 too.
    #[test]
    fn already_abandoned_overrides_pending_state_2() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_marked_abandoned(&mut ledger, &actor, "inv_A", idem, "<none>");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::AlreadyAbandoned) => {}
            other => panic!("expected AlreadyAbandoned, got {other:?}"),
        }
    }

    /// Cross-invoice contamination check for the state-2 classifier:
    /// an Attempt for B does NOT push A into state-2. Mirror of
    /// `precondition_does_not_cross_invoice_ids` against the new
    /// `latest_submission_attempt` helper.
    #[test]
    fn pending_classification_does_not_cross_invoice_ids() {
        let (mut ledger, actor) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        // A has an Attempt; B has nothing.
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem_a);
        // B's classification is NeverSubmitted, not Pending.
        match stuck_precondition(&ledger, "inv_B").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::NeverSubmitted) => {}
            other => panic!("expected NeverSubmitted for inv_B, got {other:?}"),
        }
        // A's classification is Pending.
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::Pending);
                assert_eq!(p.idempotency_key, idem_a);
            }
            other => panic!("expected Stuck(Pending) for inv_A, got {other:?}"),
        }
        // Unused — only here so clippy does not complain that idem_b
        // is declared but never read.
        let _ = idem_b;
    }

    // ── PR-20 / ADR-0033 §6 — Layer-2 is informational-only ──────────

    /// PR-20 / ADR-0033 §6: an `InvoiceCheckPerformed(outcome=exists)`
    /// entry does NOT change classification. An invoice with
    /// `Attempt` + `InvoiceCheckPerformed(outcome=exists)` and no
    /// `Response` is still state-2 Pending — the precondition
    /// walker classifies by ABERP-side state changes (Attempt /
    /// Response / Abandoned) only; Layer-2 NAV-side facts are NOT
    /// classification-bearing per the deliberate minimal scope.
    /// CLAUDE.md rule 9: a future refactor that silently makes
    /// Layer-2 entries classification-bearing would surface here,
    /// not at the first F48 state-recovery PR.
    #[test]
    fn check_performed_exists_does_not_change_state_2_classification() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_check_performed(&mut ledger, &actor, "inv_A", idem, "exists");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::Pending);
                assert!(p.prior_transaction_id.is_none());
                assert_eq!(p.idempotency_key, idem);
            }
            other => panic!("expected Stuck(Pending), got {other:?}"),
        }
    }

    /// PR-20 / ADR-0033 §6: same pin for `outcome=absent`. The
    /// retry orchestration follows up with a fresh Attempt /
    /// Response pair after an `Absent` outcome (see
    /// `retry_submission::run`'s Phase 0 → Phase 1+2 transition);
    /// but a hypothetical `Absent` entry WITHOUT the subsequent
    /// retry-pair (e.g., process crashed between TX0 and TX1)
    /// must still classify as state-2 Pending — the prior Attempt
    /// stands; Layer-2 is informational.
    #[test]
    fn check_performed_absent_does_not_change_state_2_classification() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_check_performed(&mut ledger, &actor, "inv_A", idem, "absent");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::Pending);
                assert_eq!(p.idempotency_key, idem);
            }
            other => panic!("expected Stuck(Pending), got {other:?}"),
        }
    }

    /// PR-20 / ADR-0033 §6: an `InvoiceCheckPerformed` entry on a
    /// state-3 invoice (Attempt + Response present + an Exists
    /// check) classifies as AwaitingAck regardless of the Layer-2
    /// outcome. The Response is the deciding factor; Layer-2 is
    /// informational.
    #[test]
    fn check_performed_does_not_change_state_3_classification() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        // Even if a Layer-2 check was performed (hypothetical —
        // PR-20 only fires Layer-2 on state-2; this pins the
        // classifier's robustness to any ordering).
        write_check_performed(&mut ledger, &actor, "inv_A", idem, "exists");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::Stuck(p) => {
                assert_eq!(p.stage, StuckStage::AwaitingAck);
                assert_eq!(p.prior_transaction_id.as_deref(), Some("TXID-A"));
            }
            other => panic!("expected Stuck(AwaitingAck), got {other:?}"),
        }
    }

    // ── PR-43 / F49 — latest_check_performed_outcome ─────────────────

    /// No check entries → `None`. Pins the degenerate-input behaviour
    /// against future regressions.
    #[test]
    fn latest_check_performed_outcome_returns_none_on_empty_ledger() {
        let (ledger, _actor) = fixture_ledger();
        let outcome = latest_check_performed_outcome(&ledger, "inv_A").unwrap();
        assert!(outcome.is_none());
    }

    /// Single `outcome="exists"` entry → `Some("exists")`. The base
    /// case of the F49 guard's evidence read.
    #[test]
    fn latest_check_performed_outcome_returns_exists_when_only_exists_entry() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_check_performed(&mut ledger, &actor, "inv_A", idem, "exists");
        let outcome = latest_check_performed_outcome(&ledger, "inv_A").unwrap();
        assert_eq!(outcome.as_deref(), Some("exists"));
    }

    /// Single `outcome="absent"` entry → `Some("absent")`. The
    /// guard's negative-evidence case (NAV does not have the
    /// invoice; abandonment is safe).
    #[test]
    fn latest_check_performed_outcome_returns_absent_when_only_absent_entry() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_check_performed(&mut ledger, &actor, "inv_A", idem, "absent");
        let outcome = latest_check_performed_outcome(&ledger, "inv_A").unwrap();
        assert_eq!(outcome.as_deref(), Some("absent"));
    }

    /// Multiple entries: latest (highest seq) wins. Pins the walk-
    /// reverse contract — an earlier Exists followed by a later
    /// Absent must surface as Absent. The guard fires on the latest
    /// evidence per ADR-0033 §6.
    #[test]
    fn latest_check_performed_outcome_returns_latest_when_multiple_entries() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_check_performed(&mut ledger, &actor, "inv_A", idem, "exists");
        write_check_performed(&mut ledger, &actor, "inv_A", idem, "absent");
        let outcome = latest_check_performed_outcome(&ledger, "inv_A").unwrap();
        assert_eq!(outcome.as_deref(), Some("absent"));
    }

    /// Cross-invoice isolation: a check entry on inv_B does NOT
    /// surface for inv_A. Defence-in-depth pin per CLAUDE.md
    /// rule 9 — a regression that drops the invoice_id filter
    /// would surface here, not at the first cross-tenant guard
    /// false-positive.
    #[test]
    fn latest_check_performed_outcome_does_not_cross_invoice_ids() {
        let (mut ledger, actor) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        write_check_performed(&mut ledger, &actor, "inv_B", idem_b, "exists");
        let outcome_a = latest_check_performed_outcome(&ledger, "inv_A").unwrap();
        let outcome_b = latest_check_performed_outcome(&ledger, "inv_B").unwrap();
        assert!(outcome_a.is_none());
        assert_eq!(outcome_b.as_deref(), Some("exists"));
        let _ = idem_a;
    }

    /// PR-20 / ADR-0033 §6: `InvoiceMarkedAbandoned` still wins
    /// over any Layer-2 entry. A divergence (NAV has the invoice
    /// per `outcome=exists`; operator chose to abandon locally)
    /// is preserved as audit evidence but classifies as
    /// `AlreadyAbandoned`. CLAUDE.md rule 12: the divergence is
    /// loud in the audit chain, not hidden in classification.
    #[test]
    fn already_abandoned_wins_over_check_performed_exists() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_check_performed(&mut ledger, &actor, "inv_A", idem, "exists");
        write_marked_abandoned(&mut ledger, &actor, "inv_A", idem, "<none>");
        match stuck_precondition(&ledger, "inv_A").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::AlreadyAbandoned) => {}
            other => panic!("expected AlreadyAbandoned, got {other:?}"),
        }
    }
}
