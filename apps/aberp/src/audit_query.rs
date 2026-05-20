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

/// The audit-ledger view of a stuck invoice's precondition state.
/// Returned by [`stuck_precondition`] when the invoice IS stuck;
/// carries every field the PR-8 commands need to write their
/// own audit entries (the prior `transaction_id` and last ack status)
/// alongside the `IdempotencyKey` that links to the original issuance
/// per F8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckPrecondition {
    /// The NAV `transaction_id` from the most-recent
    /// `InvoiceSubmissionResponse` for this invoice. Always populated —
    /// a stuck invoice by definition has a prior submission response.
    pub prior_transaction_id: String,
    /// String form (`"RECEIVED"` / `"PROCESSING"`) of the most-recent
    /// `InvoiceAckStatus` payload's `ack_status` field. `None` if no
    /// ack entry exists yet — the operator can still retry/abandon at
    /// this point (an `InvoiceSubmissionResponse` is sufficient for
    /// the stuck precondition; the poll loop need not have ever run).
    pub prior_last_ack_status: Option<String>,
    /// The original issuance's idempotency key. The PR-8 audit entries
    /// thread it through per F8 ("every NAV-related entry for an
    /// invoice carries the SAME idempotency_key as the issuance
    /// entries").
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
/// classifies the state. The classification rules in one place:
///
///   - If any `InvoiceMarkedAbandoned` exists for this invoice →
///     `NotStuck(AlreadyAbandoned)`. (Terminal-by-operator-decision.)
///   - Else: find the most-recent `InvoiceSubmissionResponse`. If
///     none → `NotStuck(NeverSubmitted)`.
///   - Else: walk every `InvoiceAckStatus` for this invoice; the
///     most-recent one's `ack_status` decides:
///       - `"SAVED"`     → `NotStuck(AlreadyFinalized)`
///       - `"ABORTED"`   → `NotStuck(AlreadyRejected)`
///       - any other     → `Stuck(...)` carrying that string as `prior_last_ack_status`
///       - no ack entry  → `Stuck(...)` with `None`
///
/// The `IdempotencyKey` on the returned `StuckPrecondition` is taken
/// from the most-recent `InvoiceSubmissionResponse`'s payload (its
/// `idempotency_key` field is the same as the original issuance's
/// per F8). This couples the PR-8 audit writes' idempotency_key to
/// the submission_response's, which is the closest in-time anchor
/// for the retry/abandon decision.
pub fn stuck_precondition(ledger: &Ledger, invoice_id: &str) -> Result<StuckOutcome> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries to resolve stuck precondition")?;

    // 1. Terminal-by-operator-decision wins over everything else.
    if has_marked_abandoned(&entries, invoice_id)? {
        return Ok(StuckOutcome::NotStuck(NotStuckReason::AlreadyAbandoned));
    }

    // 2. No prior submission_response → never submitted.
    let Some((prior_transaction_id, idempotency_key)) =
        latest_submission_response(&entries, invoice_id)?
    else {
        return Ok(StuckOutcome::NotStuck(NotStuckReason::NeverSubmitted));
    };

    // 3. Walk acks for terminal status; non-terminal (or none) → stuck.
    let prior_last_ack_status = latest_ack_status(&entries, invoice_id)?;
    match prior_last_ack_status.as_deref() {
        Some("SAVED") => Ok(StuckOutcome::NotStuck(NotStuckReason::AlreadyFinalized)),
        Some("ABORTED") => Ok(StuckOutcome::NotStuck(NotStuckReason::AlreadyRejected)),
        _ => Ok(StuckOutcome::Stuck(StuckPrecondition {
            prior_transaction_id,
            prior_last_ack_status,
            idempotency_key,
        })),
    }
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
            txid,
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
                assert_eq!(p.prior_transaction_id, "TXID-A");
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
                // Latest = PROCESSING (the second ack), not RECEIVED.
                assert_eq!(p.prior_last_ack_status.as_deref(), Some("PROCESSING"));
                assert_eq!(p.prior_transaction_id, "TXID-A");
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
            StuckOutcome::Stuck(p) => assert_eq!(p.prior_transaction_id, "TXID-A"),
            other => panic!("expected Stuck for inv_A, got {other:?}"),
        }
        // B is correctly abandoned.
        match stuck_precondition(&ledger, "inv_B").unwrap() {
            StuckOutcome::NotStuck(NotStuckReason::AlreadyAbandoned) => {}
            other => panic!("expected AlreadyAbandoned for inv_B, got {other:?}"),
        }
    }
}
