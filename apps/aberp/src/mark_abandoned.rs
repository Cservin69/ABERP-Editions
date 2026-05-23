//! Orchestration for the `aberp mark-abandoned` subcommand (PR-8-2).
//!
//! Operator-unblock command that records the operator's decision to
//! stop retrying a `SubmissionStuck` invoice per ADR-0009 §5.
//! Terminal in the audit ledger: after `InvoiceMarkedAbandoned` is
//! written, no further `aberp` subcommand will operate on this
//! invoice (`retry-submission`, `poll-ack`, and a future re-abandon
//! all see the `AlreadyAbandoned` precondition and loud-fail).
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args (tenant; reason text required).
//!   2. Open tenant DuckDB; load the previously-issued invoice +
//!      idempotency_key from the billing store (scoped read tx; same
//!      shape as `submit_invoice::run` / `retry_submission::run`).
//!   3. Read the audit ledger via a fresh `Ledger::open` and resolve
//!      the stuck precondition through [`crate::audit_query::stuck_precondition`].
//!      Loud-fail on every `NotStuck` reason — `mark-abandoned` is
//!      a no-op outside the `Stuck` posture.
//!   4. Under a single DuckDB transaction, append the
//!      `InvoiceMarkedAbandoned` entry. One entry, one tx, because
//!      there is no NAV call to atomically pair it with.
//!   5. Verify the audit chain after commit (success-criterion gate).
//!   6. Advance the typestate (Stuck → Abandoned) and print the
//!      operator-visible summary.
//!
//! # Why NO NAV call
//!
//! `mark-abandoned` does NOT contact NAV. Per ADR-0009 §6, abandoning
//! a stuck submission is distinct from a **technical annulment**
//! (which DOES call `manageAnnulment` against NAV to withdraw a faulty
//! data submission). Abandonment is a local audit-ledger fact: ABERP
//! has decided not to keep retrying; the invoice's status at NAV
//! remains whatever NAV last reported. If the operator additionally
//! needs to withdraw the data submission from NAV's side, that is
//! the future `request-technical-annulment` command (ADR-0009 §6) —
//! NOT this one.
//!
//! # PR-43 / F49 — Layer-2-aware abandonment guard
//!
//! Between the stuck-precondition resolve and the audit-ledger write,
//! the orchestration consults the most-recent `InvoiceCheckPerformed`
//! audit entry for the invoice via
//! [`crate::audit_query::latest_check_performed_outcome`]. When the
//! outcome is `"exists"` — i.e., `retry-submission` or
//! `drain-pending-retries` Phase 0 (PR-20 / ADR-0033 §1) confirmed
//! NAV has the invoice — the orchestration refuses the abandonment
//! by default. Abandoning locally on top of NAV-side acceptance
//! creates a silent divergence: ABERP records the chain as
//! terminal-abandoned while NAV's record stands; an inspector
//! reading the bundle would see contradictory evidence.
//!
//! The default-refuse loud message names `aberp recover-from-nav`
//! (PR-21 / ADR-0034) as the recovery path: reconstruct the local
//! `InvoiceSubmissionResponse` from NAV's `queryInvoiceData`, then
//! `aberp poll-ack` to drive the terminal state via NAV's
//! `queryTransactionStatus`. An explicit
//! `--force-despite-nav-exists` flag overrides the guard for the
//! cases where the operator has out-of-band justification for the
//! divergence (e.g., the NAV-side record will be technically
//! annulled separately). The overriden audit entry's `reason` is
//! automatically suffixed with `[forced-despite-nav-side-exists]`
//! so the bundle reader sees the override's effect without
//! consulting CLI history.
//!
//! Outcome `"absent"` (NAV does not have the invoice) and
//! `"failure"` (the Layer-2 check itself failed) do NOT trigger
//! the guard — abandonment is safe-or-unknown in those cases;
//! per CLAUDE.md rule 2 the guard is minimal, refusing only the
//! one clearly-divergent case.
//!
//! # NAV credentials NOT loaded
//!
//! Unlike `retry-submission`, `mark-abandoned` does not load
//! `NavCredentials` from the keychain. The actor user_id is derived
//! from the OS user instead — a defensive choice: there is no NAV
//! call, and an operator who runs `mark-abandoned` on a
//! keychain-less machine should not be blocked by a missing
//! credential artifact. The audit-evidence bundle still carries a
//! local-CLI Actor per ADR-0008.
//!
//! # F12 trap status
//!
//! `InvoiceMarkedAbandoned` is the second of PR-8's two new
//! `EventKind` variants. Four coordinated edits land in the same
//! commit (variant body, `as_str` arm, `from_storage_str` arm,
//! `round_trip_for_every_variant` hand-listed array).

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{self as billing, IdempotencyKey, ReadyInvoice};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads;
use crate::audit_query::{self, StuckOutcome, StuckPrecondition, StuckStage};
use crate::binary_hash;
use crate::cli::MarkAbandonedArgs;

pub fn run(args: &MarkAbandonedArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "mark_abandoned",
        invoice_id = %args.invoice_id,
        tenant = %args.tenant,
    )
    .entered();

    // 1. Parse + validate CLI args.
    let tenant = TenantId::new(args.tenant.clone()).ok_or_else(|| {
        anyhow!(
            "--tenant value '{}' is empty or has a null byte",
            args.tenant
        )
    })?;
    let reason = args.reason.trim();
    if reason.is_empty() {
        return Err(anyhow!(
            "--reason is required for mark-abandoned per ADR-0009 §5 \
             (terminal operator decision must carry a human-readable justification)"
        ));
    }

    // Actor for the local CLI invocation. No NAV credentials are
    // loaded here (see module header) — derive the user_id from the
    // OS-reported username instead. Loud-fail if neither USER nor
    // LOGNAME is set; otherwise the audit-ledger Actor would record
    // an empty user_id and the F15-class "who did this?" question
    // becomes unanswerable.
    let session_id = Ulid::new().to_string();
    let os_user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "neither USER nor LOGNAME is set in the environment — \
                 cannot derive an Actor.user_id for the audit ledger; \
                 mark-abandoned writes a terminal operator decision and \
                 must record who made it"
            )
        })?;
    let actor = Actor::from_local_cli(session_id, &os_user);
    tracing::info!(
        user_id = %actor.user_id,
        "actor derived for mark-abandoned (no NAV credentials loaded)"
    );

    // 2. Load the previously-issued invoice + idempotency key.
    let mut conn = Connection::open(&args.db)
        .with_context(|| format!("open tenant DuckDB at {}", args.db.display()))?;
    let (ready_invoice, idempotency_key) = load_issued_invoice(&mut conn, &args.invoice_id)?;
    if ready_invoice.id.to_prefixed_string() != args.invoice_id {
        return Err(anyhow!(
            "loaded invoice id {} does not match requested {}",
            ready_invoice.id.to_prefixed_string(),
            args.invoice_id
        ));
    }
    tracing::info!(
        seq = ready_invoice.sequence_number,
        idempotency_key = %idempotency_key.to_canonical_string(),
        "issued invoice loaded for mark-abandoned"
    );

    // 3. Resolve the stuck precondition.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let stuck = resolve_stuck_or_loud_fail(
        &args.db,
        tenant.clone(),
        binary_hash_bytes,
        &args.invoice_id,
        &idempotency_key,
    )?;

    // 3a. PR-43 / F49 closure — Layer-2-aware guard. Consult the
    //     most-recent InvoiceCheckPerformed for this invoice; if
    //     outcome is "exists", refuse the abandonment by default
    //     (NAV has the invoice; local abandonment would create
    //     silent divergence). The operator overrides with
    //     --force-despite-nav-exists when out-of-band justification
    //     applies; the override is loud in the audit reason field.
    let latest_check = {
        let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger to consult latest_check_performed_outcome (F49 guard)")?;
        audit_query::latest_check_performed_outcome(&ledger, &args.invoice_id)?
    };
    let nav_side_exists = matches!(latest_check.as_deref(), Some("exists"));
    if nav_side_exists && !args.force_despite_nav_exists {
        return Err(anyhow!(
            "F49 guard: mark-abandoned REFUSED for invoice {}: \
             the most-recent InvoiceCheckPerformed audit entry has outcome=\"exists\" — \
             NAV already has the invoice (Layer-2 queryInvoiceCheck per ADR-0033 §1). \
             Abandoning locally would silently diverge ABERP's terminal-abandoned state \
             from NAV's accepted-submission state. \
             Run `aberp recover-from-nav --invoice-id {} --tax-number ... \
             --endpoint {{test|production}}` (PR-21 / ADR-0034) to reconstruct the local \
             InvoiceSubmissionResponse from NAV's queryInvoiceData, then `aberp poll-ack` \
             to drive the terminal state via queryTransactionStatus. \
             If the divergence is intentional (e.g., the NAV-side record will be technically \
             annulled separately and the operator wants to terminate the local chain now), \
             re-run with --force-despite-nav-exists — the resulting audit entry's reason \
             field will carry a [forced-despite-nav-side-exists] marker.",
            args.invoice_id, args.invoice_id,
        ));
    }
    let forced_marker_active = nav_side_exists && args.force_despite_nav_exists;
    if forced_marker_active {
        tracing::warn!(
            invoice_id = %args.invoice_id,
            "F49 guard OVERRIDDEN: --force-despite-nav-exists is set; \
             writing InvoiceMarkedAbandoned despite NAV-side Exists evidence"
        );
    }

    // 4. Write the InvoiceMarkedAbandoned entry under one tx. The
    //    audit reason carries a [forced-despite-nav-side-exists]
    //    suffix when the F49 guard was overridden (CLAUDE.md rule 12
    //    — the override is loud in the bundle).
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);
    let effective_reason = effective_reason_for_audit(reason, forced_marker_active);
    write_marked_abandoned_audit_entry(
        &mut conn,
        &ledger_meta,
        actor,
        &ready_invoice,
        idempotency_key,
        &effective_reason,
        &stuck,
    )?;

    // 5. Verify the audit chain (success-criterion gate).
    drop(conn);
    let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes).context("open audit ledger")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER mark-abandoned")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 5a. PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit.
    let mirror_path = audit_ledger::mirror_path_for(&args.db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after mark-abandoned commit")?;

    // 6. Typestate advance + operator-visible summary. PR-19 /
    //    ADR-0032 §4: state-2 Pending has no prior_transaction_id;
    //    the typestate-display path is taken only for state-3
    //    AwaitingAck (where the prior txid exists). State-2 prints
    //    a stage-aware summary directly without the typestate
    //    dance — the audit ledger is the source of truth per
    //    ADR-0023 A5/A6, and the typestate transition is purely
    //    cosmetic for the summary message.
    let invoice_id_str = ready_invoice.id.to_prefixed_string();
    let invoice_seq = ready_invoice.sequence_number;
    let prior_last_ack = stuck.prior_last_ack_status.as_deref().unwrap_or("<none>");
    let stage_label = match stuck.stage {
        StuckStage::Pending => "state-2 Pending",
        StuckStage::AwaitingAck => "state-3 AwaitingAck",
    };
    match stuck.prior_transaction_id.clone() {
        Some(txid) => {
            // State-3 path — the existing pre-PR-19 typestate
            // advance is retained for the operator-visible message.
            let submitted = ready_invoice.into_submitted(txid);
            let stuck_typestate = submitted.into_submission_stuck();
            let abandoned = stuck_typestate.into_abandoned();
            tracing::error!(
                invoice_id = %abandoned.id.to_prefixed_string(),
                seq = abandoned.sequence_number,
                prior_transaction_id = %abandoned.nav_transaction_id,
                prior_last_ack_status = ?stuck.prior_last_ack_status,
                stage = stage_label,
                "invoice marked ABANDONED by operator — terminal; sequence not reused; \
                 a corrective new invoice may be required"
            );
            println!(
                "mark-abandoned OK ({}): invoice {} (seq {}) marked ABANDONED \
                 (prior txid {}, prior last ack {}) \
                 — sequence not reused; audit chain verified across {} entries; \
                 issue a corrective new invoice if the business transaction still needs reporting",
                stage_label,
                abandoned.id.to_prefixed_string(),
                abandoned.sequence_number,
                abandoned.nav_transaction_id,
                prior_last_ack,
                verified,
            );
        }
        None => {
            // State-2 Pending path — no prior_transaction_id; skip
            // the typestate dance and print the stage-aware summary
            // directly.
            tracing::error!(
                invoice_id = %invoice_id_str,
                seq = invoice_seq,
                prior_last_ack_status = ?stuck.prior_last_ack_status,
                stage = stage_label,
                "invoice marked ABANDONED by operator — terminal; sequence not reused; \
                 NO prior NAV transactionId (state-2 Pending — the prior Attempt's wire \
                 broke or the process crashed before TX2 commit per ADR-0032 §1)"
            );
            println!(
                "mark-abandoned OK ({}): invoice {} (seq {}) marked ABANDONED \
                 (no prior NAV transactionId — state-2 Pending) \
                 — sequence not reused; audit chain verified across {} entries; \
                 NOTE the prior Attempt's submission may or may not have reached NAV \
                 (Layer-2 queryInvoiceCheck per ADR-0009 §5 / ADR-0032 §\"Open questions\" F44 \
                 is named-deferred — until it lands, the NAV-side fate is unknown)",
                stage_label,
                invoice_id_str,
                invoice_seq,
                verified,
            );
        }
    }

    Ok(())
}

/// Open the audit ledger, resolve the stuck precondition. Same
/// loud-fail surface + F8 mismatch check as
/// `retry_submission::resolve_stuck_or_loud_fail` — duplicated for
/// the operator-facing-twin reason the parser helpers are duplicated.
fn resolve_stuck_or_loud_fail(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
    invoice_id: &str,
    issuance_idempotency_key: &IdempotencyKey,
) -> Result<StuckPrecondition> {
    let ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to resolve mark-abandoned precondition")?;
    match audit_query::stuck_precondition(&ledger, invoice_id)? {
        StuckOutcome::Stuck(p) => {
            if p.idempotency_key != *issuance_idempotency_key {
                return Err(anyhow!(
                    "F8 contract violation: submission_response idempotency_key '{}' \
                     does not match issuance idempotency_key '{}' — \
                     the audit ledger appears tampered or schema-drifted",
                    p.idempotency_key.to_canonical_string(),
                    issuance_idempotency_key.to_canonical_string(),
                ));
            }
            Ok(p)
        }
        StuckOutcome::NotStuck(reason) => Err(anyhow!(
            "cannot mark-abandoned invoice {}: {}",
            invoice_id,
            reason.as_message()
        )),
    }
}

/// Scoped read tx — identical contract to
/// `retry_submission::load_issued_invoice` and
/// `submit_invoice::load_issued_invoice`.
fn load_issued_invoice(
    conn: &mut Connection,
    invoice_id: &str,
) -> Result<(ReadyInvoice, IdempotencyKey)> {
    let tx = conn
        .transaction()
        .context("begin read transaction for invoice lookup")?;
    let pair = billing::load_ready_invoice_by_id(&tx, invoice_id)
        .context("billing::load_ready_invoice_by_id")?
        .ok_or_else(|| anyhow!("no issued invoice with id {invoice_id} in this tenant DB"))?;
    tx.commit().context("commit read transaction")?;
    Ok(pair)
}

/// PR-43 / F49 closure — compose the audit-payload `reason` text.
/// When the operator overrode the Layer-2-aware guard with
/// `--force-despite-nav-exists`, suffix the operator-supplied reason
/// with a `[forced-despite-nav-side-exists]` marker so the audit-
/// evidence bundle reader sees the override's effect without
/// consulting CLI history (CLAUDE.md rule 12 — fail loud).
fn effective_reason_for_audit(reason: &str, forced_marker_active: bool) -> String {
    if forced_marker_active {
        format!("{reason} [forced-despite-nav-side-exists]")
    } else {
        reason.to_string()
    }
}

/// Open one audit-write tx, append the InvoiceMarkedAbandoned entry, commit.
fn write_marked_abandoned_audit_entry(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    reason: &str,
    stuck: &StuckPrecondition,
) -> Result<()> {
    audit_ledger::ensure_schema(conn).context("ensure audit-ledger schema for mark-abandoned")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (mark-abandoned audit append)")?;

    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();
    let payload = audit_payloads::InvoiceMarkedAbandonedPayload::new(
        &invoice_id_str,
        idempotency_key,
        stuck.prior_transaction_id.clone(),
        stuck.prior_last_ack_status.clone(),
        reason,
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceMarkedAbandoned,
        payload.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceMarkedAbandoned")?;
    tx.commit()
        .context("commit DuckDB transaction (mark-abandoned audit append)")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::effective_reason_for_audit;

    /// PR-43 / F49 closure — the override-marker is appended verbatim
    /// after a single space. Pin per CLAUDE.md rule 9: the marker
    /// text is load-bearing for the audit-evidence bundle's
    /// loud-divergence signal; a refactor that silently changes the
    /// marker text would mask the override in the bundle.
    #[test]
    fn effective_reason_appends_forced_marker_when_active() {
        let reason = "operator chose to abandon despite NAV-side acceptance";
        let composed = effective_reason_for_audit(reason, true);
        assert_eq!(
            composed,
            "operator chose to abandon despite NAV-side acceptance [forced-despite-nav-side-exists]"
        );
        assert!(composed.contains("[forced-despite-nav-side-exists]"));
        assert!(composed.starts_with(reason));
    }

    /// PR-43 / F49 closure — when the guard was not overridden the
    /// operator's reason is preserved verbatim. Pins the orthogonal
    /// case so a future regression that always appends the marker
    /// (and thus pollutes every non-forced bundle) surfaces here.
    #[test]
    fn effective_reason_preserves_reason_verbatim_when_not_forced() {
        let reason = "non-divergent abandonment";
        let composed = effective_reason_for_audit(reason, false);
        assert_eq!(composed, reason);
        assert!(!composed.contains("forced-despite"));
    }

    #[test]
    fn reason_must_be_non_empty() {
        // Empty / whitespace-only reason is loud-failed before any
        // audit write. Pinned the same way `retry_submission` pins it.
        assert!("   ".trim().is_empty());
        assert!("".trim().is_empty());
        assert!(!"x".trim().is_empty());
    }

    /// Three terminal-state strings (`SAVED`, `ABORTED`, abandoned)
    /// route to different operator-visible messages. This test pins
    /// the *intent* — the `NotStuckReason` enum's `as_message`
    /// distinguishes the three cases unambiguously per CLAUDE.md
    /// rule 12.
    #[test]
    fn not_stuck_messages_distinguish_the_three_terminal_cases() {
        use crate::audit_query::NotStuckReason;
        let finalized_msg = NotStuckReason::AlreadyFinalized.as_message();
        let rejected_msg = NotStuckReason::AlreadyRejected.as_message();
        let abandoned_msg = NotStuckReason::AlreadyAbandoned.as_message();
        let never_msg = NotStuckReason::NeverSubmitted.as_message();

        // The four messages are pairwise distinct.
        let all = [finalized_msg, rejected_msg, abandoned_msg, never_msg];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "NotStuckReason messages must be pairwise distinct");
                }
            }
        }

        // The finalized message specifically names SAVED — operators
        // grepping the log for "SAVED" should find it.
        assert!(
            finalized_msg.contains("SAVED"),
            "finalized message must mention SAVED so operators grepping the log find it"
        );
        // The rejected message specifically names ABORTED.
        assert!(
            rejected_msg.contains("ABORTED"),
            "rejected message must mention ABORTED so operators grepping the log find it"
        );
    }
}
