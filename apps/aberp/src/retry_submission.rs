//! Orchestration for the `aberp retry-submission` subcommand (PR-8-1,
//! amended by PR-19 / ADR-0032 §4 to accept state-2 Pending in
//! addition to state-3 AwaitingAck, and §1 to use the two-tx
//! Attempt-before-call posture; further amended by PR-20 /
//! ADR-0033 §1 to add a Phase 0 Layer-2 `queryInvoiceCheck`
//! disambiguation step for state-2 retries that closes the
//! duplicate-submission residual PR-19's adversarial review #2
//! named-warned).
//!
//! Operator-unblock command for an invoice in the `SubmissionStuck`
//! posture per ADR-0009 §5: re-submits the invoice via the existing
//! `tokenExchange` + `manageInvoice` pipeline and writes one extra
//! `InvoiceRetryRequested` audit entry that captures the operator's
//! decision distinct from the per-attempt NAV evidence.
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args (8-digit tax number; tenant; endpoint;
//!      reason text required).
//!   2. Load `NavCredentials` from the OS keychain (loud-fail on missing).
//!   3. Open tenant DuckDB; load the previously-issued invoice +
//!      idempotency_key from the billing store (scoped read tx; same
//!      shape as `submit_invoice::run`).
//!   4. Read the audit ledger via a fresh `Ledger::open` and resolve
//!      the stuck precondition through [`crate::audit_query::stuck_precondition`].
//!      Loud-fail on every `NotStuck` reason — `retry-submission` is a
//!      no-op outside the `Stuck` posture and must not silently
//!      submit again on a `SAVED` / `ABORTED` / `Abandoned` invoice.
//!   5. Re-read the NAV InvoiceData XML bytes from disk (same source
//!      `submit_invoice` reads — operator points the command at the
//!      same `--invoice-xml` file).
//!   6. Build a tokio current-thread runtime and drive the NAV
//!      prepare phase (`tokenExchange` + `manage_invoice::build_request`
//!      for `operation = CREATE`, same shape as
//!      `submit_invoice::prepare_for_attempt_audit`). NO wire send
//!      yet — that happens in step 8 after TX1 commit.
//!   7. **TX1 — RetryRequested + Attempt-before-call** (PR-19 /
//!      ADR-0032 §1). Under one DuckDB transaction, append TWO audit
//!      entries: `InvoiceRetryRequested` (operator decision +
//!      precondition; `prior_transaction_id` is `Option<String>` per
//!      ADR-0032 §4 — `None` for state-2 Pending, `Some` for
//!      state-3 AwaitingAck) and `InvoiceSubmissionAttempt`
//!      (verbatim request body from the prepare phase). Commit. Sync
//!      mirror per ADR-0030 §2.
//!   8. **Wire send** — POST the pre-rendered envelope via
//!      `manage_invoice::send_built_request`.
//!   9. **TX2 — Response on success, AttemptFailed on failure**
//!      (PR-19 / ADR-0032 §1). Under a second DuckDB transaction,
//!      append `InvoiceSubmissionResponse` (verbatim response + new
//!      transactionId) on success OR `InvoiceSubmissionAttemptFailed`
//!      (typed error_class + code + message per
//!      `submission_queue::classify_attempt_failure`) on failure.
//!      Commit. Sync mirror.
//!  10. Verify the audit chain after TX2 commit (success-criterion
//!      gate). Advance the typestate (Stuck → Submitted with the
//!      new txid, on success) and print the operator-visible
//!      summary.
//!
//! # Why TX1 widens to two entries (RetryRequested + Attempt)
//!
//! Per `submit_invoice`'s two-tx posture rationale: the Attempt entry
//! must commit BEFORE the wire send so a transport-mid-flight loss
//! leaves the Attempt row in the ledger. The operator's
//! `retry_requested` decision and the resulting Attempt are
//! atomically paired in TX1 — a process crash between the two would
//! produce a half-written retry-decision-with-no-evidence (or
//! vice versa) that the operator cannot reason about. The single
//! TX1 commit guarantees the pair lands together. TX2 commits the
//! Response or AttemptFailed entry independently of TX1.
//!
//! # State-2 Pending acceptance (PR-19 / ADR-0032 §4)
//!
//! Pre-PR-19 `retry-submission` accepted only state-3 (a Response
//! exists, no terminal ack — the AwaitingAck stage). PR-19 extends
//! the precondition walker to also accept state-2 (an Attempt
//! exists, no Response — the Pending stage), which the new two-tx
//! posture introduces. State-2 retries write the same TX1 +
//! TX2 shape as state-3 retries; the only difference is the
//! `prior_transaction_id` field on the `InvoiceRetryRequestedPayload`
//! (None for state-2 because no prior Response exists).
//!
//! # State-2 Layer-2 disambiguation (PR-20 / ADR-0033 §1)
//!
//! PR-20 adds a Phase 0 step BEFORE TX1 for state-2 retries: a
//! `queryInvoiceCheck` call against the NAV-facing invoice number,
//! recorded in a new `InvoiceCheckPerformed` audit entry. Three
//! outcomes per ADR-0033 §1:
//!
//!   - **Exists** — NAV already has the invoice. Skip TX1 + TX2;
//!     no re-POST happens; no duplicate-submission risk. The
//!     operator-visible summary points the operator at
//!     `aberp recover-from-nav` (PR-21 / ADR-0034) to reconstruct
//!     the local `InvoiceSubmissionResponse` from NAV's
//!     `queryInvoiceData` and then `aberp poll-ack` to drive the
//!     terminal state. The operator may alternatively run
//!     `aberp mark-abandoned` locally to terminate the chain by
//!     operator decision.
//!   - **Absent** — NAV does NOT have the invoice. Proceed to
//!     TX1 + TX2 per the pre-PR-20 / PR-19 shape. Genuine
//!     transport-mid-flight loss; the re-POST is safe.
//!   - **Failure** — queryInvoiceCheck failed at any layer. Abort
//!     the retry per ADR-0033 §"Surfaced conflict 1 Reading A"; do
//!     NOT proceed to TX1 + TX2. The `InvoiceCheckPerformed` audit
//!     entry carries the typed `failure_class` / `failure_code` /
//!     `failure_message`; the operator re-runs `retry-submission`
//!     later. The invoice remains in state-2 Pending.
//!
//! State-3 retries (AwaitingAck) skip Phase 0 entirely — they have
//! a prior NAV `transaction_id`, and NAV's Layer-1
//! `INVOICE_NUMBER_NOT_UNIQUE` guard already covers the state-3
//! duplicate residual per ADR-0033 §1. The state-3 path is the
//! verbatim PR-19 shape.
//!
//! # Why not call into `submit_invoice::run` directly
//!
//! `submit_invoice::run` is shaped for the initial-submission case
//! and writes Attempt + Response (or AttemptFailed) per ADR-0032 §1.
//! Adding the `InvoiceRetryRequested` entry on top would require
//! either parametrising `submit_invoice`'s TX1 body (would invade
//! its scope) or wrapping the call (would split the
//! RetryRequested + Attempt pairing across two txs, defeating the
//! atomicity rationale above). The structural NAV-call shape is
//! mirrored here inline per the operator-facing-twin posture
//! (CLAUDE.md rule 2 — neither extracted nor speculatively shared
//! until a third caller appears with the same shape).
//!
//! # F12 trap status
//!
//! PR-8 added two new `EventKind` variants. `InvoiceRetryRequested`
//! is one of them. The four coordinated edits (variant body, `as_str`
//! arm, `from_storage_str` arm, `round_trip_for_every_variant`
//! hand-listed array) land in the same commit as this file. If a
//! future contributor adds an EventKind without those four edits, the
//! round-trip test fails — but this header is the loud reminder.
//! PR-19 / ADR-0032 §2 adds `InvoiceSubmissionAttemptFailed` —
//! tenth landing of the four-edit ritual; the TX2 failure-path
//! write target in this module.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT extend Layer-2 to state-3 retries. PR-20 /
//!     ADR-0033 §1 names state-3 explicitly out of scope; NAV's
//!     Layer-1 `INVOICE_NUMBER_NOT_UNIQUE` guard is the existing
//!     state-3 disambiguation surface. A future F (named-
//!     deferred) may add belt-and-braces Layer-2 to state-3 if
//!     operational evidence surfaces.
//!   - It does NOT reconstruct the local Response/Ack chain after
//!     a positive Layer-2 check. The post-positive-check NAV-side
//!     state recovery (fetching the chain via `queryInvoiceData`
//!     per ADR-0009 §5's full intent) lives on PR-21 / ADR-0034's
//!     `aberp recover-from-nav` operator command (which closes
//!     F48). retry-submission's state-2 + Exists operator-visible
//!     summary points at recover-from-nav rather than completing
//!     the recovery inline (per ADR-0034 §"Surfaced conflict 1
//!     Reading B" — operator-driven explicit invocation; no
//!     automatic chain reconstruction inside retry-submission).
//!   - It does NOT poll `queryTransactionStatus` — the operator runs
//!     `aberp poll-ack` after the retry, same as the original
//!     submission flow. (For an `Exists` outcome the operator's
//!     next move is `aberp recover-from-nav` to reconstruct the
//!     Response from NAV's `queryInvoiceData`, then `aberp
//!     poll-ack` against the recovered transactionId; or
//!     `mark-abandoned` locally to terminate the chain by
//!     operator decision.)
//!   - It does NOT mutate any billing row — the `submission_state`
//!     fact lives in the audit ledger per A5/A6.

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{
    self as billing, BillingStore, DuckDbBillingStore, IdempotencyKey, InvoiceSeries, ReadyInvoice,
};
use aberp_nav_transport::{
    operations::{
        manage_invoice, query_invoice_check::{self, QueryInvoiceCheckOutcome},
        token_exchange,
    },
    soap::{InvoiceDirection, InvoiceOperation, ManageInvoiceItem},
    NavCredentials, NavEndpoint, NavTransport, NavTransportError,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads;
use crate::audit_query::{self, StuckOutcome, StuckPrecondition, StuckStage};
use crate::binary_hash;
use crate::cli::{NavEnv, RetrySubmissionArgs};
use crate::submission_queue;

pub fn run(args: &RetrySubmissionArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "retry_submission",
        invoice_id = %args.invoice_id,
        tenant = %args.tenant,
        endpoint = ?args.endpoint,
    )
    .entered();

    // 1. Parse + validate CLI args.
    let tenant = TenantId::new(args.tenant.clone()).ok_or_else(|| {
        anyhow!(
            "--tenant value '{}' is empty or has a null byte",
            args.tenant
        )
    })?;
    let tax_number_8 = parse_tax_number_8(&args.tax_number)?;
    let nav_endpoint = match args.endpoint {
        NavEnv::Test => NavEndpoint::Test,
        NavEnv::Production => NavEndpoint::Production,
    };
    let endpoint_audit_label = match args.endpoint {
        NavEnv::Test => "test",
        NavEnv::Production => "production",
    };
    let reason = args.reason.trim();
    if reason.is_empty() {
        return Err(anyhow!(
            "--reason is required for retry-submission per ADR-0009 §5 \
             (operator decision must carry a human-readable justification)"
        ));
    }

    // 2. Load NAV credentials BEFORE touching the DB.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for this CLI invocation"
    );

    // 3. Load the previously-issued invoice + its idempotency key.
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
        "issued invoice loaded for retry-submission"
    );

    // 4. Resolve the stuck precondition via the typed audit-query
    //    helper. Drop the tx-Connection's life is unaffected (read tx
    //    committed in load_issued_invoice); we open a fresh Ledger
    //    handle which uses its own duckdb::Connection.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let stuck = resolve_stuck_or_loud_fail(
        &args.db,
        tenant.clone(),
        binary_hash_bytes,
        &args.invoice_id,
        &idempotency_key,
    )?;

    // 5. Read NAV InvoiceData XML bytes (same source as submit_invoice).
    let invoice_xml = std::fs::read(&args.invoice_xml).with_context(|| {
        format!(
            "read NAV InvoiceData XML from {}",
            args.invoice_xml.display()
        )
    })?;
    if invoice_xml.is_empty() {
        return Err(anyhow!(
            "invoice XML at {} is empty",
            args.invoice_xml.display()
        ));
    }
    tracing::info!(
        bytes = invoice_xml.len(),
        "InvoiceData XML loaded for retry"
    );

    // 5a. PR-9-0 / ADR-0022: validate before the retry NAV call. Same
    //     posture as submit_invoice: a hand-edited or schema-drifted
    //     on-disk XML loud-fails BEFORE any tokenExchange happens. No
    //     audit entries land on a validation failure.
    aberp_nav_xsd_validator::validate_invoice_data(&invoice_xml).with_context(|| {
        format!(
            "NAV InvoiceData v3.0 invariant check (ADR-0022) failed for retry on {}",
            args.invoice_xml.display()
        )
    })?;
    tracing::info!(
        nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
        "on-disk InvoiceData XML passed v3.0 invariant check before NAV retry"
    );

    // 5b. Hoisted from step 6 per PR-20 / ADR-0033 §1: the tokio
    //     runtime + ledger_meta are needed by Phase 0 (state-2
    //     Layer-2 disambiguation) AND by Phase 1-2 (the existing
    //     PR-19 prepare → TX1 → wire → TX2 shape).
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for retry NAV calls")?;

    // 5c. PR-20 / ADR-0033 §1: Phase 0 — Layer-2 disambiguation for
    //     state-2 Pending retries. State-3 (AwaitingAck) skips this
    //     step entirely (the existing PR-19 path applies; NAV's
    //     Layer-1 INVOICE_NUMBER_NOT_UNIQUE guard is the existing
    //     state-3 dedup per ADR-0033 §1). The Phase 0 step
    //     writes one `InvoiceCheckPerformed` audit entry in its
    //     own TX (TX0); the outcome drives whether the retry
    //     proceeds (Absent), skips re-POST (Exists), or aborts
    //     (Failure per ADR-0033 §"Surfaced conflict 1 Reading A").
    if stuck.stage == StuckStage::Pending {
        let nav_invoice_number = derive_nav_invoice_number(&args.db, &ready_invoice)?;
        tracing::info!(
            nav_invoice_number = %nav_invoice_number,
            "state-2 retry: performing Layer-2 queryInvoiceCheck per ADR-0033 §1"
        );
        let decision = perform_layer_2_check(
            &runtime,
            nav_endpoint,
            &credentials,
            &tax_number_8,
            &nav_invoice_number,
            &mut conn,
            &ledger_meta,
            actor.clone(),
            &ready_invoice,
            idempotency_key,
            endpoint_audit_label,
            &args.db,
            tenant.clone(),
            binary_hash_bytes,
        )?;
        match decision {
            Layer2Decision::SkipRePost => {
                // NAV already has the invoice. Skip the re-POST per
                // ADR-0033 §1 — no duplicate-submission risk. The
                // local Response/Ack chain remains absent (F48-
                // deferred chain reconstruction); operator-visible
                // summary names the gap loud per CLAUDE.md rule 12.
                drop(conn);
                let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes).context(
                    "open audit ledger after TX0 Exists for chain verification",
                )?;
                let verified = ledger.verify_chain().context(
                    "audit-ledger chain verification failed AFTER Phase 0 Exists",
                )?;
                tracing::info!(
                    entries_verified = verified,
                    nav_invoice_number = %nav_invoice_number,
                    "Layer-2 queryInvoiceCheck returned Exists — re-POST skipped"
                );
                println!(
                    "retry-submission Layer-2 Exists: NAV already has invoice {} ({}) \
                     — re-POST skipped (no duplicate submission) \
                     (audit chain verified across {} entries; \
                     InvoiceCheckPerformed recorded with outcome=exists); \
                     invoice remains state-2 Pending locally because the \
                     prior submission's Response/Ack chain is absent — \
                     run `aberp recover-from-nav --invoice-id {} --tax-number ... \
                     --endpoint {{test|production}}` to reconstruct the local \
                     InvoiceSubmissionResponse from NAV's queryInvoiceData \
                     (PR-21 / ADR-0034), then `aberp poll-ack` to drive the \
                     terminal state via queryTransactionStatus; or run \
                     `aberp mark-abandoned` locally to terminate the chain by \
                     operator decision",
                    ready_invoice.id.to_prefixed_string(),
                    nav_invoice_number,
                    verified,
                    ready_invoice.id.to_prefixed_string(),
                );
                return Ok(());
            }
            Layer2Decision::Abort(failure_message) => {
                // Layer-2 itself failed. Per ADR-0033 §"Surfaced
                // conflict 1 Reading A" the retry aborts; the
                // operator re-runs `retry-submission` later. The
                // invoice remains in state-2 Pending; the
                // InvoiceCheckPerformed audit entry with
                // outcome=failure was written by
                // perform_layer_2_check before this branch fired.
                drop(conn);
                let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes).context(
                    "open audit ledger after TX0 Failure for chain verification",
                )?;
                let verified = ledger.verify_chain().context(
                    "audit-ledger chain verification failed AFTER Phase 0 Failure",
                )?;
                tracing::error!(
                    invoice_id = %ready_invoice.id.to_prefixed_string(),
                    entries_verified = verified,
                    "retry-submission: Layer-2 queryInvoiceCheck failed; \
                     TX0 InvoiceCheckPerformed(outcome=failure) audit written; \
                     retry ABORTED per ADR-0033 §1"
                );
                eprintln!(
                    "retry-submission Layer-2 FAILED for invoice {}: {} \
                     (audit chain verified across {} entries; \
                     InvoiceCheckPerformed recorded with outcome=failure); \
                     invoice remains in state-2 Pending — re-run \
                     `aberp retry-submission` after NAV is reachable",
                    ready_invoice.id.to_prefixed_string(),
                    failure_message,
                    verified,
                );
                return Err(anyhow!(
                    "Layer-2 queryInvoiceCheck failed: {}",
                    failure_message
                ));
            }
            Layer2Decision::ProceedToRePost => {
                // NAV does not have the invoice. The state-2 retry
                // is genuinely "wire broke before NAV saw it";
                // proceed to the existing PR-19 prepare → TX1 →
                // wire → TX2 path below.
                tracing::info!(
                    nav_invoice_number = %nav_invoice_number,
                    "Layer-2 queryInvoiceCheck returned Absent — proceeding to manageInvoice re-POST"
                );
            }
        }
    }

    // 6. NAV prepare phase: tokenExchange + build envelope. NO wire
    //    send yet (PR-19 / ADR-0032 §1).
    let prepared = runtime.block_on(prepare_for_attempt_audit(
        nav_endpoint,
        &credentials,
        &tax_number_8,
        &invoice_xml,
    ))?;
    tracing::info!(
        request_bytes = prepared.request_xml.len(),
        stage = ?stuck.stage,
        "manageInvoice envelope built; writing TX1 (RetryRequested + Attempt)"
    );

    // 7. TX1 — RetryRequested + Attempt-before-call (PR-19 /
    //    ADR-0032 §1). Two audit entries in one tx so the
    //    operator-decision and the resulting Attempt are atomically
    //    paired.
    write_retry_requested_and_attempt_audit(
        &mut conn,
        &ledger_meta,
        actor.clone(),
        &ready_invoice,
        idempotency_key,
        endpoint_audit_label,
        reason,
        &stuck,
        prepared.request_xml.clone(),
    )?;
    {
        let ledger_tx1 = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger after TX1 commit")?;
        let mirror_path = audit_ledger::mirror_path_for(&args.db);
        ledger_tx1
            .sync_mirror(&mirror_path)
            .context("sync audit-ledger mirror file after TX1 RetryRequested+Attempt commit")?;
    }

    // 8. Wire send — POST the pre-rendered envelope.
    let wire_result = runtime.block_on(manage_invoice::send_built_request(
        &prepared.transport,
        &prepared.request_xml,
    ));

    // 9. TX2 — Response or AttemptFailed.
    match wire_result {
        Ok(send_outcome) => {
            tracing::info!(
                new_transaction_id = %send_outcome.transaction_id,
                prior_transaction_id = ?stuck.prior_transaction_id,
                stage = ?stuck.stage,
                "NAV manageInvoice (retry) OK"
            );
            write_response_audit(
                &mut conn,
                &ledger_meta,
                actor.clone(),
                &ready_invoice,
                idempotency_key,
                &send_outcome.transaction_id,
                send_outcome.response_xml,
            )?;
            drop(conn);
            let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes)
                .context("open audit ledger after TX2 Response commit")?;
            let verified = ledger
                .verify_chain()
                .context("audit-ledger chain verification failed AFTER retry-submission")?;
            tracing::info!(entries_verified = verified, "audit chain verified");
            let mirror_path = audit_ledger::mirror_path_for(&args.db);
            ledger
                .sync_mirror(&mirror_path)
                .context("sync audit-ledger mirror file after TX2 Response commit")?;
            let submitted =
                ready_invoice.into_submitted(send_outcome.transaction_id.clone());
            let prior_txid_label = stuck
                .prior_transaction_id
                .as_deref()
                .unwrap_or("<no prior NAV transaction id — state-2 Pending>");
            let stage_label = match stuck.stage {
                StuckStage::Pending => "state-2 Pending",
                StuckStage::AwaitingAck => "state-3 AwaitingAck",
            };
            println!(
                "retry-submission OK ({}): invoice {} (seq {}) re-submitted -> NAV transactionId {} \
                 (prior txid {}, prior last ack {}) \
                 (audit chain verified across {} entries); \
                 run `aberp poll-ack` to drive terminal state",
                stage_label,
                submitted.id.to_prefixed_string(),
                submitted.sequence_number,
                submitted.nav_transaction_id,
                prior_txid_label,
                stuck.prior_last_ack_status.as_deref().unwrap_or("<none>"),
                verified,
            );
            Ok(())
        }
        Err(wire_err) => {
            let (error_class, error_code) =
                submission_queue::classify_attempt_failure(&wire_err);
            let error_message = format!("{wire_err}");
            let response_xml: Option<Vec<u8>> = None;
            write_attempt_failed_audit(
                &mut conn,
                &ledger_meta,
                actor.clone(),
                &ready_invoice,
                idempotency_key,
                endpoint_audit_label,
                error_class,
                error_code,
                error_message.clone(),
                response_xml,
            )?;
            drop(conn);
            let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes)
                .context("open audit ledger after TX2 AttemptFailed commit")?;
            let verified = ledger
                .verify_chain()
                .context("audit-ledger chain verification failed AFTER AttemptFailed")?;
            let mirror_path = audit_ledger::mirror_path_for(&args.db);
            ledger
                .sync_mirror(&mirror_path)
                .context("sync audit-ledger mirror file after TX2 AttemptFailed commit")?;
            tracing::error!(
                invoice_id = %ready_invoice.id.to_prefixed_string(),
                entries_verified = verified,
                error_class = error_class,
                "retry-submission: manageInvoice failed; TX2 AttemptFailed audit written"
            );
            eprintln!(
                "retry-submission FAILED for invoice {}: {} \
                 (audit chain verified across {} entries; \
                 InvoiceSubmissionAttemptFailed recorded with error_class={}); \
                 invoice remains in state-2 Pending — re-run `aberp retry-submission` \
                 (the Layer-2 queryInvoiceCheck step PR-20 / ADR-0033 §1 added \
                 to state-2 retries will reconfirm the NAV-side state before any \
                 next re-POST)",
                ready_invoice.id.to_prefixed_string(),
                error_message,
                verified,
                error_class,
            );
            Err(anyhow!(
                "retry-submission manageInvoice failed: {}",
                error_message
            ))
        }
    }
}

/// Open the audit ledger, resolve the stuck precondition. Loud-fail
/// on every `NotStuck` reason; loud-fail on idempotency-key mismatch
/// between issuance and the precondition. Returns the precondition
/// shape on success.
///
/// The idempotency-key mismatch check is defence-in-depth: the F8
/// contract pins the issuance's key to every NAV-related entry. If
/// the precondition's key differs from the billing row, something
/// has tampered with the ledger (rule 12 — fail loud).
///
/// PR-19 / ADR-0032 §4: accepts state-2 (StuckStage::Pending) in
/// addition to state-3 (StuckStage::AwaitingAck) — both share the
/// retry-submission command shape; only the
/// `prior_transaction_id` field on the `InvoiceRetryRequestedPayload`
/// differs (`None` for state-2 because no prior Response exists).
fn resolve_stuck_or_loud_fail(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
    invoice_id: &str,
    issuance_idempotency_key: &IdempotencyKey,
) -> Result<StuckPrecondition> {
    let ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to resolve retry-submission precondition")?;
    match audit_query::stuck_precondition(&ledger, invoice_id)? {
        StuckOutcome::Stuck(p) => {
            if p.idempotency_key != *issuance_idempotency_key {
                return Err(anyhow!(
                    "F8 contract violation: precondition idempotency_key '{}' \
                     does not match issuance idempotency_key '{}' — \
                     the audit ledger appears tampered or schema-drifted",
                    p.idempotency_key.to_canonical_string(),
                    issuance_idempotency_key.to_canonical_string(),
                ));
            }
            Ok(p)
        }
        StuckOutcome::NotStuck(reason) => Err(anyhow!(
            "cannot retry invoice {}: {}",
            invoice_id,
            reason.as_message()
        )),
    }
}

/// PR-19 / ADR-0032 §1: the retry prepare-for-attempt-audit bundle.
/// Mirror of `submit_invoice::PreparedSubmission`.
struct PreparedSubmission {
    transport: NavTransport,
    request_xml: Vec<u8>,
}

/// PR-19 / ADR-0032 §1 + §3: open transport, tokenExchange, build
/// envelope. Mirror of `submit_invoice::prepare_for_attempt_audit`
/// per the operator-facing-twin posture. The retry path always uses
/// `InvoiceOperation::Create` (same as the pre-PR-19 retry-submission
/// shape) — chain operations (STORNO / MODIFY) are not yet on the
/// retry surface (separate trigger).
async fn prepare_for_attempt_audit(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    invoice_xml: &[u8],
) -> Result<PreparedSubmission> {
    let transport = NavTransport::new(endpoint).context("build NAV transport")?;
    let token = token_exchange::call(&transport, credentials, tax_number_8)
        .await
        .context("NAV tokenExchange (retry)")?;
    let request_xml = manage_invoice::build_request(
        credentials,
        tax_number_8,
        &token.decoded_token,
        &[ManageInvoiceItem {
            operation: InvoiceOperation::Create,
            invoice_data_xml: invoice_xml,
        }],
    )
    .map_err(|e: NavTransportError| {
        anyhow!("manage_invoice::build_request (envelope construction; retry) failed: {e}")
    })?;
    Ok(PreparedSubmission {
        transport,
        request_xml,
    })
}

/// Scoped read tx + invoice + idempotency_key — identical contract to
/// `submit_invoice::load_issued_invoice`. Duplicated here for the same
/// reason `poll_ack` duplicates it (the two orchestration modules are
/// operator-facing twins; a future retry-specific load path would be
/// invisible if they shared a helper). Per CLAUDE.md rule 8: read
/// before write — `submit_invoice` IS the existing pattern; we mirror
/// it, not extend it.
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

/// PR-19 / ADR-0032 §1: TX1 audit-write — open one audit tx, append
/// `InvoiceRetryRequested` + `InvoiceSubmissionAttempt` in that order,
/// commit. Both entries share the F8 idempotency key. The pair lands
/// atomically so the operator's decision and the resulting Attempt
/// are inseparable in the audit chain (a crash mid-tx rolls both
/// back; a crash post-commit leaves both).
#[allow(clippy::too_many_arguments)]
fn write_retry_requested_and_attempt_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    reason: &str,
    stuck: &StuckPrecondition,
    request_xml: Vec<u8>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for retry-submission TX1")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (retry-submission TX1 RetryRequested+Attempt)")?;

    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();

    // 1. InvoiceRetryRequested — operator's decision + precondition.
    //    `prior_transaction_id` threads from the StuckPrecondition's
    //    Option directly per ADR-0032 §4: Some for state-3
    //    AwaitingAck; None for state-2 Pending.
    let retry_payload = audit_payloads::InvoiceRetryRequestedPayload::new(
        &invoice_id_str,
        idempotency_key,
        stuck.prior_transaction_id.clone(),
        stuck.prior_last_ack_status.clone(),
        reason,
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceRetryRequested,
        retry_payload.to_bytes(),
        actor.clone(),
        Some(idem_str.clone()),
    )
    .context("audit_ledger::append_in_tx InvoiceRetryRequested (retry TX1)")?;

    // 2. InvoiceSubmissionAttempt — verbatim retry-request body.
    let attempt = audit_payloads::InvoiceSubmissionAttemptPayload::new(
        &invoice_id_str,
        idempotency_key,
        endpoint_label,
        request_xml,
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceSubmissionAttempt,
        attempt.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttempt (retry TX1)")?;

    tx.commit()
        .context("commit DuckDB transaction (retry-submission TX1 RetryRequested+Attempt)")?;
    Ok(())
}

/// PR-19 / ADR-0032 §1: TX2 success audit-write — append
/// `InvoiceSubmissionResponse` in its own tx after the wire send
/// returns success. Mirror of `submit_invoice::write_response_audit`.
fn write_response_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    transaction_id: &str,
    response_xml: Vec<u8>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for retry-submission TX2 Response")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (retry-submission TX2 Response audit append)")?;
    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();
    let response = audit_payloads::InvoiceSubmissionResponsePayload::new(
        &invoice_id_str,
        idempotency_key,
        transaction_id,
        response_xml,
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceSubmissionResponse,
        response.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceSubmissionResponse (retry TX2)")?;
    tx.commit()
        .context("commit DuckDB transaction (retry-submission TX2 Response audit append)")?;
    Ok(())
}

/// PR-19 / ADR-0032 §1 + §2: TX2 failure audit-write — append
/// `InvoiceSubmissionAttemptFailed` in its own tx after the wire
/// send returns an error. Mirror of
/// `submit_invoice::write_attempt_failed_audit`.
#[allow(clippy::too_many_arguments)]
fn write_attempt_failed_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    error_class: &'static str,
    error_code: Option<String>,
    error_message: String,
    response_xml: Option<Vec<u8>>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for retry-submission TX2 AttemptFailed")?;
    let tx = conn.transaction().context(
        "begin DuckDB transaction (retry-submission TX2 AttemptFailed audit append)",
    )?;
    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();
    let failed = audit_payloads::InvoiceSubmissionAttemptFailedPayload::new(
        &invoice_id_str,
        idempotency_key,
        endpoint_label,
        error_class,
        error_code,
        error_message,
        response_xml,
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceSubmissionAttemptFailed,
        failed.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttemptFailed (retry TX2)")?;
    tx.commit().context(
        "commit DuckDB transaction (retry-submission TX2 AttemptFailed audit append)",
    )?;
    Ok(())
}

/// PR-20 / ADR-0033 §1: the three decisions Phase 0 emits to its
/// caller. The orchestration matches on this to either skip
/// Phase 1+2 (Exists), abort (Failure), or proceed (Absent).
#[derive(Debug)]
enum Layer2Decision {
    /// NAV has the invoice. Skip the manageInvoice re-POST.
    SkipRePost,
    /// NAV does not have the invoice. Proceed to the PR-19
    /// prepare → TX1 → wire → TX2 path.
    ProceedToRePost,
    /// queryInvoiceCheck failed at some layer. Abort the retry;
    /// operator re-runs later. The wrapped String is the operator-
    /// visible failure message (already recorded in the audit
    /// payload's `failure_message` field by the time this decision
    /// is returned).
    Abort(String),
}

/// PR-20 / ADR-0033 §1: derive the NAV-facing invoice number
/// string (`"{series_code}/{seq:05}"`) from the loaded ReadyInvoice.
/// Mirror of `observe_receiver_confirmation::load_base_nav_invoice_number`
/// — the same canonical NAV-facing invoice number shape per
/// ADR-0009 §3 and `nav_xml::render_invoice_data`.
///
/// Opens a fresh `DuckDbBillingStore` to consult the
/// `find_series_by_id` port. The caller already has the `ReadyInvoice`
/// in hand (loaded by `load_issued_invoice`); the only missing piece
/// is the series code, which the billing store resolves by ULID.
fn derive_nav_invoice_number(
    db_path: &std::path::Path,
    invoice: &ReadyInvoice,
) -> Result<String> {
    let store = DuckDbBillingStore::open(db_path)
        .with_context(|| format!("open billing DuckDB at {} for Layer-2 series lookup", db_path.display()))?;
    let series: InvoiceSeries = store
        .find_series_by_id(invoice.series_id)
        .context("billing::find_series_by_id (retry-submission Layer-2 series lookup)")?
        .ok_or_else(|| {
            anyhow!(
                "invoice {} references series_id {} which is not present in \
                 invoice_series — tenant DB appears tampered between invoice \
                 insertion and retry-submission",
                invoice.id.to_prefixed_string(),
                invoice.series_id.to_prefixed_string()
            )
        })?;
    Ok(format!(
        "{}/{:05}",
        series.code.as_str(),
        invoice.sequence_number
    ))
}

/// PR-20 / ADR-0033 §1: perform Phase 0 — the Layer-2
/// `queryInvoiceCheck` disambiguation step. Drives the NAV wire
/// call, writes the `InvoiceCheckPerformed` audit entry in TX0,
/// syncs the mirror, and returns a [`Layer2Decision`] for the
/// caller to act on.
///
/// On wire success: the boolean `check_result` maps via
/// [`QueryInvoiceCheckOutcome::from_check_result`] to either
/// `Exists` (→ [`Layer2Decision::SkipRePost`]) or `Absent` (→
/// [`Layer2Decision::ProceedToRePost`]). The payload's `outcome`
/// is `"exists"` or `"absent"`; the failure fields are `None`.
///
/// On wire failure: the typed error classifies through
/// [`submission_queue::classify_attempt_failure`] (extended in
/// PR-20 to cover the five `QueryInvoiceCheck*` variants); the
/// audit payload's `outcome` is `"failure"` with the typed
/// `failure_class` / `failure_code` / `failure_message`. The
/// returned [`Layer2Decision::Abort`] wraps the operator-visible
/// failure message.
#[allow(clippy::too_many_arguments)]
fn perform_layer_2_check(
    runtime: &tokio::runtime::Runtime,
    nav_endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    nav_invoice_number: &str,
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    endpoint_audit_label: &'static str,
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
) -> Result<Layer2Decision> {
    // Build transport. (No tokenExchange — queryInvoiceCheck is a
    // NAV query operation per ADR-0009 §4 / ADR-0033 §3.)
    let transport = NavTransport::new(nav_endpoint)
        .context("build NAV transport for Layer-2 queryInvoiceCheck")?;

    // Build the request envelope. Loud-fail on any envelope-
    // construction error.
    let request_xml = query_invoice_check::build_request(
        credentials,
        tax_number_8,
        nav_invoice_number,
        InvoiceDirection::Outbound,
    )
    .map_err(|e: NavTransportError| {
        anyhow!("query_invoice_check::build_request (envelope construction) failed: {e}")
    })?;

    // Wire send.
    let wire_result =
        runtime.block_on(query_invoice_check::send_built_request(&transport, &request_xml));

    // Classify outcome + build payload.
    let (decision, payload) = match wire_result {
        Ok(send_outcome) => {
            let outcome_enum = QueryInvoiceCheckOutcome::from_check_result(send_outcome.check_result);
            let payload = audit_payloads::InvoiceCheckPerformedPayload::new_for_outcome(
                &invoice.id.to_prefixed_string(),
                idempotency_key,
                endpoint_audit_label,
                nav_invoice_number,
                outcome_enum.as_audit_str(),
                request_xml.clone(),
                send_outcome.response_xml,
            );
            let decision = match outcome_enum {
                QueryInvoiceCheckOutcome::Exists => Layer2Decision::SkipRePost,
                QueryInvoiceCheckOutcome::Absent => Layer2Decision::ProceedToRePost,
            };
            (decision, payload)
        }
        Err(wire_err) => {
            let (failure_class, failure_code) =
                submission_queue::classify_attempt_failure(&wire_err);
            let failure_message = format!("{wire_err}");
            // queryInvoiceCheck::send_built_request returns Err
            // without surfacing the response bytes (matches the
            // convention every other operations module uses); for
            // failure outcomes the audit entry's response_xml is
            // therefore None. A future amendment that wires
            // response-body capture for NAV-error funcCode bodies
            // could populate this — out of PR-20 scope.
            let response_xml: Option<Vec<u8>> = None;
            let payload = audit_payloads::InvoiceCheckPerformedPayload::new_for_failure(
                &invoice.id.to_prefixed_string(),
                idempotency_key,
                endpoint_audit_label,
                nav_invoice_number,
                request_xml.clone(),
                response_xml,
                failure_class,
                failure_code,
                failure_message.clone(),
            );
            (Layer2Decision::Abort(failure_message), payload)
        }
    };

    // TX0 — write the InvoiceCheckPerformed audit entry.
    write_check_performed_audit(conn, ledger_meta, actor, idempotency_key, payload)?;

    // Sync mirror after TX0 commit. Same pattern as every other
    // post-tx mirror sync; the mirror is the secondary-evidence
    // source per ADR-0030 §2.
    {
        let ledger_tx0 = Ledger::open(db_path, tenant, binary_hash)
            .context("open audit ledger after TX0 InvoiceCheckPerformed commit")?;
        let mirror_path = audit_ledger::mirror_path_for(db_path);
        ledger_tx0.sync_mirror(&mirror_path).context(
            "sync audit-ledger mirror file after TX0 InvoiceCheckPerformed commit",
        )?;
    }

    Ok(decision)
}

/// PR-20 / ADR-0033 §1: TX0 audit-write — open one DuckDB
/// transaction, append the `InvoiceCheckPerformed` entry, commit.
/// One entry per Phase 0 execution regardless of outcome (Exists /
/// Absent / Failure).
fn write_check_performed_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    idempotency_key: IdempotencyKey,
    payload: audit_payloads::InvoiceCheckPerformedPayload,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for retry-submission TX0 InvoiceCheckPerformed")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (retry-submission TX0 InvoiceCheckPerformed)")?;
    let idem_str = idempotency_key.to_canonical_string();
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceCheckPerformed,
        payload.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceCheckPerformed (retry TX0)")?;
    tx.commit()
        .context("commit DuckDB transaction (retry-submission TX0 InvoiceCheckPerformed)")?;
    Ok(())
}

/// 8-digit base of a Hungarian tax number. Mirror of
/// `submit_invoice::parse_tax_number_8` — same loud-fail shape.
/// Duplicated for the same operator-facing-twin reason `poll_ack`
/// duplicates it.
fn parse_tax_number_8(raw: &str) -> Result<String> {
    let base = raw.split('-').next().unwrap_or(raw);
    if base.len() != 8 || !base.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!(
            "--tax-number '{raw}' base is not 8 ASCII digits \
             (expected forms: 12345678, 12345678-1, 12345678-1-42)"
        ));
    }
    Ok(base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_must_be_non_empty() {
        // Empty / whitespace-only reason is loud-failed before any NAV
        // call. The test does not need a full RetrySubmissionArgs to
        // verify this: it pins the trim() contract directly.
        assert!("   ".trim().is_empty());
        assert!("".trim().is_empty());
        assert!(!"x".trim().is_empty());
    }

    #[test]
    fn tax_number_8_parses_same_as_submit_invoice() {
        // Same contract as submit_invoice::parse_tax_number_8 and
        // poll_ack::parse_tax_number_8. If they ever drift, the three
        // operator-facing twins will produce confusingly different
        // errors on the same operator input.
        assert_eq!(parse_tax_number_8("12345678").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1-42").unwrap(), "12345678");
        assert!(parse_tax_number_8("1234567").is_err());
        assert!(parse_tax_number_8("1234567X").is_err());
        assert!(parse_tax_number_8("123456789-1-42").is_err());
    }
}
