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

use std::path::Path;
use std::time::Duration;

use aberp_audit_ledger::{
    self as audit_ledger, Actor, Entry, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_billing::{self as billing, IdempotencyKey, ReadyInvoice};
use aberp_nav_transport::{
    operations::query_transaction_status::{self, ProcessingStatus, QueryTransactionStatusOutcome},
    NavCredentials, NavEndpoint, NavTransport, NavTransportError,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
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
    let outcome = runtime.block_on(poll_ack_from_inputs(
        &args.db,
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
    db: &Path,
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

    // 3. Open DuckDB; load the invoice + its idempotency key.
    let mut conn =
        Connection::open(db).with_context(|| format!("open tenant DuckDB at {}", db.display()))?;
    let (ready_invoice, idempotency_key) = load_issued_invoice(&mut conn, invoice_id_str)?;
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
        &mut conn,
        &ledger_meta,
        &actor,
    )
    .await?;

    // 6. Verify the audit chain (success-criterion gate).
    drop(conn);
    let ledger = Ledger::open(db, tenant, binary_hash_bytes).context("open audit ledger")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER poll-ack")?;
    tracing::info!(entries_verified = verified, "audit chain verified");
    let mirror_path = audit_ledger::mirror_path_for(db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after poll-ack commit")?;

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
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
    invoice_id: &str,
) -> Result<String> {
    let ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to look up transactionId")?;
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
fn extract_transaction_id(entry: &Entry, invoice_id: &str) -> Option<String> {
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
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: &Actor,
) -> Result<LoopTerminus> {
    let transport = NavTransport::new(endpoint).context("build NAV transport")?;

    // Ensure the audit-ledger schema exists once up-front so the
    // per-poll tx body can call `append_in_tx` directly (the schema
    // is idempotent so this is safe even when called after prior
    // submit-invoice runs).
    audit_ledger::ensure_schema(conn).context("ensure audit-ledger schema for poll-ack")?;

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
                        conn,
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
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: &Actor,
    invoice_id: &str,
    transaction_id: &str,
    outcome: &QueryTransactionStatusOutcome,
) -> Result<()> {
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
