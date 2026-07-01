//! Orchestration for the `aberp poll-ack` subcommand (PR-7-C-2).
//!
//! Drives the bounded poll loop that turns a `SubmittedInvoice` into one
//! of three terminal typestates per ADR-0009 §2 + §5:
//!
//!   - `SAVED`   → `FinalizedInvoice`         (terminal-positive)
//!   - `ABORTED` → `RejectedInvoice`          (terminal-negative)
//!   - exhausted → `SubmissionStuckInvoice`   (bounded retries hit)
//!
//! The poll loop's per-attempt audit-ledger emission is the load-bearing
//! evidence for the audit-evidence bundle (ADR-0009 §8 — "every
//! `queryTransactionStatus` response across the chain").
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args (8-digit tax number; tenant; endpoint).
//!   2. Load `NavCredentials` from the OS keychain (loud-fail on missing).
//!   3. Open the tenant DuckDB; look up the previously-issued invoice
//!      AND its persisted idempotency_key from the billing store
//!      (scoped read tx; same shape as `submit_invoice::run`).
//!   4. Read the audit ledger via a fresh `Ledger::open` and look up
//!      the most-recent `InvoiceSubmissionResponse` entry for this
//!      invoice id — its payload carries the NAV `transaction_id`.
//!      Loud-fail if no such entry exists (operator tried to poll an
//!      invoice that was never submitted).
//!   5. Build a tokio current-thread runtime and drive the bounded poll
//!      loop on it. `queryTransactionStatus` is a NAV *query* operation
//!      per ADR-0009 §4 — it authenticates via the per-request `<user>`
//!      block (passwordHash + requestSignature) and does NOT consume an
//!      `exchangeToken`, so each poll attempt is ONE HTTP call, not two.
//!      Per attempt the loop calls `queryTransactionStatus` against the
//!      persisted transactionId, writes one `InvoiceAckStatus` audit
//!      entry under its own DuckDB tx (per-poll commit so a process
//!      crash mid-loop still leaves every completed poll's evidence
//!      behind), and decides: terminal status (`SAVED` / `ABORTED`)
//!      breaks the loop; intermediate (`RECEIVED` / `PROCESSING`)
//!      sleeps the backoff and continues; non-retryable error breaks
//!      with `Stuck`; retryable error sleeps the backoff and
//!      continues. Loop exit without a terminal status is `Stuck`.
//!   6. Verify the audit chain after the loop. Loud-fail if the chain
//!      did not verify (success-criterion gate per ADR-0008).
//!   7. Advance the typestate and print the operator-visible summary.
//!
//! # Why per-poll commit, not one-tx-at-end
//!
//! `submit_invoice::run` writes both submission entries in one tx
//! because they are siblings in time (attempt + response, milliseconds
//! apart) and the operator-visible failure mode is "we tried but did
//! not succeed." The poll loop's entries span up to 31 seconds; a
//! single-tx-at-end posture would lose every completed poll's audit
//! evidence on a process crash at second 28. Per-poll commit matches
//! ADR-0009 §8's "every response across the chain" intent.
//!
//! # Why look up transactionId from the audit ledger
//!
//! Per the PR-7-B-3 design assumption A5/A6 (recorded in
//! `_handoffs/09-session-8-close.md`): the `submission_state` fact
//! lives in the audit ledger, NOT in a billing column. The submission
//! response's `transaction_id` is the source of truth; reading it from
//! the audit ledger keeps the ledger as the single canonical place and
//! avoids a second column that could drift.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT re-submit the invoice. The retry-on-submit path
//!     would land as part of a separate `RetrySubmission` command per
//!     ADR-0009 §5 ("the operator unblocks via a typed RetrySubmission
//!     or MarkSubmissionAbandoned").
//!   - It does NOT mutate any billing row — the terminal-state fact
//!     remains in the audit ledger per A5/A6.
//!   - It does NOT call `queryInvoiceCheck` (Layer-2 idempotency per
//!     ADR-0009 §5); that path lands when the crash-between-submit-
//!     and-ack disambiguation case surfaces.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aberp_audit_ledger::{
    self as audit_ledger, Actor, BinaryHash, Entry, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_billing::{self as billing, IdempotencyKey, ReadyInvoice};
use aberp_nav_transport::{
    operations::query_transaction_status::{self, ProcessingStatus, QueryTransactionStatusOutcome},
    NavCredentials, NavEndpoint, NavTransport, NavTransportError,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::{NavEnv, PollAckArgs};

/// ADR-0009 §5: "Max attempts: 5, exponential backoff (1s, 2s, 4s, 8s, 16s)."
/// The cap is hard-coded rather than CLI-configurable because the retry
/// policy is a regulator-facing invariant — the operator should not be
/// able to silently extend it. A future operator-tunable knob would
/// require its own command + audit entry per ADR-0007 §"Operator-as-
/// threat-actor controls."
const MAX_POLL_ATTEMPTS: u32 = 5;

/// ADR-0009 §5 backoff base. `1s, 2s, 4s, 8s, 16s` is `1000ms << (n-1)`
/// for `n` in `1..=5`. Hard-coded for the same regulator-facing-invariant
/// reason as the attempt cap.
const BACKOFF_BASE_MILLIS: u64 = 1_000;

/// Error-side outcome from one attempt. The Ok side of an attempt is
/// the typed [`QueryTransactionStatusOutcome`] returned by the operation
/// itself; this enum is the classified-error path so the loop body can
/// match on retry vs stick without re-parsing the underlying
/// `NavTransportError`.
///
/// The diagnostic strings are captured for the loop's tracing event
/// but are NOT written into the audit ledger (no response_xml to emit
/// per the PR-7-C-1 trade-off — see `query_transaction_status.rs`'s
/// module header).
#[derive(Debug)]
enum AttemptError {
    /// Retryable error (transport-layer or NAV `Retryable`); the loop
    /// should back off and try again if attempts remain.
    Retryable(String),
    /// Non-retryable error; the loop must break with `Stuck`.
    NonRetryable(String),
}

/// Where the loop ended up after consuming all attempts (or breaking
/// early). Drives the typestate transition + the printed summary.
#[derive(Debug)]
enum LoopTerminus {
    /// One of the four NAV ack values that ends a poll loop with
    /// real evidence. `Saved` and `Aborted` are the ADR-0009 §2
    /// terminal-positive / terminal-negative outcomes; `Received`
    /// and `Processing` reach this enum only on attempts-exhausted
    /// (the last poll still returned an intermediate status).
    LastStatus(ProcessingStatus),
    /// Bounded retries exhausted with only error responses (no
    /// successful poll returned a parsed status). Carries the last
    /// diagnostic for the operator-visible summary.
    AllAttemptsErrored(String),
    /// NAV returned a non-retryable error. Carries the diagnostic.
    NonRetryableError(String),
}

pub fn run(args: &PollAckArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "poll_ack",
        invoice_id = %args.invoice_id,
        tenant = %args.tenant,
        endpoint = ?args.endpoint,
    )
    .entered();

    // PR-44η / session-60 — thin wrapper over [`poll_ack_from_inputs`].
    // The CLI-specific responsibilities (load NAV credentials, mint
    // the `Actor`, print the operator-visible summary line) stay here;
    // the bounded-poll-loop + audit-write pipeline lives in the
    // library function so the new `POST /invoices/:id/poll-ack` route
    // (`serve.rs::poll_ack_request`) calls the same path.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for this CLI invocation"
    );

    let nav_endpoint = match args.endpoint {
        NavEnv::Test => NavEndpoint::Test,
        NavEnv::Production => NavEndpoint::Production,
    };

    // PR-56 / session-76 — build the tokio runtime at the CLI's
    // top-level so [`poll_ack_from_inputs`] can stay async-native.
    // Same posture as `submit_invoice::run`; see that module for the
    // nested-runtime-panic background.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for poll-ack CLI")?;
    // ADR-0098 C2 — the one-shot CLI path constructs its OWN shared Handle so
    // poll_ack_from_inputs routes DB access through a single instance uniformly
    // (the serve path passes state.db). A CLI process has no concurrent writer,
    // but this keeps the post-commit durability hook on this path too and lets
    // poll_ack_from_inputs take `&HandleArc` for both callers.
    let tenant_for_handle = TenantId::new(args.tenant.clone())
        .ok_or_else(|| anyhow!("tenant value '{}' is empty or has a null byte", args.tenant))?;
    let db_handle = aberp_db::Handle::open_default(&args.db, tenant_for_handle)
        .with_context(|| format!("open shared DuckDB handle at {}", args.db.display()))?;
    let outcome = runtime.block_on(poll_ack_from_inputs(
        &db_handle,
        &args.tenant,
        &args.invoice_id,
        &args.tax_number,
        nav_endpoint,
        &credentials,
        actor,
    ))?;

    // 7. Typestate advance + operator-visible summary.
    match &outcome.terminal {
        PollAckTerminal::Finalized => {
            println!(
                "ack-poll finalized invoice {} (seq {}) -> NAV SAVED \
                 transactionId {} (audit chain verified across {} entries)",
                outcome.invoice_id,
                outcome.sequence_number,
                outcome.transaction_id,
                outcome.entries_verified,
            );
        }
        PollAckTerminal::Rejected => {
            tracing::error!(
                invoice_id = %outcome.invoice_id,
                seq = outcome.sequence_number,
                transaction_id = %outcome.transaction_id,
                "NAV ABORTED: invoice rejected, sequence slot is used-with-reason; \
                 a corrective new invoice must be issued"
            );
            println!(
                "ack-poll REJECTED invoice {} (seq {}) -> NAV ABORTED \
                 transactionId {} (audit chain verified across {} entries) \
                 — sequence not reused; issue a corrective new invoice",
                outcome.invoice_id,
                outcome.sequence_number,
                outcome.transaction_id,
                outcome.entries_verified,
            );
        }
        PollAckTerminal::StuckIntermediate { last_status } => {
            tracing::error!(
                invoice_id = %outcome.invoice_id,
                seq = outcome.sequence_number,
                transaction_id = %outcome.transaction_id,
                last_status = %last_status,
                "poll-ack: attempts exhausted with intermediate status, invoice STUCK"
            );
            println!(
                "ack-poll STUCK invoice {} (seq {}) -> last status {} after {} attempts \
                 (audit chain verified across {} entries) — operator action required",
                outcome.invoice_id,
                outcome.sequence_number,
                last_status,
                MAX_POLL_ATTEMPTS,
                outcome.entries_verified,
            );
        }
        PollAckTerminal::StuckNonRetryable { diagnostic } => {
            tracing::error!(
                invoice_id = %outcome.invoice_id,
                seq = outcome.sequence_number,
                transaction_id = %outcome.transaction_id,
                "poll-ack: NAV non-retryable error during poll, invoice STUCK: {}",
                diagnostic,
            );
            println!(
                "ack-poll STUCK invoice {} (seq {}) -> NAV non-retryable error: {} \
                 (audit chain verified across {} entries) — operator action required",
                outcome.invoice_id, outcome.sequence_number, diagnostic, outcome.entries_verified,
            );
        }
        PollAckTerminal::StuckAllAttemptsErrored { diagnostic } => {
            tracing::error!(
                invoice_id = %outcome.invoice_id,
                seq = outcome.sequence_number,
                transaction_id = %outcome.transaction_id,
                "poll-ack: every attempt errored, invoice STUCK: {}",
                diagnostic,
            );
            println!(
                "ack-poll STUCK invoice {} (seq {}) -> all {} attempts errored, last: {} \
                 (audit chain verified across {} entries) — operator action required",
                outcome.invoice_id,
                outcome.sequence_number,
                MAX_POLL_ATTEMPTS,
                diagnostic,
                outcome.entries_verified,
            );
        }
    }

    Ok(())
}

/// PR-44η / session-60 — terminal classification of a completed poll
/// loop. Mirrors the per-arm operator-visible summary the CLI's
/// `run` prints, but exposed as a typed enum so the serve route can
/// surface the terminus on the wire response.
#[derive(Debug, Clone)]
pub enum PollAckTerminal {
    /// NAV ack: SAVED — invoice advanced to `FinalizedInvoice`.
    Finalized,
    /// NAV ack: ABORTED — invoice advanced to `RejectedInvoice`.
    Rejected,
    /// Bounded attempts exhausted with the last poll returning an
    /// intermediate status (`RECEIVED` / `PROCESSING`). The invoice
    /// is left in `SubmissionStuck` per ADR-0009 §5.
    StuckIntermediate { last_status: String },
    /// NAV returned a non-retryable error mid-loop; the loop broke
    /// early with `SubmissionStuck`.
    StuckNonRetryable { diagnostic: String },
    /// Every attempt errored (transient or retryable) without a
    /// terminal status; the loop exhausted with `SubmissionStuck`.
    StuckAllAttemptsErrored { diagnostic: String },
}

/// PR-44η / session-60 — successful poll-loop outcome returned by
/// [`poll_ack_from_inputs`]. Carries enough fact for both the CLI's
/// operator-visible summary AND the serve route's wire response.
#[derive(Debug)]
pub struct PollAckOutcome {
    pub invoice_id: String,
    pub sequence_number: u64,
    pub transaction_id: String,
    pub terminal: PollAckTerminal,
    pub attempts_made: u32,
    pub entries_verified: u64,
}

/// PR-44η / session-60 — library-callable poll-loop entry. Consumed
/// by [`run`] (the CLI path) AND by `serve::poll_ack_request` (the
/// loopback `POST /invoices/:id/poll-ack` route). Both surfaces share
/// one bounded-poll-loop + per-attempt audit pipeline.
///
/// Pipeline (steps map to the pre-PR-44η `run` numbering in this
/// module's doc comment):
///
///   1. Parse `tax_number_raw` to its 8-digit base; resolve `TenantId`.
///   3. Open DuckDB; load the invoice + its idempotency key.
///   4. Resolve the NAV `transactionId` from the most-recent
///      `InvoiceSubmissionResponse` audit entry.
///   5. Drive the bounded poll loop on the caller's tokio runtime
///      (CLI owns a current-thread runtime in `run`; the SPA route
///      handler is itself async per PR-56 / session-76); per attempt
///      writes one `InvoiceAckStatus` audit entry under its own
///      DuckDB tx.
///   6. Verify-chain + mirror-sync success-criterion gate.
///
/// Returns a typed [`PollAckOutcome`] so callers don't re-walk the
/// ledger to format their summary; the operator-visible eprintln /
/// JSON shape lives at the caller.
#[allow(clippy::too_many_arguments)]
pub async fn poll_ack_from_inputs(
    db: &aberp_db::HandleArc,
    tenant_str: &str,
    invoice_id_str: &str,
    tax_number_raw: &str,
    nav_endpoint: NavEndpoint,
    credentials: &NavCredentials,
    actor: Actor,
) -> Result<PollAckOutcome> {
    // 1. Parse + validate inputs.
    let tenant = TenantId::new(tenant_str.to_string())
        .ok_or_else(|| anyhow!("tenant value '{}' is empty or has a null byte", tenant_str))?;
    let tax_number_8 = parse_tax_number_8(tax_number_raw)?;

    // 3. Load the invoice + its idempotency key via a shared READ clone of the
    //    ONE instance (ADR-0098 C2). Scoped so the read clone drops before the
    //    poll loop's per-attempt writes; load_issued_invoice only reads.
    let (ready_invoice, idempotency_key) = {
        let mut conn = db
            .read()
            .context("shared read: load issued invoice for poll-ack (ADR-0098 Gap 1a C2)")?;
        load_issued_invoice(&mut conn, invoice_id_str)?
    };
    if ready_invoice.id.to_prefixed_string() != invoice_id_str {
        return Err(anyhow!(
            "loaded invoice id {} does not match requested {}",
            ready_invoice.id.to_prefixed_string(),
            invoice_id_str
        ));
    }
    tracing::info!(
        seq = ready_invoice.sequence_number,
        idempotency_key = %idempotency_key.to_canonical_string(),
        "issued invoice loaded for ack poll"
    );

    // 4. Look up the NAV transactionId from the audit ledger.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let transaction_id =
        lookup_transaction_id(db, tenant.clone(), binary_hash_bytes, invoice_id_str)?;
    tracing::info!(
        transaction_id = %transaction_id,
        "NAV transactionId resolved from audit-ledger submission_response"
    );

    let submitted_invoice = ready_invoice.into_submitted(transaction_id.clone());
    let submitted_invoice_id = submitted_invoice.id.to_prefixed_string();
    let sequence_number = submitted_invoice.sequence_number;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 5. Drive the poll loop on the caller's runtime. PR-56 /
    //    session-76 — pre-PR-56 this function built its own
    //    current-thread runtime and `block_on`'d `poll_loop` inline,
    //    which panicked when called from the axum handler's already-
    //    running multi-thread runtime. Both the CLI's
    //    `poll_ack::run` and the SPA's `POST /invoices/:id/poll-ack`
    //    handler now own the runtime above this call and `.await`
    //    here.
    let terminus = poll_loop(
        nav_endpoint,
        credentials,
        &tax_number_8,
        &transaction_id,
        &submitted_invoice_id,
        db,
        &ledger_meta,
        &actor,
    )
    .await?;

    // 6. Verify the audit chain (success-criterion gate). ADR-0098 C2 — via a
    //    shared READ clone (Ledger::from_connection); the mirror was already
    //    synced on each per-attempt WriteGuard drop in poll_loop. No independent
    //    Connection::open / Ledger::open re-open (the duckdb#23046 replay locus).
    let verify_conn = db
        .read()
        .context("shared read: verify chain after poll-ack (ADR-0098 C2)")?;
    let ledger = Ledger::from_connection(verify_conn, tenant, binary_hash_bytes);
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER poll-ack")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    let (terminal, attempts_made) = match terminus {
        LoopTerminus::LastStatus(ProcessingStatus::Saved) => {
            (PollAckTerminal::Finalized, MAX_POLL_ATTEMPTS)
        }
        LoopTerminus::LastStatus(ProcessingStatus::Aborted) => {
            (PollAckTerminal::Rejected, MAX_POLL_ATTEMPTS)
        }
        LoopTerminus::LastStatus(intermediate) => (
            PollAckTerminal::StuckIntermediate {
                last_status: intermediate.as_nav_str().to_string(),
            },
            MAX_POLL_ATTEMPTS,
        ),
        LoopTerminus::NonRetryableError(diagnostic) => {
            (PollAckTerminal::StuckNonRetryable { diagnostic }, 1)
        }
        LoopTerminus::AllAttemptsErrored(diagnostic) => (
            PollAckTerminal::StuckAllAttemptsErrored { diagnostic },
            MAX_POLL_ATTEMPTS,
        ),
    };

    Ok(PollAckOutcome {
        invoice_id: submitted_invoice_id,
        sequence_number,
        transaction_id,
        terminal,
        attempts_made,
        entries_verified: verified,
    })
}

/// Open a scoped read tx, look up the issued invoice, and return it
/// alongside its persisted idempotency key. Mirror of
/// `submit_invoice::load_issued_invoice` — same shape, same contract.
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

/// Read every audit-ledger entry, find the most-recent
/// `InvoiceSubmissionResponse` whose payload references `invoice_id`,
/// return its `transaction_id`. Loud-fail if no such entry exists.
///
/// "Most recent" = highest `seq` (the ledger orders by `seq`; `entries()`
/// returns them in seq order per `Ledger::entries` doc comment). If
/// multiple submissions occurred (rare; would require a resubmit which
/// PR-7-C doesn't do — but a future `RetrySubmission` command might),
/// the latest is the one to poll against.
fn lookup_transaction_id(
    db: &aberp_db::HandleArc,
    tenant: TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
    invoice_id: &str,
) -> Result<String> {
    // ADR-0098 C2 — read the ledger via a shared read clone (from_connection),
    // not an independent Ledger::open of the live path.
    let conn = db
        .read()
        .context("shared read: look up NAV transactionId (ADR-0098 Gap 1a C2)")?;
    let ledger = Ledger::from_connection(conn, tenant, binary_hash);
    let entries = ledger.entries().context("read audit ledger entries")?;
    let txid = entries
        .iter()
        .rev() // most-recent first
        .find_map(|entry| extract_transaction_id(entry, invoice_id))
        .ok_or_else(|| {
            anyhow!(
                "no InvoiceSubmissionResponse audit entry found for invoice {invoice_id} \
                 — did `aberp submit-invoice` complete for this id?"
            )
        })?;
    if txid.is_empty() {
        return Err(anyhow!(
            "InvoiceSubmissionResponse for invoice {invoice_id} has empty transaction_id"
        ));
    }
    Ok(txid)
}

/// Inspect one audit entry: if it is an `InvoiceSubmissionResponse`
/// whose payload's `invoice_id` matches the target, return its
/// `transaction_id`. Else `None`.
///
/// Typed payload decode per F9: the payload bytes go through
/// `serde_json::from_slice` on `InvoiceSubmissionResponsePayload`, not
/// ad-hoc string match. A schema mismatch produces a parse error which
/// we treat as "not the entry we want" (return None) — the caller's
/// loud-fail on "no entry found" still surfaces the real problem.
pub(crate) fn extract_transaction_id(entry: &Entry, invoice_id: &str) -> Option<String> {
    if entry.kind != EventKind::InvoiceSubmissionResponse {
        return None;
    }
    let parsed: audit_payloads::InvoiceSubmissionResponsePayload =
        serde_json::from_slice(&entry.payload).ok()?;
    if parsed.invoice_id == invoice_id {
        Some(parsed.transaction_id)
    } else {
        None
    }
}

/// Bounded poll loop. `queryTransactionStatus` is a NAV *query* operation
/// per ADR-0009 §4 — it authenticates with the per-request `<user>` block
/// (passwordHash + requestSignature) and does **not** consume an
/// `exchangeToken`. Only `manageInvoice` / `manageAnnulment` need the
/// token. The poll loop therefore makes one HTTP call per attempt, not
/// two — keeping the worst-case wall time inside the ADR-0009 §5 budget.
#[allow(clippy::too_many_arguments)]
async fn poll_loop(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    transaction_id: &str,
    invoice_id: &str,
    db: &aberp_db::HandleArc,
    ledger_meta: &LedgerMeta,
    actor: &Actor,
) -> Result<LoopTerminus> {
    let transport = NavTransport::new(endpoint).context("build NAV transport")?;
    // ADR-0098 C2 — the daemon holds no long-lived `conn`; the audit-ledger
    // schema is ensured inside each per-attempt `write_ack_audit_entry`
    // db.write() window (idempotent), so the prior up-front ensure_schema on a
    // threaded `conn` is removed.

    let mut last_error: Option<String> = None;
    let mut last_intermediate_status: Option<ProcessingStatus> = None;

    for attempt in 1..=MAX_POLL_ATTEMPTS {
        // PR-56 / session-76 — non-entered span: the prior
        // `.entered()` form produced a `!Send` `EnteredSpan` guard
        // that lived across the `.await`s below, which made the
        // outer `poll_ack_from_inputs` future `!Send` and failed
        // axum's `Handler` bound on the SPA-side `/poll-ack` route.
        // `Instrument` + `in_scope` preserve the per-attempt span
        // structure without holding a thread-local guard.
        use tracing::Instrument as _;
        let span = tracing::info_span!("poll_attempt", attempt);

        let outcome = run_one_attempt(&transport, credentials, tax_number_8, transaction_id)
            .instrument(span.clone())
            .await;

        let terminal = span.in_scope(|| -> Result<Option<LoopTerminus>> {
            match outcome {
                Ok(query_outcome) => {
                    let status = query_outcome.processing_status;
                    tracing::info!(
                        status = status.as_nav_str(),
                        response_bytes = query_outcome.response_xml.len(),
                        "queryTransactionStatus OK"
                    );

                    // Audit-write per poll. Each poll commits its own tx so
                    // a crash mid-loop preserves every completed poll's
                    // evidence.
                    write_ack_audit_entry(
                        db,
                        ledger_meta,
                        actor,
                        invoice_id,
                        transaction_id,
                        &query_outcome,
                    )?;

                    if status.is_terminal() {
                        return Ok(Some(LoopTerminus::LastStatus(status)));
                    }
                    last_intermediate_status = Some(status);
                    Ok(None)
                }
                Err(AttemptError::Retryable(diag)) => {
                    tracing::warn!(attempt, "queryTransactionStatus retryable error: {}", diag);
                    last_error = Some(diag);
                    Ok(None)
                }
                Err(AttemptError::NonRetryable(diag)) => {
                    tracing::error!(
                        attempt,
                        "queryTransactionStatus non-retryable error: {}",
                        diag
                    );
                    Ok(Some(LoopTerminus::NonRetryableError(diag)))
                }
            }
        })?;
        if let Some(t) = terminal {
            return Ok(t);
        }

        // Don't sleep after the last attempt; just exit the loop.
        if attempt < MAX_POLL_ATTEMPTS {
            let delay = Duration::from_millis(BACKOFF_BASE_MILLIS * (1u64 << (attempt - 1)));
            span.in_scope(|| {
                tracing::info!(
                    next_attempt = attempt + 1,
                    backoff_millis = delay.as_millis() as u64,
                    "backing off before next poll attempt"
                );
            });
            tokio::time::sleep(delay).instrument(span).await;
        }
    }

    // Loop ended without a terminal status. Either we have a last
    // intermediate (preferred — it's a real NAV-observed state) or only
    // errors (carry the last diagnostic).
    Ok(match (last_intermediate_status, last_error) {
        (Some(status), _) => LoopTerminus::LastStatus(status),
        (None, Some(diag)) => LoopTerminus::AllAttemptsErrored(diag),
        // Loop ran but produced neither — this means a future refactor
        // dropped both arms; surface loud.
        (None, None) => LoopTerminus::AllAttemptsErrored(
            "internal: poll loop exhausted attempts with neither status nor error captured"
                .to_string(),
        ),
    })
}

/// One attempt: a single `queryTransactionStatus` call. On NAV-side
/// success returns the typed outcome; on any error, maps it to the
/// internal [`AttemptError`] classification so the loop body can match
/// on retry vs stick.
async fn run_one_attempt(
    transport: &NavTransport,
    credentials: &NavCredentials,
    tax_number_8: &str,
    transaction_id: &str,
) -> Result<QueryTransactionStatusOutcome, AttemptError> {
    match query_transaction_status::call(transport, credentials, tax_number_8, transaction_id).await
    {
        Ok(outcome) => Ok(outcome),
        Err(NavTransportError::QueryTransactionStatusNonRetryable { code, message }) => {
            Err(AttemptError::NonRetryable(format!("{code}: {message}")))
        }
        Err(NavTransportError::QueryTransactionStatusRetryable { code, message }) => {
            Err(AttemptError::Retryable(format!("{code}: {message}")))
        }
        Err(NavTransportError::QueryTransactionStatusHttp(e)) => {
            Err(AttemptError::Retryable(format!("transport: {e}")))
        }
        Err(NavTransportError::QueryTransactionStatusHttpStatus { status }) => {
            Err(AttemptError::Retryable(format!("HTTP {status}")))
        }
        Err(NavTransportError::QueryTransactionStatusResponseParse(msg)) => {
            // PR-9-0 / ADR-0022 graduation: the XSD validator is now
            // landed on the issuance + submit + retry paths. With our
            // own emitted bytes structurally checked, a parse failure
            // on NAV's response side means NAV is sending us a shape
            // we cannot parse — that is schema drift on NAV's side,
            // NOT a transient transport blip. Retrying does not help
            // (the next response will have the same shape). Loud-fail
            // per CLAUDE.md rule 12: stick the invoice, surface to the
            // operator, who will trigger the release-level
            // schema-allowlist update or a hot-fix.
            //
            // Pre-PR-9-0 this arm returned `Retryable` per the
            // ADR-0021 §"Items deferred" note. The note's named
            // trigger has now fired (the validator landed). If the
            // validator is ever removed, this arm must be re-flipped
            // back to `Retryable` and the ADR-0022 supersede path
            // exercised.
            Err(AttemptError::NonRetryable(format!("parse: {msg}")))
        }
        Err(other) => {
            // Any other NavTransportError shape — e.g., a credential or
            // SOAP envelope path that should not fire mid-loop (the
            // envelope is rebuilt every attempt; if it ever fails at
            // attempt N>1, treat as retryable rather than infinitely
            // looping on the same boundary).
            Err(AttemptError::Retryable(format!("nav-transport: {other}")))
        }
    }
}

/// Open a fresh per-poll DuckDB tx, append one `InvoiceAckStatus` entry
/// carrying the verbatim NAV response_xml + parsed ack_status, commit.
fn write_ack_audit_entry(
    db: &aberp_db::HandleArc,
    ledger_meta: &LedgerMeta,
    actor: &Actor,
    invoice_id: &str,
    transaction_id: &str,
    outcome: &QueryTransactionStatusOutcome,
) -> Result<()> {
    // ADR-0098 C2 — acquire the shared writer for this single per-poll audit tx,
    // then drop it (post-commit hook runs the lockstep sync_mirror). Tight
    // window: the call sites run this inside a synchronous `span.in_scope` /
    // terminal write, so the !Send WriteGuard never crosses the NAV await.
    let mut conn = db
        .write()
        .context("shared writer: poll-ack InvoiceAckStatus (ADR-0098 Gap 1a C2)")?;
    audit_ledger::ensure_schema(&conn).context("ensure audit-ledger schema for poll-ack")?;
    let tx = conn
        .transaction()
        .context("begin per-poll DuckDB transaction (InvoiceAckStatus append)")?;

    let payload = audit_payloads::InvoiceAckStatusPayload::new(
        invoice_id,
        transaction_id,
        outcome.processing_status.as_nav_str(),
        outcome.response_xml.clone(),
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceAckStatus,
        payload.to_bytes(),
        actor.clone(),
        None, // poll entries do not share the issuance idempotency
              // key; the InvoiceSubmissionResponse entry already
              // anchors the chain to that key. Per ADR-0009 §8 the
              // audit-evidence bundle reconstructs by invoice_id +
              // transaction_id, not by walking idempotency_key for
              // poll entries.
    )
    .context("audit_ledger::append_in_tx InvoiceAckStatus")?;

    tx.commit()
        .context("commit per-poll DuckDB transaction (InvoiceAckStatus append)")?;
    Ok(())
}

// ── Session-161 — NAV poll-as-daemon ─────────────────────────────────
//
// The bounded `poll_loop` above (ADR-0009 §5, 5 attempts / ~15s) is the
// OPERATOR-triggered path: the CLI `poll-ack` subcommand and the SPA's
// manual "Poll" button (`POST /invoices/:id/poll-ack`). It writes one
// `InvoiceAckStatus` per attempt — bounded evidence, no bloat risk.
//
// The AUTOMATIC path (post-issue tail + app-boot recovery) instead runs
// the indefinite two-phase daemon below. It must survive long-tail NAV
// processing (minutes → hours) without the operator clicking anything,
// so it polls until terminal or process shutdown. To keep the audit
// ledger from growing 1440 rows/day per long-stuck invoice, the daemon
// writes a SINGLE terminal `InvoiceAckStatus` on resolution — never the
// intermediate RECEIVED/PROCESSING beats. Intermediate state is implicit:
// the SPA renders `Submitted ⌛` until a terminal row appears and flips it
// to ✓ Final / ⚠ Rejected. (There is no `NavPollStopped`/`Stuck` audit
// variant in this codebase — the "stuck" pictogram is purely the
// actionable `Submitted` SPA state. The S161 brief's Part E was written
// against a variant that does not exist; nothing is deprecated.)

/// Phase-1 backoff schedule (seconds). Fast exponential catch for the
/// common case where NAV resolves within ~2 minutes. After the last
/// delay the daemon switches to the steady phase-2 cadence.
const PHASE1_DELAYS_SECS: [u64; 7] = [1, 2, 4, 8, 16, 30, 60];

/// Phase-2 steady daemon cadence (seconds). 1-minute interval per
/// invoice; well inside NAV's published rate limits even with the
/// [`Semaphore`] cap saturated (≤ cap polls/min total).
const PHASE2_INTERVAL_SECS: u64 = 60;

/// Consecutive transient-error cap before the daemon gives up. A
/// persistent network/NAV outage should not spin a zombie task forever;
/// after this many *consecutive* retryable errors the daemon exits and
/// leaves the invoice actionable for a manual poll. Reset to zero by any
/// successful (non-terminal) poll, so an intermittent network never
/// trips it.
const MAX_CONSECUTIVE_DAEMON_ERRORS: u32 = 10;

/// Everything the [`run_nav_poll_daemon`] needs to drive an invoice's
/// indefinite poll to terminal. Grouped into one struct so the daemon
/// stays decoupled from `serve::AppState` (the spawn sites in
/// `serve.rs` build this from the ledger + keychain) and so the spawn
/// call reads clean.
pub struct PollDaemonInputs {
    /// ADR-0098 C2 (Gap 1a) — the shared process-wide DuckDB Handle.
    pub db: aberp_db::HandleArc,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub invoice_id: String,
    pub transaction_id: String,
    pub tax_number_8: String,
    pub endpoint: NavEndpoint,
    pub credentials: NavCredentials,
    pub actor: Actor,
}

/// How the two-phase poll schedule ended. `Terminal` carries the NAV
/// outcome whose `response_xml` becomes the single audit entry the
/// daemon writes; `GaveUp` carries the operator-visible reason.
/// `Cancelled` is the PR-209 / S213 shape — the
/// [`ShutdownCoordinator`](crate::shutdown::ShutdownCoordinator)
/// fired its token between polls so the daemon exits without writing
/// a terminal audit row. The invoice stays in its current state and
/// the next app-boot recovery pass re-spawns the daemon to keep
/// chasing the NAV ack.
#[derive(Debug)]
enum PollScheduleResult {
    Terminal {
        outcome: QueryTransactionStatusOutcome,
        polls: u64,
    },
    GaveUp {
        reason: String,
        polls: u64,
    },
    Cancelled {
        polls: u64,
    },
}

/// The two-phase poll schedule core. Generic over an async poll closure
/// so the timer pins (`#[tokio::test(start_paused = true)]`) can inject
/// a scripted NAV client without a live transport — the closure is the
/// only seam that touches NAV. Runs phase-1 exponential backoff, then an
/// unbounded phase-2 1-minute loop, until a terminal status, a
/// non-retryable error, or the consecutive-retryable-error cap.
///
/// Both phases treat a *retryable* error as "transient — keep polling"
/// (bounded by [`MAX_CONSECUTIVE_DAEMON_ERRORS`]). This is deliberately
/// MORE tolerant than the bounded `poll_loop`, whose attempt cap means a
/// transient blip can exhaust it; the daemon has no cap to exhaust, so a
/// network hiccup must not end it. A *non-retryable* error (e.g. NAV
/// schema drift surfaced as a parse failure) still ends the daemon
/// immediately — retrying cannot help, and the invoice stays actionable.
async fn drive_poll_schedule<F, Fut>(
    mut poll_once: F,
    cancel: CancellationToken,
) -> PollScheduleResult
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<QueryTransactionStatusOutcome, AttemptError>>,
{
    let mut consecutive_errors: u32 = 0;
    let mut polls: u64 = 0;

    // Classify one poll outcome into either an early return or "keep
    // going". Factored so phase-1 and phase-2 share identical handling
    // (and so the consecutive-error reset rule lives in exactly one
    // place — CLAUDE.md rule 8).
    macro_rules! handle {
        ($outcome:expr) => {{
            polls += 1;
            match $outcome {
                Ok(outcome) if outcome.processing_status.is_terminal() => {
                    return PollScheduleResult::Terminal { outcome, polls };
                }
                Ok(_) => {
                    consecutive_errors = 0;
                }
                Err(AttemptError::NonRetryable(diag)) => {
                    return PollScheduleResult::GaveUp {
                        reason: format!("non-retryable NAV error: {diag}"),
                        polls,
                    };
                }
                Err(AttemptError::Retryable(diag)) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_DAEMON_ERRORS {
                        return PollScheduleResult::GaveUp {
                            reason: format!(
                                "{MAX_CONSECUTIVE_DAEMON_ERRORS} consecutive retryable \
                                 errors; last: {diag}"
                            ),
                            polls,
                        };
                    }
                }
            }
        }};
    }

    // Phase 1 — fast exponential backoff. PR-209 / S213 — race each
    // sleep + each poll-attempt against the shutdown token so a
    // mid-backoff cancellation exits within the shutdown timeout
    // instead of waiting out the next 60s sleep. Cancellation during
    // an in-flight NAV call cancels the poll itself; the daemon
    // exits without writing a terminal row and the next app-boot
    // recovery pass re-spawns it.
    for delay in PHASE1_DELAYS_SECS {
        tokio::select! {
            _ = cancel.cancelled() => return PollScheduleResult::Cancelled { polls },
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
        }
        tokio::select! {
            _ = cancel.cancelled() => return PollScheduleResult::Cancelled { polls },
            outcome = poll_once() => handle!(outcome),
        }
    }

    // Phase 2 — steady daemon. No expiry: runs until terminal, a
    // non-retryable error, the error cap, or shutdown cancellation
    // (PR-209 / S213). Pre-PR-209 only "the task dies with the
    // process" stopped it — the process never died because the
    // daemon held it alive, exactly the bug PR-209 closes.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return PollScheduleResult::Cancelled { polls },
            _ = tokio::time::sleep(Duration::from_secs(PHASE2_INTERVAL_SECS)) => {}
        }
        tokio::select! {
            _ = cancel.cancelled() => return PollScheduleResult::Cancelled { polls },
            outcome = poll_once() => handle!(outcome),
        }
    }
}

/// Session-161 — spawn-friendly NAV poll daemon. Acquires a concurrency
/// permit (held for the daemon's whole lifetime — the cap bounds the
/// number of invoices being actively watched, not just instantaneous
/// HTTP calls), then drives [`drive_poll_schedule`] to terminal. On a
/// terminal status it writes ONE `InvoiceAckStatus` audit entry +
/// verifies the chain + syncs the mirror, exactly as the bounded loop's
/// final commit would. On give-up it writes nothing and logs loud — the
/// invoice stays `Submitted` (actionable) for a manual poll.
///
/// The future is `Send` (no held tracing span guard across `.await`) so
/// callers can `tokio::spawn` it directly.
pub async fn run_nav_poll_daemon(
    inputs: PollDaemonInputs,
    semaphore: Arc<Semaphore>,
    cancel: CancellationToken,
) -> Result<()> {
    let _permit = semaphore
        .acquire()
        .await
        .context("acquire NAV poll daemon concurrency permit")?;

    let transport = NavTransport::new(inputs.endpoint).context("build NAV transport for daemon")?;

    let result = drive_poll_schedule(
        || {
            run_one_attempt(
                &transport,
                &inputs.credentials,
                &inputs.tax_number_8,
                &inputs.transaction_id,
            )
        },
        cancel,
    )
    .await;

    match result {
        PollScheduleResult::Terminal { outcome, polls } => {
            tracing::info!(
                invoice_id = %inputs.invoice_id,
                transaction_id = %inputs.transaction_id,
                polls,
                status = outcome.processing_status.as_nav_str(),
                "NAV poll daemon reached terminal status; writing terminal audit entry"
            );
            write_daemon_terminal_ack(&inputs, &outcome)
        }
        PollScheduleResult::GaveUp { reason, polls } => {
            tracing::warn!(
                invoice_id = %inputs.invoice_id,
                transaction_id = %inputs.transaction_id,
                polls,
                reason = %reason,
                "NAV poll daemon stopped without a terminal status; \
                 invoice stays Submitted/actionable for a manual poll"
            );
            Ok(())
        }
        PollScheduleResult::Cancelled { polls } => {
            // PR-209 / S213 — shutdown cancelled this daemon between
            // (or during) polls. The invoice stays in its current
            // state (Submitted or whatever the chain reports) and
            // the next `aberp serve` boot's recovery scan re-spawns
            // a fresh daemon for it via the same
            // `query_non_terminal_invoices` walk. No audit row —
            // mid-shutdown bookkeeping noise would force the
            // operator to read past the cancellation when
            // postmortem-ing the *real* terminal status that lands
            // after restart.
            tracing::info!(
                invoice_id = %inputs.invoice_id,
                transaction_id = %inputs.transaction_id,
                polls,
                "NAV poll daemon cancelled by shutdown; invoice stays current \
                 state, app-boot recovery will re-spawn next launch"
            );
            Ok(())
        }
    }
}

/// Write the daemon's single terminal `InvoiceAckStatus` entry, then
/// re-run the same verify-chain + mirror-sync success-criterion gate the
/// bounded `poll_ack_from_inputs` runs after its loop. Opens its own
/// DuckDB connection (the daemon owns no `conn`).
fn write_daemon_terminal_ack(
    inputs: &PollDaemonInputs,
    outcome: &QueryTransactionStatusOutcome,
) -> Result<()> {
    // ADR-0098 C2 — route the terminal audit append through the shared Handle.
    // write_ack_audit_entry acquires db.write() internally (tight window; the
    // post-commit hook syncs the mirror on drop). The post-commit verify_chain
    // is preserved via a shared read clone — no independent Connection::open /
    // Ledger::open re-open.
    let ledger_meta = LedgerMeta::new(inputs.tenant.clone(), inputs.binary_hash);
    write_ack_audit_entry(
        &inputs.db,
        &ledger_meta,
        &inputs.actor,
        &inputs.invoice_id,
        &inputs.transaction_id,
        outcome,
    )?;

    let verify_conn = inputs
        .db
        .read()
        .context("shared read: verify chain after daemon terminal write (ADR-0098 C2)")?;
    let ledger = Ledger::from_connection(verify_conn, inputs.tenant.clone(), inputs.binary_hash);
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER daemon terminal write")?;
    tracing::info!(
        invoice_id = %inputs.invoice_id,
        entries_verified = verified,
        "daemon terminal audit entry written + chain verified"
    );
    Ok(())
}

/// Extract the 8-digit base of a Hungarian tax number. Mirror of
/// `submit_invoice::parse_tax_number_8` — same parser, same loud-fail
/// shapes. Duplicated here rather than re-exporting because the two
/// orchestration modules are operator-facing twins; future divergence
/// (e.g., a poll-only tenant validation step) would be invisible if
/// they shared a helper.
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
    use aberp_billing::IdempotencyKey;

    /// Sanity: the backoff schedule matches ADR-0009 §5 exactly. Pinned
    /// against a hand-computed table so a future "let's make it
    /// configurable" refactor cannot silently change the regulator-
    /// facing invariant.
    #[test]
    fn backoff_schedule_matches_adr_0009_section_5() {
        let table = [
            (1u32, 1_000u64),
            (2, 2_000),
            (3, 4_000),
            (4, 8_000),
            (5, 16_000),
        ];
        for (attempt, expected) in table {
            let delay = BACKOFF_BASE_MILLIS * (1u64 << (attempt - 1));
            assert_eq!(
                delay, expected,
                "attempt {attempt}: expected {expected}ms backoff, got {delay}ms"
            );
        }
        // Total wait if all four backoffs fire (between attempts 1+2,
        // 2+3, 3+4, 4+5): 1+2+4+8 = 15s. The 16s slot is NOT used
        // because the loop exits without sleeping after attempt 5.
        // Documented loud here so a future tweak to the loop's
        // sleep-after-last-attempt behaviour is obvious.
        let total_realized = 1_000 + 2_000 + 4_000 + 8_000;
        assert_eq!(total_realized, 15_000);
    }

    #[test]
    fn tax_number_8_parses_same_as_submit_invoice() {
        // Same contract as submit_invoice::parse_tax_number_8. If they
        // ever drift, the two orchestration modules will produce
        // confusingly different errors on the same operator input.
        assert_eq!(parse_tax_number_8("12345678").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1-42").unwrap(), "12345678");
        assert!(parse_tax_number_8("1234567").is_err());
        assert!(parse_tax_number_8("1234567X").is_err());
        assert!(parse_tax_number_8("123456789-1-42").is_err());
    }

    // ── Session-161 — two-phase poll daemon pins ─────────────────────
    //
    // These drive [`drive_poll_schedule`] under `start_paused = true`:
    // tokio auto-advances virtual time whenever the only runnable work
    // is a pending timer, so the indefinite phase-2 loop completes
    // deterministically in microseconds while the assertions still see
    // the real 1/2/4/8/16/30/60 + 60s schedule.

    use std::cell::RefCell;
    use std::rc::Rc;

    fn ok_outcome(status: ProcessingStatus) -> QueryTransactionStatusOutcome {
        QueryTransactionStatusOutcome {
            processing_status: status,
            request_xml: Vec::new(),
            response_xml: b"<r/>".to_vec(),
        }
    }

    /// Build a scripted poll closure + a virtual-time log. `script(n)`
    /// returns the result for the n-th (0-based) poll. The returned
    /// `Vec` (shared) records the virtual seconds-since-start at each
    /// poll so the schedule itself can be pinned.
    fn scripted<R>(
        script: R,
    ) -> (
        impl FnMut() -> std::pin::Pin<
            Box<dyn Future<Output = Result<QueryTransactionStatusOutcome, AttemptError>>>,
        >,
        Rc<RefCell<Vec<u64>>>,
    )
    where
        R: Fn(usize) -> Result<QueryTransactionStatusOutcome, AttemptError> + 'static,
    {
        let log: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));
        let count = Rc::new(RefCell::new(0usize));
        let start = tokio::time::Instant::now();
        let script = Rc::new(script);
        let out_log = log.clone();
        let closure = move || {
            let n = {
                let mut c = count.borrow_mut();
                let v = *c;
                *c += 1;
                v
            };
            log.borrow_mut().push(start.elapsed().as_secs());
            let result = (script)(n);
            Box::pin(async move { result }) as std::pin::Pin<Box<dyn Future<Output = _>>>
        };
        (closure, out_log)
    }

    /// Phase-1 backoff fires at exactly 1,2,4,8,16,30,60s cumulative
    /// (→ 1,3,7,15,31,61,121 elapsed), and a terminal status inside
    /// phase 1 stops the schedule there.
    #[tokio::test(start_paused = true)]
    async fn daemon_phase1_backoff_schedule_and_terminal() {
        // intermediate for the first 6 polls, SAVED on the 7th.
        let (poll, log) = scripted(|n| {
            if n >= 6 {
                Ok(ok_outcome(ProcessingStatus::Saved))
            } else {
                Ok(ok_outcome(ProcessingStatus::Processing))
            }
        });
        let result = drive_poll_schedule(poll, CancellationToken::new()).await;
        match result {
            PollScheduleResult::Terminal { outcome, polls } => {
                assert_eq!(polls, 7, "terminal SAVED on the 7th poll");
                assert_eq!(outcome.processing_status, ProcessingStatus::Saved);
            }
            other => panic!("expected Terminal, got {other:?}"),
        }
        assert_eq!(
            *log.borrow(),
            vec![1, 3, 7, 15, 31, 61, 121],
            "phase-1 cumulative elapsed must follow the 1,2,4,8,16,30,60 schedule"
        );
    }

    /// Phase 2 is an unbounded 1-minute loop: the daemon keeps polling
    /// well past phase 1's 7 attempts and only stops on terminal. Pins
    /// ≥10 phase-2 polls and the steady 60s cadence.
    #[tokio::test(start_paused = true)]
    async fn daemon_phase2_runs_indefinitely_until_terminal() {
        // SAVED only on the 18th poll → 7 phase-1 + 11 phase-2 polls.
        let (poll, log) = scripted(|n| {
            if n >= 17 {
                Ok(ok_outcome(ProcessingStatus::Saved))
            } else {
                Ok(ok_outcome(ProcessingStatus::Processing))
            }
        });
        let result = drive_poll_schedule(poll, CancellationToken::new()).await;
        match result {
            PollScheduleResult::Terminal { polls, .. } => {
                assert_eq!(polls, 18);
                assert!(polls - 7 >= 10, "at least 10 phase-2 polls before terminal");
            }
            other => panic!("expected Terminal, got {other:?}"),
        }
        // Phase-2 polls (index 7..18) are spaced exactly 60s apart, on
        // top of phase 1's last poll at 121s: 181, 241, 301, ...
        let log = log.borrow();
        for i in 7..log.len() {
            assert_eq!(
                log[i] - log[i - 1],
                PHASE2_INTERVAL_SECS,
                "phase-2 poll {i} must be 60s after the previous"
            );
        }
    }

    /// A burst of transient (retryable) errors below the consecutive cap
    /// must NOT kill the daemon — it keeps polling and still reaches
    /// terminal. The cap counter resets on the next success.
    #[tokio::test(start_paused = true)]
    async fn daemon_survives_transient_errors() {
        // polls 0,1,2 retryable-error; 3 intermediate (resets counter);
        // 4 SAVED.
        let (poll, _log) = scripted(|n| match n {
            0..=2 => Err(AttemptError::Retryable(format!("blip {n}"))),
            3 => Ok(ok_outcome(ProcessingStatus::Processing)),
            _ => Ok(ok_outcome(ProcessingStatus::Saved)),
        });
        let result = drive_poll_schedule(poll, CancellationToken::new()).await;
        match result {
            PollScheduleResult::Terminal { polls, .. } => assert_eq!(polls, 5),
            other => panic!("expected Terminal despite transient errors, got {other:?}"),
        }
    }

    /// Persistent retryable errors trip the consecutive-error cap and
    /// the daemon gives up cleanly (no terminal write, no zombie loop)
    /// after exactly [`MAX_CONSECUTIVE_DAEMON_ERRORS`] polls.
    #[tokio::test(start_paused = true)]
    async fn daemon_gives_up_after_consecutive_error_cap() {
        let (poll, _log) = scripted(|_| Err(AttemptError::Retryable("down".into())));
        let result = drive_poll_schedule(poll, CancellationToken::new()).await;
        match result {
            PollScheduleResult::GaveUp { polls, .. } => {
                assert_eq!(polls, MAX_CONSECUTIVE_DAEMON_ERRORS as u64);
            }
            other => panic!("expected GaveUp at the error cap, got {other:?}"),
        }
    }

    /// A non-retryable error (e.g. NAV schema drift) ends the daemon
    /// immediately — retrying cannot help.
    #[tokio::test(start_paused = true)]
    async fn daemon_gives_up_immediately_on_non_retryable() {
        let (poll, _log) = scripted(|_| Err(AttemptError::NonRetryable("schema drift".into())));
        let result = drive_poll_schedule(poll, CancellationToken::new()).await;
        match result {
            PollScheduleResult::GaveUp { polls, reason } => {
                assert_eq!(polls, 1);
                assert!(reason.contains("non-retryable"), "reason: {reason}");
            }
            other => panic!("expected GaveUp on non-retryable, got {other:?}"),
        }
    }

    /// Terminal on the very first poll stops cleanly after exactly one
    /// poll — no further polling, no zombie task.
    #[tokio::test(start_paused = true)]
    async fn daemon_terminal_on_first_poll_polls_once() {
        let calls = Rc::new(RefCell::new(0usize));
        let result = {
            let calls = calls.clone();
            drive_poll_schedule(
                move || {
                    *calls.borrow_mut() += 1;
                    async move { Ok(ok_outcome(ProcessingStatus::Aborted)) }
                },
                CancellationToken::new(),
            )
            .await
        };
        match result {
            PollScheduleResult::Terminal { polls, outcome } => {
                assert_eq!(polls, 1);
                assert_eq!(outcome.processing_status, ProcessingStatus::Aborted);
            }
            other => panic!("expected Terminal, got {other:?}"),
        }
        assert_eq!(
            *calls.borrow(),
            1,
            "exactly one poll for a first-poll terminal"
        );
    }

    /// PR-209 / S213 — cancelling the token mid-schedule exits with
    /// `Cancelled` without polling further. Pin guards the
    /// graceful-shutdown contract: the next app-boot recovery pass
    /// is responsible for re-spawning the daemon, NOT a terminal
    /// audit row written under the wrong premise.
    #[tokio::test(start_paused = true)]
    async fn daemon_cancel_token_exits_with_cancelled() {
        let cancel = CancellationToken::new();
        let cancel_for_spawn = cancel.clone();
        // After ~5s virtual time, fire cancellation. The daemon is
        // in its phase-1 sleep at this point (first delay = 1s, then
        // 2s, then 4s — 7s elapsed at the 3rd poll). Firing at 5s
        // catches the schedule mid-sleep, not mid-poll.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            cancel_for_spawn.cancel();
        });
        let (poll, _log) = scripted(|_| Ok(ok_outcome(ProcessingStatus::Processing)));
        let result = drive_poll_schedule(poll, cancel).await;
        match result {
            PollScheduleResult::Cancelled { polls } => {
                // 1s sleep + 1 poll, 2s sleep + 1 poll = 2 polls
                // by the time cancellation fires at 5s.
                assert!(
                    polls <= 3,
                    "expected ≤ 3 polls before cancellation, got {polls}"
                );
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    /// PR-209 / S213 — a token cancelled BEFORE the first sleep
    /// completes exits at the very first `select!` arm with zero
    /// polls. Conservative pin: a token that was already cancelled
    /// at spawn time must NOT slip a single poll through.
    #[tokio::test(start_paused = true)]
    async fn daemon_pre_cancelled_token_exits_with_zero_polls() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (poll, _log) = scripted(|_| Ok(ok_outcome(ProcessingStatus::Processing)));
        let result = drive_poll_schedule(poll, cancel).await;
        match result {
            PollScheduleResult::Cancelled { polls } => {
                assert_eq!(polls, 0, "pre-cancelled token must not poll");
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    /// The transaction-id lookup helper: a payload whose invoice_id
    /// matches yields the transaction_id; a non-matching invoice_id is
    /// None; a non-submission_response kind is None.
    #[test]
    fn extract_transaction_id_extracts_only_matching_invoice_id() {
        use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();

        let actor = Actor::from_local_cli("sess".to_string(), "user");
        let idem = IdempotencyKey::new();

        // One InvoiceSubmissionResponse for invoice A.
        let payload_a = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem,
            "TXID-A",
            b"<x/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                payload_a.to_bytes(),
                actor.clone(),
                None,
            )
            .unwrap();

        // One InvoiceSubmissionResponse for invoice B.
        let payload_b = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_B",
            idem,
            "TXID-B",
            b"<y/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                payload_b.to_bytes(),
                actor,
                None,
            )
            .unwrap();

        let entries = ledger.entries().unwrap();
        // Pick A out from a ledger that contains both.
        let txid_a = entries
            .iter()
            .rev()
            .find_map(|e| extract_transaction_id(e, "inv_A"))
            .unwrap();
        assert_eq!(txid_a, "TXID-A");

        // Pick B out of the same ledger.
        let txid_b = entries
            .iter()
            .rev()
            .find_map(|e| extract_transaction_id(e, "inv_B"))
            .unwrap();
        assert_eq!(txid_b, "TXID-B");

        // Non-matching invoice id returns None.
        let none = entries
            .iter()
            .rev()
            .find_map(|e| extract_transaction_id(e, "inv_NONE"));
        assert!(none.is_none());
    }
}
