//! Orchestration for the `aberp drain-pending-retries` subcommand
//! (PR-42, F45 closure — automatic state-2 retry loop).
//!
//! Walks the audit ledger, classifies state-2 Pending invoices via
//! [`crate::submission_queue::pending_retries_from_ledger`], and drives
//! each one through the same Layer-2 + TX1 + wire + TX2 pipeline that
//! the operator-confirmed `aberp retry-submission` uses (PR-19 /
//! ADR-0032 §1, PR-20 / ADR-0033 §1). The per-invoice driver mirrors
//! `retry_submission::run`'s flow inline per the operator-facing-twin
//! posture (CLAUDE.md rule 11 — match codebase conventions; both
//! `submit_invoice` ↔ `drain_submission_queue` and `retry_submission`
//! ↔ this module are inline-mirrored rather than extracted to a
//! shared helper).
//!
//! # Why an automatic loop closes F45
//!
//! Pre-PR-42, state-2 Pending was recoverable only via the operator
//! typing `aberp retry-submission --invoice-id <id> --invoice-xml
//! <path> --reason <text>` once per stuck invoice. F45 names the
//! gap: a NAV transport flake that fails N submissions mid-flight
//! leaves N invoices stuck, each requiring a manual command. The
//! automatic loop walks the same classifier the operator command
//! consumes (`audit_query::stuck_precondition`'s state-2 surface) and
//! drives the same per-invoice pipeline against every stuck invoice
//! in one run.
//!
//! # Pipeline (whole run)
//!
//!   1. Parse + validate CLI args.
//!   2. Load `NavCredentials` from the OS keychain (loud-fail per
//!      ADR-0020 §3 — same posture as every other NAV-touching
//!      subcommand).
//!   3. Compute the binary hash + build `LedgerMeta`.
//!   4. Resolve pending retries via
//!      [`crate::submission_queue::pending_retries_from_ledger`]. FIFO
//!      by issuance date.
//!   5. Drive the per-invoice pipeline in a loop (see below).
//!   6. Print the run summary.
//!
//! # Pipeline (per invoice)
//!
//!   a. Loud-fail if `nav_xml_path` is `None` (pre-PR-18 entries —
//!      the operator drains those via the manual `aberp retry-
//!      submission --invoice-xml <path>` command).
//!   b. Read the XML bytes from disk.
//!   c. Validate via `aberp_nav_xsd_validator::validate_invoice_data`
//!      — same pre-NAV gate every existing `submit-*` / `drain-*`
//!      command runs.
//!   d. Load the issued invoice from billing (idempotency-key sanity
//!      check vs. the classifier's F8-derived key — defence-in-depth
//!      per CLAUDE.md rule 12).
//!   e. Read the NAV-facing invoice number from the invoice's on-disk
//!      NAV XML `<invoiceNumber>` (S184/S190) — the byte-exact string
//!      NAV holds, never re-synthesised from the series code + seq.
//!   f. **TX0 — Layer-2 disambiguation (PR-20 / ADR-0033 §1).**
//!      `queryInvoiceCheck` against the NAV-facing invoice number.
//!      One `InvoiceCheckPerformed` audit entry per execution.
//!      - **Exists** — NAV already has the invoice. Skip the
//!        re-POST; the per-invoice summary points the operator at
//!        `aberp recover-from-nav` (PR-21 / ADR-0034). Continue
//!        the drain loop.
//!      - **Absent** — NAV does NOT have the invoice. Proceed to
//!        TX1 + wire + TX2 below.
//!      - **Failure** — `queryInvoiceCheck` itself failed. Abort
//!        this invoice's retry per ADR-0033 §"Surfaced conflict 1
//!        Reading A". Classify the failure as transport vs.
//!        application (transport stops the drain; application
//!        continues to the next invoice — same fork as
//!        `drain-submission-queue`).
//!   g. **TX1 — RetryRequested + Attempt-before-call (ADR-0032 §1).**
//!      Two audit entries in one tx so the auto-reason and the
//!      fresh Attempt are atomically paired. The auto-reason is a
//!      fixed string naming the drain run; the operator's decision
//!      is "run drain-pending-retries", and the audit-evidence
//!      chain captures the per-invoice retry through the same
//!      RetryRequested+Attempt shape the manual command uses.
//!   h. **Wire send.**
//!   i. **TX2 — Response on success, AttemptFailed on failure
//!      (ADR-0032 §1).** One audit entry in its own tx.
//!   j. Verify the audit chain; sync the mirror; print the per-
//!      invoice OK or FAILED line.
//!
//! # Transport-vs-application fork (ADR-0031 §4 / ADR-0032 §2)
//!
//! Mirrors `drain-submission-queue`: a transport-class wire failure
//! at any phase (Layer-2 check or manageInvoice POST) short-circuits
//! the FIFO loop — the remaining pending retries stay pending for
//! the next drain run. Application-class failures surface per-invoice
//! LOUD and the loop continues to the next invoice. Layer-2 Exists
//! and Layer-2 Failure outcomes do NOT short-circuit (Exists is a
//! per-invoice "skip re-POST" decision; Failure is a per-invoice
//! abort that the operator triages later).
//!
//! # F12 four-edit ritual status
//!
//! NOT FIRED. Every audit entry the drain writes
//! (`InvoiceCheckPerformed`, `InvoiceRetryRequested`,
//! `InvoiceSubmissionAttempt`, `InvoiceSubmissionResponse`,
//! `InvoiceSubmissionAttemptFailed`) is an existing EventKind
//! variant. F45 closure adds no new audit semantics; it composes the
//! PR-19 + PR-20 surface into an automatic loop.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT process state-1 Draft invoices. `drain-
//!     submission-queue` is the operator surface for those.
//!   - It does NOT process state-3 AwaitingAck invoices. The
//!     classifier filters only state-2 Pending; state-3 is
//!     recoverable via `aberp retry-submission` (PR-19) or
//!     `aberp poll-ack` (PR-7-C-2) depending on the operator's
//!     intent.
//!   - It does NOT support `--xml-path-override` for pre-PR-18
//!     entries. Pre-PR-18 state-2 invoices loud-fail at the
//!     `nav_xml_path: None` check; the operator drains those via
//!     the manual `aberp retry-submission --invoice-xml <path>`
//!     command. The fall-through path can additively gain an
//!     override flag later if operational evidence surfaces a
//!     non-trivial pre-PR-18 backlog.
//!   - It does NOT take a `--reason` flag. The auto-reason names
//!     the drain run; per-invoice operator-decision rationale lives
//!     on the manual `aberp retry-submission --reason` command.
//!   - It does NOT poll `queryTransactionStatus`. The operator runs
//!     `aberp poll-ack` after the drain (or schedules it
//!     independently); the drain is one-shot per per-invoice retry.
//!   - It does NOT enforce a per-invoice backoff or cooldown — F50
//!     names the operator-tunable threshold trigger.

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{self as billing, IdempotencyKey, ReadyInvoice};
use aberp_nav_transport::{
    operations::{
        manage_invoice,
        query_invoice_check::{self, QueryInvoiceCheckOutcome},
        token_exchange,
    },
    soap::{InvoiceDirection, InvoiceOperation, ManageInvoiceItem},
    NavCredentials, NavEndpoint, NavTransport, NavTransportError,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::{DrainPendingRetriesArgs, NavEnv};
use crate::submission_queue::{self, PendingRetry};

/// Fixed audit-payload `reason` text the drain writes on each
/// `InvoiceRetryRequested` entry. The operator's decision is to run
/// `aberp drain-pending-retries`; the per-invoice retry inherits that
/// decision via this string. Distinct from
/// `aberp retry-submission --reason` (operator-supplied per invoice).
const AUTO_REASON: &str =
    "automatic state-2 retry (aberp drain-pending-retries; F45 closure / ADR-0032 §4)";

// ──────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────

pub fn run(args: &DrainPendingRetriesArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "drain_pending_retries",
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

    // 2. Load NAV credentials BEFORE touching the DB.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for drain-pending-retries"
    );

    // 3. Compute binary hash + LedgerMeta.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 3a. ADR-0098/0099 — route EVERY DuckDB access through the ONE shared
    //     process-wide `aberp_db::Handle`. Pre-fix the drain opened an
    //     independent `Ledger`/`Connection`/`DuckDbBillingStore` per invoice
    //     (up to a second live instance while a `&mut Connection` was still
    //     alive), each a separate checkpoint actor racing the same file —
    //     the duckdb#23046 / S332 checkpoint-ART corruption locus
    //     (`storage/mod.rs` names `Ledger::open` as the trigger;
    //     `submit_invoice.rs` documents its own migration OFF it). The
    //     Handle serialises writes behind one writer mutex, hands out
    //     coherent `try_clone` reads, and runs the lockstep mirror sync on
    //     every WriteGuard drop — so the drain never holds two live
    //     instances. Same pattern as `submit_invoice::run`.
    let db = aberp_db::Handle::open_default(&args.db, tenant.clone())
        .with_context(|| format!("open shared DuckDB handle at {}", args.db.display()))?;

    // 4. Resolve pending retries via the audit-ledger walker. FIFO
    //    by issue date. Read through a coherent `try_clone` of the ONE
    //    instance (ADR-0098 C2), never an independent `Ledger::open`.
    let pending = {
        let conn = db
            .read()
            .context("shared read: pending-retries walk (ADR-0098 C2)")?;
        let ledger = Ledger::from_connection(conn, tenant.clone(), binary_hash_bytes);
        submission_queue::pending_retries_from_ledger(&ledger)?
    };
    let pending_count = pending.len();
    tracing::info!(
        pending_count = pending_count,
        "drain-pending-retries: state-2 invoices resolved"
    );

    if pending.is_empty() {
        println!("drain-pending-retries: 0 state-2 invoices pending retry; nothing to do.");
        return Ok(());
    }

    // 5. Drive the per-invoice pipeline.
    let limit = if args.max_invoices == 0 {
        pending_count
    } else {
        args.max_invoices.min(pending_count)
    };
    let mut ok_count: usize = 0;
    let mut skipped_exists_count: usize = 0;
    // S390/E — invoices skipped because another process holds the
    // cross-process submission lock for them (a concurrent serve click /
    // submit / drain). Distinct from `skipped_exists_count` (Layer-2
    // found it already at NAV).
    let mut skipped_in_progress: usize = 0;
    let mut application_error_count: usize = 0;
    let mut transport_error: Option<String> = None;
    let mut stop_index: Option<usize> = None;

    for (idx, retry) in pending.iter().take(limit).enumerate() {
        // S390/E — hold the cross-process submission lock across this
        // invoice's Layer-2 check + re-POST so a concurrent process
        // cannot double-submit it. `None` = in progress elsewhere → skip.
        let _invoice_lock =
            match crate::submission_lock::try_acquire(&args.db, &args.tenant, &retry.invoice_id) {
                Ok(Some(guard)) => guard,
                Ok(None) => {
                    skipped_in_progress += 1;
                    tracing::warn!(
                        invoice_id = %retry.invoice_id,
                        "drain-pending-retries: submission already in progress in another \
                         process; skipping this invoice"
                    );
                    eprintln!(
                        "drain-pending-retries: invoice {} SKIPPED \
                         (submission in progress in another process)",
                        retry.invoice_id
                    );
                    continue;
                }
                Err(e) => {
                    application_error_count += 1;
                    tracing::error!(
                        invoice_id = %retry.invoice_id,
                        "drain-pending-retries: submission-lock error; continuing. {e:#}"
                    );
                    eprintln!(
                        "drain-pending-retries: invoice {} lock error (continuing): {e:#}",
                        retry.invoice_id
                    );
                    continue;
                }
            };
        let outcome = drive_one_retry(
            retry,
            &db,
            nav_endpoint,
            endpoint_audit_label,
            &credentials,
            &tax_number_8,
            &ledger_meta,
            tenant.clone(),
            binary_hash_bytes,
            actor.clone(),
        );

        match outcome {
            Ok(DriveOutcome::RetriedOk) => {
                ok_count += 1;
            }
            Ok(DriveOutcome::SkippedExists) => {
                skipped_exists_count += 1;
            }
            Err(DrainRetryError::Transport(msg)) => {
                transport_error = Some(msg.clone());
                stop_index = Some(idx);
                tracing::error!(
                    invoice_id = %retry.invoice_id,
                    "drain-pending-retries: NAV transport error; stopping. {}",
                    msg
                );
                eprintln!(
                    "drain-pending-retries: NAV transport error on invoice {}; \
                     {} retry(ies) sent, {} skipped (NAV-side Exists), \
                     {} application failure(s), {} pending retries remaining. \
                     Re-run when NAV is reachable. Error: {}",
                    retry.invoice_id,
                    ok_count,
                    skipped_exists_count,
                    application_error_count,
                    pending_count - ok_count - skipped_exists_count - application_error_count,
                    msg
                );
                break;
            }
            Err(DrainRetryError::Application(msg)) => {
                application_error_count += 1;
                tracing::error!(
                    invoice_id = %retry.invoice_id,
                    "drain-pending-retries: per-invoice application error; continuing. {}",
                    msg
                );
                eprintln!(
                    "drain-pending-retries: invoice {} FAILED (continuing to next): {}",
                    retry.invoice_id, msg
                );
            }
        }
    }

    // 6. Run summary. LOUD per CLAUDE.md rule 12: every count is
    //    surfaced and any short-circuit is named.
    println!(
        "drain-pending-retries: retried {} of {} state-2 invoices \
         (skipped NAV-side Exists: {}, skipped-in-progress: {}, application errors: {}, \
         transport error: {}, max-invoices: {}). \
         Stopped early: {}.",
        ok_count,
        pending_count,
        skipped_exists_count,
        skipped_in_progress,
        application_error_count,
        transport_error.as_deref().unwrap_or("none"),
        if args.max_invoices == 0 {
            "unbounded".to_string()
        } else {
            args.max_invoices.to_string()
        },
        match stop_index {
            Some(i) => format!("yes (at index {})", i),
            None => "no".to_string(),
        }
    );

    if let Some(msg) = transport_error {
        return Err(anyhow!(
            "drain-pending-retries: transport error short-circuited the run: {}",
            msg
        ));
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Per-invoice driver
// ──────────────────────────────────────────────────────────────────────

/// Per-invoice success classification. `RetriedOk` is the
/// Absent-then-Response path; `SkippedExists` is the Layer-2-Exists
/// path (no re-POST happened; operator next-step is
/// `recover-from-nav`).
#[derive(Debug)]
enum DriveOutcome {
    RetriedOk,
    SkippedExists,
}

/// Typed per-invoice error so the run loop can fork on transport vs.
/// application. Mirror of `drain_submission_queue::DrainPerInvoiceError`
/// per the operator-facing-twin posture.
#[derive(Debug)]
enum DrainRetryError {
    /// Stop the drain. Transport-layer failure at Layer-2 OR
    /// manageInvoice.
    Transport(String),
    /// Continue the drain. NAV-side application error, credential
    /// error, XSD-validation error, file-read error, audit-write
    /// error, Layer-2 Failure outcome — anything that's not a
    /// transport failure.
    Application(String),
}

#[allow(clippy::too_many_arguments)]
fn drive_one_retry(
    retry: &PendingRetry,
    db: &aberp_db::HandleArc,
    nav_endpoint: NavEndpoint,
    endpoint_audit_label: &'static str,
    credentials: &NavCredentials,
    tax_number_8: &str,
    ledger_meta: &LedgerMeta,
    tenant: TenantId,
    binary_hash_bytes: aberp_audit_ledger::BinaryHash,
    actor: Actor,
) -> Result<DriveOutcome, DrainRetryError> {
    // a. Resolve the on-disk XML path.
    let xml_path = match retry.nav_xml_path.as_deref() {
        Some(p) => p.to_string(),
        None => {
            return Err(DrainRetryError::Application(format!(
                "no NAV XML path available for invoice {}: \
                 the audit payload's nav_xml_path is None (this invoice was issued by a pre-PR-18 binary). \
                 drain-pending-retries does not accept an override flag; \
                 retry this invoice manually via \
                 `aberp retry-submission --invoice-id {} --invoice-xml <path> --reason <text>`.",
                retry.invoice_id, retry.invoice_id
            )));
        }
    };

    // b. Read the XML bytes.
    let invoice_xml = std::fs::read(&xml_path).map_err(|e| {
        DrainRetryError::Application(format!("read NAV InvoiceData XML from {}: {e}", xml_path))
    })?;
    if invoice_xml.is_empty() {
        return Err(DrainRetryError::Application(format!(
            "invoice XML at {} is empty",
            xml_path
        )));
    }

    // c. Validate via the v3.0 invariant check (ADR-0022).
    aberp_nav_xsd_validator::validate_invoice_data(&invoice_xml).map_err(|e| {
        DrainRetryError::Application(format!(
            "NAV InvoiceData v3.0 invariant check (ADR-0022) failed for {}: {e}",
            xml_path
        ))
    })?;

    // d. Load the issued invoice + idempotency key from billing via a
    //    coherent shared READ clone of the ONE instance (ADR-0098 C2).
    //    Defence-in-depth F8 check: the billing-side key must match
    //    the classifier's key (which came from the Attempt payload).
    let (ready_invoice, billing_idempotency_key) = {
        let mut conn = db.read().map_err(|e| {
            DrainRetryError::Application(format!(
                "shared read: load issued invoice for drain-pending-retries (ADR-0098 C2): {e}"
            ))
        })?;
        load_issued_invoice(&mut conn, &retry.invoice_id)
            .map_err(|e| DrainRetryError::Application(format!("{e:#}")))?
    };
    if billing_idempotency_key != retry.idempotency_key {
        return Err(DrainRetryError::Application(format!(
            "F8 contract violation: billing idempotency_key '{}' does not match \
             classifier idempotency_key '{}' for invoice {} — the audit ledger \
             or billing store appears tampered or schema-drifted",
            billing_idempotency_key.to_canonical_string(),
            retry.idempotency_key.to_canonical_string(),
            retry.invoice_id,
        )));
    }

    // e. Read the NAV-facing invoice number from the invoice's on-disk
    //    NAV XML `<invoiceNumber>` — the byte-exact string NAV holds on
    //    file (S184/S190). Re-deriving it from the billing series code +
    //    sequence silently drifts: the legacy `INV-default` series
    //    literal is not the number NAV was given (`ABERP/2026/00009`), so
    //    `queryInvoiceCheck` would ask NAV about a number it never saw ⇒
    //    always Absent ⇒ the Layer-2 duplicate guard never fires.
    let nav_invoice_number = nav_query_invoice_number(std::path::Path::new(&xml_path))
        .map_err(|e| DrainRetryError::Application(format!("{e:#}")))?;

    // f. Build tokio runtime + Phase 0: Layer-2 disambiguation.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            DrainRetryError::Application(format!(
                "build tokio current-thread runtime for drain-pending-retries NAV calls: {e}"
            ))
        })?;
    let layer2_decision = perform_layer_2_check(
        &runtime,
        nav_endpoint,
        credentials,
        tax_number_8,
        &nav_invoice_number,
        db,
        ledger_meta,
        actor.clone(),
        &ready_invoice,
        retry.idempotency_key,
        endpoint_audit_label,
    )
    .map_err(|e| DrainRetryError::Application(format!("{e:#}")))?;
    match layer2_decision {
        Layer2Decision::SkipRePost => {
            tracing::info!(
                invoice_id = %retry.invoice_id,
                nav_invoice_number = %nav_invoice_number,
                "drain-pending-retries: Layer-2 Exists — re-POST skipped"
            );
            println!(
                "drain-pending-retries: invoice {} -> Layer-2 Exists (NAV already has it; \
                 re-POST skipped; run `aberp recover-from-nav --invoice-id {} \
                 --tax-number ... --endpoint {{test|production}}` to reconstruct the local \
                 Response chain, then `aberp poll-ack` to drive terminal state)",
                retry.invoice_id, retry.invoice_id
            );
            return Ok(DriveOutcome::SkippedExists);
        }
        Layer2Decision::Abort(msg) => {
            // Layer-2 itself failed. The TX0 InvoiceCheckPerformed
            // (outcome=failure) entry is already written by
            // perform_layer_2_check. Classify the wire error so the
            // drain loop's fork (transport→break, application→
            // continue) matches drain-submission-queue.
            return Err(classify_layer_2_failure(&msg));
        }
        Layer2Decision::ProceedToRePost => {
            tracing::info!(
                invoice_id = %retry.invoice_id,
                nav_invoice_number = %nav_invoice_number,
                "drain-pending-retries: Layer-2 Absent — proceeding to TX1+wire+TX2"
            );
        }
    }

    // g. NAV prepare: tokenExchange + build_request. NO wire send yet.
    //    S381/F1 — the envelope operation is the ledger-derived one
    //    carried on `PendingRetry::operation`, not a hard-coded CREATE:
    //    a stuck STORNO / MODIFY must re-POST under its own operation.
    let prepared = runtime
        .block_on(prepare_for_attempt_audit(
            nav_endpoint,
            credentials,
            tax_number_8,
            &invoice_xml,
            retry.operation,
        ))
        .map_err(classify_nav_error)?;

    // h. TX1 — RetryRequested + Attempt-before-call (ADR-0032 §1).
    //    Written through the shared writer; the WriteGuard's drop hook
    //    runs the lockstep mirror sync (ADR-0098 1b), so no separate
    //    Ledger::open + sync_mirror is needed.
    {
        let mut conn = db.write().map_err(|e| {
            DrainRetryError::Application(format!(
                "shared writer: drain-pending-retries TX1 audit for invoice {}: {e}",
                retry.invoice_id
            ))
        })?;
        write_retry_requested_and_attempt_audit(
            &mut conn,
            ledger_meta,
            actor.clone(),
            &ready_invoice,
            retry.idempotency_key,
            endpoint_audit_label,
            AUTO_REASON,
            prepared.request_xml.clone(),
        )
        .map_err(|e| DrainRetryError::Application(format!("{e:#}")))?;
        // WriteGuard drop -> lockstep sync_mirror.
    }
    tracing::info!(
        invoice_id = %retry.invoice_id,
        "drain-pending-retries TX1 RetryRequested+Attempt audit committed; sending manageInvoice"
    );

    // i. Wire send.
    let wire_result = runtime.block_on(manage_invoice::send_built_request(
        &prepared.transport,
        &prepared.request_xml,
    ));

    // j. TX2 — Response on success, AttemptFailed on failure. Written
    //    through the shared writer (the WriteGuard drop runs the lockstep
    //    mirror sync); the post-commit verify reuses a coherent shared
    //    READ clone via `Ledger::from_connection`, never an independent
    //    re-open (ADR-0098 C2 / the duckdb#23046 checkpoint-replay locus).
    match wire_result {
        Ok(send_outcome) => {
            {
                let mut conn = db.write().map_err(|e| {
                    DrainRetryError::Application(format!(
                        "shared writer: drain-pending-retries TX2 Response audit for invoice {}: {e}",
                        retry.invoice_id
                    ))
                })?;
                write_response_audit(
                    &mut conn,
                    ledger_meta,
                    actor,
                    &ready_invoice,
                    retry.idempotency_key,
                    &send_outcome.transaction_id,
                    send_outcome.response_xml,
                )
                .map_err(|e| DrainRetryError::Application(format!("{e:#}")))?;
                // WriteGuard drop -> lockstep sync_mirror.
            }
            let verified = verify_chain_reusing_read(db, tenant, binary_hash_bytes).map_err(|e| {
                DrainRetryError::Application(format!(
                    "audit-ledger chain verification failed AFTER drain-pending-retries TX2 Response commit for invoice {}: {e:#}",
                    retry.invoice_id
                ))
            })?;
            tracing::info!(
                invoice_id = %retry.invoice_id,
                transaction_id = %send_outcome.transaction_id,
                "NAV manageInvoice OK (drain-pending-retries)"
            );
            println!(
                "drain-pending-retries: invoice {} -> NAV transactionId {} (audit chain verified across {} entries)",
                retry.invoice_id, send_outcome.transaction_id, verified
            );
            Ok(DriveOutcome::RetriedOk)
        }
        Err(wire_err) => {
            let (error_class, error_code) = submission_queue::classify_attempt_failure(&wire_err);
            let error_message = format!("{wire_err}");
            let response_xml: Option<Vec<u8>> = None;
            {
                let mut conn = db.write().map_err(|e| {
                    DrainRetryError::Application(format!(
                        "shared writer: drain-pending-retries TX2 AttemptFailed audit for invoice {}: {e}",
                        retry.invoice_id
                    ))
                })?;
                write_attempt_failed_audit(
                    &mut conn,
                    ledger_meta,
                    actor,
                    &ready_invoice,
                    retry.idempotency_key,
                    endpoint_audit_label,
                    error_class,
                    error_code,
                    error_message.clone(),
                    response_xml,
                )
                .map_err(|e| DrainRetryError::Application(format!("{e:#}")))?;
                // WriteGuard drop -> lockstep sync_mirror.
            }
            let _ = verify_chain_reusing_read(db, tenant, binary_hash_bytes).map_err(|e| {
                DrainRetryError::Application(format!(
                    "audit-ledger chain verification failed AFTER drain-pending-retries TX2 AttemptFailed commit for invoice {}: {e:#}",
                    retry.invoice_id
                ))
            })?;
            Err(classify_nav_error(wire_err))
        }
    }
}

/// Verify the audit chain after a committed write by reusing a coherent
/// shared READ clone of the ONE instance (`Ledger::from_connection`),
/// never an independent `Ledger::open` — the same "reuse, never re-open"
/// discipline `submit_invoice::verify_chain_and_sync_reusing_conn` uses
/// to keep the duckdb#23046 / S332 checkpoint-replay assertion
/// unreachable. The mirror was already synced by the WriteGuard's
/// lockstep drop hook (ADR-0098 1b), so this only verifies.
fn verify_chain_reusing_read(
    db: &aberp_db::HandleArc,
    tenant: TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
) -> Result<u64> {
    let conn = db
        .read()
        .context("shared read: drain-pending-retries post-commit verify (ADR-0098 C2)")?;
    let ledger = Ledger::from_connection(conn, tenant, binary_hash);
    ledger
        .verify_chain()
        .context("verify audit chain after drain-pending-retries commit (reusing shared read)")
}

/// PR-20 / ADR-0033 §1: the three decisions Phase 0 emits. Mirror of
/// `retry_submission::Layer2Decision` per the operator-facing-twin
/// posture.
#[derive(Debug)]
enum Layer2Decision {
    /// NAV has the invoice. Skip the manageInvoice re-POST.
    SkipRePost,
    /// NAV does not have the invoice. Proceed to TX1 + wire + TX2.
    ProceedToRePost,
    /// queryInvoiceCheck failed at some layer. Wrapped String is
    /// the operator-visible failure message (already recorded in
    /// the `InvoiceCheckPerformed(outcome=failure)` audit entry
    /// by the time this is returned).
    Abort(String),
}

/// PR-20 / ADR-0033 §1: perform Phase 0 — drive `queryInvoiceCheck`,
/// write the `InvoiceCheckPerformed` audit entry (TX0), return the
/// drive decision. Mirror of `retry_submission::perform_layer_2_check`
/// per the operator-facing-twin posture.
#[allow(clippy::too_many_arguments)]
fn perform_layer_2_check(
    runtime: &tokio::runtime::Runtime,
    nav_endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    nav_invoice_number: &str,
    db: &aberp_db::HandleArc,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    endpoint_audit_label: &'static str,
) -> Result<Layer2Decision> {
    let transport = NavTransport::new(nav_endpoint)
        .context("build NAV transport for Layer-2 queryInvoiceCheck (drain-pending-retries)")?;

    let request_xml = query_invoice_check::build_request(
        credentials,
        tax_number_8,
        nav_invoice_number,
        InvoiceDirection::Outbound,
    )
    .map_err(|e: NavTransportError| {
        anyhow!("query_invoice_check::build_request (envelope construction; drain-pending-retries) failed: {e}")
    })?;

    let wire_result = runtime.block_on(query_invoice_check::send_built_request(
        &transport,
        &request_xml,
    ));

    let (decision, payload) = match wire_result {
        Ok(send_outcome) => {
            let outcome_enum =
                QueryInvoiceCheckOutcome::from_check_result(send_outcome.check_result);
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

    // TX0 — write the InvoiceCheckPerformed entry through the shared
    // writer. The WriteGuard's drop hook runs the lockstep mirror sync
    // (ADR-0098 1b), so no separate Ledger::open + sync_mirror is needed.
    // The NAV wire call above ran WITHOUT holding the writer mutex; the
    // guard is taken only for the audit append.
    {
        let mut conn = db.write().context(
            "shared writer: drain-pending-retries TX0 InvoiceCheckPerformed audit (ADR-0098 C2)",
        )?;
        write_check_performed_audit(&mut conn, ledger_meta, actor, idempotency_key, payload)?;
        // WriteGuard drop -> lockstep sync_mirror.
    }

    Ok(decision)
}

/// PR-19 / ADR-0032 §1: open transport, tokenExchange, build envelope.
/// Mirror of `retry_submission::prepare_for_attempt_audit`. S381/F1 —
/// the envelope `operation` is the ledger-derived one carried on
/// `PendingRetry::operation` (a stuck STORNO / MODIFY re-POSTs under its
/// own operation; NAV v3.0 bodies are byte-identical so the body cannot
/// recover it), NOT a hard-coded `Create`.
async fn prepare_for_attempt_audit(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    invoice_xml: &[u8],
    operation: InvoiceOperation,
) -> Result<PreparedSubmission, NavTransportError> {
    let transport = NavTransport::new(endpoint)?;
    let token = token_exchange::call(&transport, credentials, tax_number_8).await?;
    let request_xml = manage_invoice::build_request(
        credentials,
        tax_number_8,
        &token.decoded_token,
        &[ManageInvoiceItem {
            operation,
            invoice_data_xml: invoice_xml,
        }],
    )?;
    Ok(PreparedSubmission {
        transport,
        request_xml,
    })
}

/// Mirror of `retry_submission::PreparedSubmission`.
struct PreparedSubmission {
    transport: NavTransport,
    request_xml: Vec<u8>,
}

/// Translate a `NavTransportError` into the drain's fork choice.
/// Mirror of `drain_submission_queue::classify_nav_error`.
fn classify_nav_error(err: NavTransportError) -> DrainRetryError {
    let msg = format!("{err}");
    if submission_queue::is_transport_error(&err) {
        DrainRetryError::Transport(msg)
    } else {
        DrainRetryError::Application(msg)
    }
}

/// Layer-2 wire failure has already been formatted into a String by
/// `perform_layer_2_check`. The drain's fork choice still needs to
/// distinguish transport vs. application; since we no longer have
/// the typed `NavTransportError`, classify by substring on the
/// reqwest/tls/dns prefixes the typed-error `Display` impl emits.
///
/// Defaults to `Application` (continue the drain) on unrecognised
/// shapes — the safe direction per ADR-0031 §4 (continue on
/// misclassified-transport is per-invoice loud; halt on
/// misclassified-application is silently destructive of forward
/// progress).
fn classify_layer_2_failure(msg: &str) -> DrainRetryError {
    let lower = msg.to_ascii_lowercase();
    let is_transport = lower.contains("error sending request")
        || lower.contains("tls")
        || lower.contains("dns")
        || lower.contains("connection")
        || lower.contains("timed out")
        || lower.contains("http transport");
    if is_transport {
        DrainRetryError::Transport(msg.to_string())
    } else {
        DrainRetryError::Application(msg.to_string())
    }
}

// ──────────────────────────────────────────────────────────────────────
// Per-invoice helpers (mirrors of retry_submission)
// ──────────────────────────────────────────────────────────────────────

/// Scoped read tx → (invoice, idempotency_key). Mirror of
/// `retry_submission::load_issued_invoice`.
fn load_issued_invoice(
    conn: &mut Connection,
    invoice_id: &str,
) -> Result<(ReadyInvoice, IdempotencyKey)> {
    let tx = conn
        .transaction()
        .context("begin read transaction for drain-pending-retries invoice lookup")?;
    let pair = billing::load_ready_invoice_by_id(&tx, invoice_id)
        .context("billing::load_ready_invoice_by_id (drain-pending-retries)")?
        .ok_or_else(|| anyhow!("no issued invoice with id {invoice_id} in this tenant DB"))?;
    tx.commit().context("commit read transaction")?;
    Ok(pair)
}

/// TX1 audit-write — RetryRequested + Attempt in one tx. Mirror of
/// `retry_submission::write_retry_requested_and_attempt_audit`.
#[allow(clippy::too_many_arguments)]
fn write_retry_requested_and_attempt_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    reason: &str,
    request_xml: Vec<u8>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for drain-pending-retries TX1")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (drain-pending-retries TX1 RetryRequested+Attempt)")?;

    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();

    // State-2 Pending: prior_transaction_id is None per ADR-0032 §4
    // (no prior Response exists). The classifier filters only
    // state-2 invoices.
    let retry_payload = audit_payloads::InvoiceRetryRequestedPayload::new(
        &invoice_id_str,
        idempotency_key,
        None,
        None,
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
    .context("audit_ledger::append_in_tx InvoiceRetryRequested (drain-pending-retries TX1)")?;

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
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttempt (drain-pending-retries TX1)")?;

    tx.commit()
        .context("commit DuckDB transaction (drain-pending-retries TX1 RetryRequested+Attempt)")?;
    Ok(())
}

/// TX2 success audit-write. Mirror of
/// `retry_submission::write_response_audit`.
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
        .context("ensure audit-ledger schema for drain-pending-retries TX2 Response")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (drain-pending-retries TX2 Response audit append)")?;
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
    .context("audit_ledger::append_in_tx InvoiceSubmissionResponse (drain-pending-retries TX2)")?;
    tx.commit()
        .context("commit DuckDB transaction (drain-pending-retries TX2 Response audit append)")?;
    Ok(())
}

/// TX2 failure audit-write. Mirror of
/// `retry_submission::write_attempt_failed_audit`.
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
        .context("ensure audit-ledger schema for drain-pending-retries TX2 AttemptFailed")?;
    let tx = conn.transaction().context(
        "begin DuckDB transaction (drain-pending-retries TX2 AttemptFailed audit append)",
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
    .context(
        "audit_ledger::append_in_tx InvoiceSubmissionAttemptFailed (drain-pending-retries TX2)",
    )?;
    tx.commit().context(
        "commit DuckDB transaction (drain-pending-retries TX2 AttemptFailed audit append)",
    )?;
    Ok(())
}

/// TX0 audit-write (Layer-2 InvoiceCheckPerformed). Mirror of
/// `retry_submission::write_check_performed_audit`.
fn write_check_performed_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    idempotency_key: IdempotencyKey,
    payload: audit_payloads::InvoiceCheckPerformedPayload,
) -> Result<()> {
    audit_ledger::ensure_schema(conn).context(
        "ensure audit-ledger schema for drain-pending-retries TX0 InvoiceCheckPerformed",
    )?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (drain-pending-retries TX0 InvoiceCheckPerformed)")?;
    let idem_str = idempotency_key.to_canonical_string();
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceCheckPerformed,
        payload.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceCheckPerformed (drain-pending-retries TX0)")?;
    tx.commit()
        .context("commit DuckDB transaction (drain-pending-retries TX0 InvoiceCheckPerformed)")?;
    Ok(())
}

/// S184/S190 — resolve the NAV-facing invoice number for the Layer-2
/// `queryInvoiceCheck`: the byte-exact `<invoiceNumber>` NAV holds on
/// file, read off the invoice's on-disk InvoiceData XML. NEVER
/// re-synthesised from the billing series code + sequence — the legacy
/// `INV-default` series literal is not the number NAV was given
/// (`ABERP/2026/00009`), so a synthesised number makes
/// `queryInvoiceCheck` ask NAV about a number it never saw ⇒ always
/// Absent ⇒ the Layer-2 duplicate guard never fires (the live prod
/// incident). Named seam (mirror of
/// `retry_submission::nav_query_invoice_number`) so the equality pin
/// below can prove it byte-for-byte.
fn nav_query_invoice_number(invoice_xml_path: &std::path::Path) -> Result<String> {
    crate::nav_xml::read_invoice_number_from_xml(invoice_xml_path).with_context(|| {
        format!(
            "read the NAV-facing <invoiceNumber> from the on-disk InvoiceData XML at {} \
             for the drain-pending-retries Layer-2 queryInvoiceCheck (S184/S190)",
            invoice_xml_path.display()
        )
    })
}

/// 8-digit base of a Hungarian tax number. Mirror of
/// `retry_submission::parse_tax_number_8` / every other operator-
/// facing-twin variant.
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

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test unique on-disk path under the system temp root — avoids
    /// the `tempfile` dev-dep (CLAUDE.md #2).
    fn unique_temp_xml(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("aberp-drain-{tag}-{pid}-{nanos}-{seq}.xml"))
    }

    fn unique_temp_db(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("aberp-drain-{tag}-{pid}-{nanos}-{seq}.duckdb"))
    }

    /// OPENER PIN (DEFECT 3). The drain's post-commit chain verification
    /// reuses the ONE shared `aberp_db::Handle` (a coherent read clone via
    /// `Ledger::from_connection`) and never opens a second DuckDB instance
    /// on the live file. Seed a HEAVY on-disk ledger — the depth that
    /// historically tripped the re-open checkpoint/ART replay crash
    /// (duckdb#23046 / S332) — open ONE Handle, and assert
    /// `verify_chain_reusing_read` verifies the full chain through it. The
    /// pre-fix drain re-opened an independent `Ledger::open` per phase (up
    /// to a second live instance while a `&mut Connection` was still
    /// alive); this pins the migration. CHECK 10M-b independently proves
    /// the drain's write-fork residual stays ZERO. Bundled-DuckDB (CI).
    #[test]
    fn verify_chain_reusing_read_reuses_one_handle_on_heavy_ledger() {
        use aberp_audit_ledger::BinaryHash;

        let db = unique_temp_db("reuse");
        let tenant = TenantId::new("t-drain-reuse".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([9u8; 32]);
        let actor = Actor::from_local_cli("sess-reuse".to_string(), "test-user");

        const N: usize = 64;
        {
            let mut ledger = Ledger::open(&db, tenant.clone(), bh).expect("open ledger to seed");
            for i in 0..N {
                let payload = serde_json::to_vec(&serde_json::json!({ "n": i })).unwrap();
                ledger
                    .append(
                        EventKind::InvoiceSubmissionAttempt,
                        payload,
                        actor.clone(),
                        None,
                    )
                    .expect("append seed entry");
            }
        } // ledger drops -> its Connection closes, entries committed.

        // ONE shared Handle; the verify path must reuse it, not re-open.
        let handle =
            aberp_db::Handle::open_default(&db, tenant.clone()).expect("open shared handle");
        let verified =
            verify_chain_reusing_read(&handle, tenant, bh).expect("verify via reused shared read");
        assert_eq!(verified, N as u64, "all seeded entries must verify");

        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(audit_ledger::mirror_path_for(&db));
    }

    /// EQUALITY PIN (DEFECT 1). The number the drain places into the
    /// Layer-2 `<invoiceNumberQuery><invoiceNumber>` MUST equal the
    /// on-disk XML `<invoiceNumber>`, never the synthesised
    /// `"{series.code}/{seq:05}"` (`INV-default/00009`) NAV never saw.
    /// RED before the fix, GREEN after.
    #[test]
    fn layer2_query_number_equals_on_disk_xml_number() {
        let path = unique_temp_xml("eq");
        std::fs::write(
            &path,
            b"<InvoiceData xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/data\">\
              <invoiceNumber>ABERP/2026/00009</invoiceNumber>\
              <invoiceIssueDate>2026-07-09</invoiceIssueDate></InvoiceData>",
        )
        .unwrap();
        let placed = nav_query_invoice_number(&path).unwrap();
        assert_eq!(placed, "ABERP/2026/00009");
        assert_ne!(
            placed,
            format!("{}/{:05}", "INV-default", 9u64),
            "the drain's Layer-2 query number MUST be the on-disk XML number, \
             not the synthesised series literal (Defect 1)"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// OPERATION PIN (DEFECT 2). A stuck STORNO must re-POST as a STORNO,
    /// not a fresh CREATE. The classifier stamps `PendingRetry.operation`
    /// from the ledger's chain-link entries; `prepare_for_attempt_audit`
    /// then carries it onto the envelope. Pre-fix `PendingRetry` had no
    /// operation and the drain hard-coded `Create`, so a stuck STORNO
    /// would have been re-submitted as a duplicate CREATE. RED before the
    /// fix (no operation field / hard-coded Create), GREEN after.
    #[test]
    fn stuck_storno_carries_storno_operation_not_create() {
        use aberp_audit_ledger::{Actor, BinaryHash};
        use aberp_billing::IdempotencyKey;

        let tenant = TenantId::new("t-op".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([3u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess-op".to_string(), "test-user");
        let idem = IdempotencyKey::new();

        // A storno child invoice, drafted + attempted (state-2 Pending),
        // with an InvoiceStornoIssued chain-link naming it the storno.
        let draft = audit_payloads::InvoiceDraftCreatedPayload {
            invoice_id: "inv_STORNO".to_string(),
            line_count: 1,
            idempotency_key: idem.to_canonical_string(),
            nav_xml_path: Some("/x/storno.xml".to_string()),
            currency: None,
            exchange_rate: None,
            exchange_rate_source: None,
            exchange_rate_date: None,
            huf_equivalent_total: None,
            bank_account_id: None,
            bank_account_currency: None,
            bank_account_number: None,
            bank_account_bank_name: None,
            bank_account_swift_bic: None,
            invoice_note: None,
            line_notes: Vec::new(),
            payment_deadline: None,
            delivery_date: None,
            delivery_date_override: None,
            customer_vat_status: None,
        };
        ledger
            .append(
                EventKind::InvoiceDraftCreated,
                draft.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let storno = audit_payloads::InvoiceStornoIssuedPayload::new(
            "inv_STORNO",
            7,
            "rsv_storno",
            idem,
            "inv_BASE",
            42,
            1,
        );
        ledger
            .append(
                EventKind::InvoiceStornoIssued,
                storno.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let attempt = audit_payloads::InvoiceSubmissionAttemptPayload::new(
            "inv_STORNO",
            idem,
            "test",
            b"<ManageInvoiceRequest/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                attempt.to_bytes(),
                actor,
                Some(idem.to_canonical_string()),
            )
            .unwrap();

        let pending = submission_queue::pending_retries_from_ledger(&ledger).unwrap();
        let retry = pending
            .iter()
            .find(|r| r.invoice_id == "inv_STORNO")
            .expect("the stuck storno child must be a pending retry");
        assert_eq!(
            retry.operation,
            InvoiceOperation::Storno,
            "a stuck STORNO must re-POST under Storno, not a fresh Create (Defect 2)"
        );
    }

    /// AUTO_REASON is non-empty and names the F45 closure / ADR
    /// reference so an audit-evidence-bundle reader sees the
    /// provenance of the automatic retry without consulting the CLI
    /// history. Pin per CLAUDE.md rule 9: a refactor that blanks the
    /// reason would silently break the audit-evidence requirement
    /// ADR-0009 §5 names ("operator decision must carry a human-
    /// readable justification").
    #[test]
    fn auto_reason_is_non_empty_and_self_describing() {
        assert!(!AUTO_REASON.is_empty());
        assert!(AUTO_REASON.contains("drain-pending-retries"));
        assert!(AUTO_REASON.contains("F45"));
    }

    /// Tax-number parser mirrors the other operator-facing-twin
    /// shape. Same contract as
    /// `submit_invoice::parse_tax_number_8` /
    /// `retry_submission::parse_tax_number_8` /
    /// `drain_submission_queue::parse_tax_number_8` /
    /// `poll_ack::parse_tax_number_8`.
    #[test]
    fn tax_number_8_parses_same_as_submit_invoice() {
        assert_eq!(parse_tax_number_8("12345678").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1-42").unwrap(), "12345678");
        assert!(parse_tax_number_8("1234567").is_err());
        assert!(parse_tax_number_8("1234567X").is_err());
        assert!(parse_tax_number_8("123456789-1-42").is_err());
    }

    /// `classify_layer_2_failure` routes transport-shaped messages to
    /// `Transport` (stop the drain) and everything else to
    /// `Application` (continue). The default-application direction is
    /// the safe one per ADR-0031 §4 — misclassified transport keeps
    /// the loud per-invoice surface; misclassified application would
    /// silently halt forward progress.
    #[test]
    fn classify_layer_2_failure_routes_transport_phrases_to_transport() {
        assert!(matches!(
            classify_layer_2_failure("error sending request for url (https://...)"),
            DrainRetryError::Transport(_)
        ));
        assert!(matches!(
            classify_layer_2_failure("dns error: no record"),
            DrainRetryError::Transport(_)
        ));
        assert!(matches!(
            classify_layer_2_failure("TLS handshake failed"),
            DrainRetryError::Transport(_)
        ));
        assert!(matches!(
            classify_layer_2_failure("connection refused"),
            DrainRetryError::Transport(_)
        ));
        assert!(matches!(
            classify_layer_2_failure("operation timed out"),
            DrainRetryError::Transport(_)
        ));
    }

    /// Non-transport messages route to Application (continue the
    /// drain). Pins the default-direction per ADR-0031 §4.
    #[test]
    fn classify_layer_2_failure_routes_application_phrases_to_application() {
        assert!(matches!(
            classify_layer_2_failure("INVALID_SECURITY_USER"),
            DrainRetryError::Application(_)
        ));
        assert!(matches!(
            classify_layer_2_failure("HTTP 500 from NAV"),
            DrainRetryError::Application(_)
        ));
        assert!(matches!(
            classify_layer_2_failure("missing <invoiceCheckResult>"),
            DrainRetryError::Application(_)
        ));
    }
}
