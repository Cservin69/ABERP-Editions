//! Orchestration for the `aberp poll-annulment-ack` subcommand
//! (PR-14, ADR-0009 §6, ADR-0027).
//!
//! Wire-poll half of the technical-annulment surface — pairs with
//! PR-13's `submit_annulment` (the wire-submission half). Drives a
//! bounded poll loop against NAV's `queryTransactionStatus`
//! endpoint, keyed on the **annulment-side** `transactionId`
//! looked up from the most-recent
//! `InvoiceAnnulmentSubmissionResponse` audit entry per
//! ADR-0027 §4.
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args (8-digit tax number; tenant;
//!      endpoint). Same shape as `poll_ack::run` step 1.
//!   2. Load `NavCredentials` from the OS keychain (loud-fail on
//!      any missing artifact per ADR-0020 §3). Same posture as
//!      `poll_ack::run` step 2 — credentials BEFORE DB touch.
//!   3. Open a fresh `Ledger` read-only and resolve the
//!      annulment-side `transaction_id` AND the annulment-
//!      request's idempotency_key via
//!      [`lookup_annulment_poll_inputs`]. Loud-fail if no prior
//!      `InvoiceAnnulmentSubmissionResponse` exists (operator
//!      tried to poll an annulment that was never wire-
//!      submitted) — the error message explicitly steers the
//!      operator to run `aberp submit-annulment` first
//!      (CLAUDE.md rule 12 + ADR-0027 §4).
//!   4. Build a tokio current-thread runtime and drive the
//!      bounded poll loop on it. `queryTransactionStatus` is
//!      REUSED unchanged per ADR-0027 §3 + §"Surfaced conflict
//!      1" — NAV v3.0 documents one poll endpoint that takes
//!      any `transactionId`. Per attempt the loop:
//!        - calls `queryTransactionStatus` against the
//!          annulment-side transactionId,
//!        - writes ONE `InvoiceAnnulmentAckStatus` audit entry
//!          under its own DuckDB tx (per-poll commit so a crash
//!          mid-loop preserves every completed poll's evidence),
//!        - decides: terminal status (`SAVED` / `ABORTED`)
//!          breaks the loop; intermediate (`RECEIVED` /
//!          `PROCESSING`) sleeps the backoff and continues;
//!          non-retryable error breaks with `Stuck`; retryable
//!          error sleeps the backoff and continues.
//!   5. Verify the audit chain after the loop (success-criterion
//!      gate per ADR-0008).
//!   6. Operator-visible summary per ADR-0027 §5: on terminal
//!      `SAVED` the message NAMES THE RECEIVER-CONFIRMATION GAP
//!      LOUD — NAV's SAVED for an annulment submission means
//!      "NAV accepted the annulment for processing," NOT "the
//!      receiver has confirmed." CLAUDE.md rule 12; silently
//!      treating wire-SAVED as end-to-end confirmation is
//!      exactly the silent-omission failure mode rule 12 names.
//!
//! # Why per-poll commit, not one-tx-at-end
//!
//! Same posture as `poll_ack::poll_loop` per ADR-0009 §8 —
//! "every response across the chain" intent. A single-tx-at-end
//! posture would lose every completed poll's audit evidence on
//! a crash at attempt N>1.
//!
//! # Why a distinct module from `poll_ack.rs` (no shared helper)
//!
//! Per ADR-0027 §7 + CLAUDE.md rule 2: the two flows are
//! operator-facing twins today but operationally distinct (one
//! polls an invoice-side `transactionId`, the other an
//! annulment-side `transactionId`; the post-poll typestate
//! transitions are absent for annulments — the base invoice's
//! typestate is unchanged per ADR-0025 §2). A speculative
//! shared bounded-loop helper would couple the two surfaces; the
//! trigger to extract fires when (and if) a third poll surface
//! lands (e.g., the future receiver-confirmation observation
//! per ADR-0027 §"Surfaced conflict 3").
//!
//! # What this flow does NOT do
//!
//!   - It does NOT call `manageAnnulment`. The wire submission
//!     has already happened in a prior `submit_annulment` run.
//!   - It does NOT poll for receiver confirmation. ADR-0027
//!     §"Surfaced conflict 3": the receiver-confirmation
//!     observation is a separate future surface (likely
//!     `queryInvoiceData` / `queryInvoiceChainDigest` /
//!     `queryInvoiceCheck`; named trigger fires on first
//!     operator request for "did the receiver actually confirm
//!     the annulment?").
//!   - It does NOT mutate any billing row. Annulment is not an
//!     invoice operation; the base invoice's typestate is
//!     unchanged per ADR-0025 §2.
//!   - It does NOT auto-retry beyond the bounded loop's
//!     exponential-backoff schedule. Same posture as `poll_ack`.

use std::time::Duration;

use aberp_audit_ledger::{
    self as audit_ledger, Actor, Entry, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_nav_transport::{
    operations::query_transaction_status::{self, ProcessingStatus, QueryTransactionStatusOutcome},
    NavCredentials, NavEndpoint, NavTransport, NavTransportError,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::{NavEnv, PollAnnulmentAckArgs};

/// ADR-0009 §5: "Max attempts: 5, exponential backoff (1s, 2s, 4s,
/// 8s, 16s)." Duplicated here rather than re-exported from
/// `poll_ack` per the operator-facing-twin posture (CLAUDE.md
/// rule 2): the annulment-poll's max-attempts cap may diverge in
/// a future PR if NAV-side receiver-side processing needs a
/// larger budget. If the two ever drift the operator-visible
/// surfaces will say so explicitly.
const MAX_POLL_ATTEMPTS: u32 = 5;

/// ADR-0009 §5 backoff base. Same value as
/// `poll_ack::BACKOFF_BASE_MILLIS` for the same regulator-facing-
/// invariant reason; duplicated for the same posture as
/// `MAX_POLL_ATTEMPTS` above.
const BACKOFF_BASE_MILLIS: u64 = 1_000;

/// Error-side outcome from one attempt. Mirror of
/// `poll_ack::AttemptError`; held distinct so a future divergence
/// (e.g., an annulment-poll-specific retry classification) is
/// visible at compile time rather than as a silent rename.
#[derive(Debug)]
enum AttemptError {
    Retryable(String),
    NonRetryable(String),
}

/// Where the loop ended up after consuming all attempts (or
/// breaking early). Drives the operator-visible summary. Mirror
/// of `poll_ack::LoopTerminus` with no typestate transitions
/// (the base invoice's typestate is unchanged per ADR-0025 §2).
#[derive(Debug)]
enum LoopTerminus {
    LastStatus(ProcessingStatus),
    AllAttemptsErrored(String),
    NonRetryableError(String),
}

/// Inputs resolved by the audit-ledger walk per ADR-0027 §4.
/// Captured as a typed value so the per-poll audit-write code
/// path carries both the annulment-request idempotency key (per
/// the F8 contract — ADR-0026 §F8 + ADR-0027 §6) and the
/// annulment-side `transaction_id` without re-walking the
/// ledger.
#[derive(Debug)]
struct AnnulmentPollInputs {
    /// NAV-assigned annulment-side `transactionId` from the most-
    /// recent `InvoiceAnnulmentSubmissionResponse` for the base
    /// invoice. Passed verbatim to `queryTransactionStatus`.
    transaction_id: String,
    /// The annulment-request's idempotency key (also persisted
    /// on the wire-response entry per ADR-0026 §F8). Flows into
    /// the per-poll `InvoiceAnnulmentAckStatus` audit entries
    /// per ADR-0027 §6 so the audit-evidence-bundle reader can
    /// walk back from any poll entry to the originating
    /// `InvoiceTechnicalAnnulmentRequested` operator-decision
    /// entry via shared key.
    annulment_idempotency_key: String,
}

pub fn run(args: &PollAnnulmentAckArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "poll_annulment_ack",
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

    // 2. Load NAV credentials BEFORE touching the DB — same
    //    posture as `poll_ack::run` step 2.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for poll-annulment-ack"
    );

    // 3. Resolve the annulment-side transactionId + the
    //    annulment-request idempotency key from the audit
    //    ledger (ADR-0027 §4). Open the ledger read-only; the
    //    walker loud-fails if no
    //    InvoiceAnnulmentSubmissionResponse exists for this
    //    invoice and steers the operator to run
    //    `aberp submit-annulment` first (CLAUDE.md rule 12).
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let inputs = {
        let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger for poll-annulment-ack lookup")?;
        lookup_annulment_poll_inputs(&ledger, &args.invoice_id)?
    };
    tracing::info!(
        transaction_id = %inputs.transaction_id,
        annulment_idempotency_key = %inputs.annulment_idempotency_key,
        "annulment-side transactionId + idempotency_key resolved from audit ledger"
    );

    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 4. tokio current-thread runtime for the poll loop. Built
    //    AFTER every prerequisite is validated so we don't pay
    //    the startup cost on a malformed input. Open a fresh
    //    Connection here (the lookup-ledger was opened read-
    //    only and is already dropped; the per-poll audit-write
    //    path needs a writable Connection).
    let mut conn = Connection::open(&args.db)
        .with_context(|| format!("open tenant DuckDB at {}", args.db.display()))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for annulment poll loop")?;

    let terminus = runtime.block_on(poll_loop(
        nav_endpoint,
        &credentials,
        &tax_number_8,
        &inputs.transaction_id,
        &args.invoice_id,
        &inputs.annulment_idempotency_key,
        &mut conn,
        &ledger_meta,
        &actor,
    ))?;

    // 5. Verify the audit chain after the loop (success-
    //    criterion gate). Drop the tx-Connection first; re-open
    //    a fresh Ledger to read.
    drop(conn);
    let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes)
        .context("re-open audit ledger after poll-annulment-ack")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER poll-annulment-ack")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 5a. PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit.
    let mirror_path = audit_ledger::mirror_path_for(&args.db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after poll-annulment-ack commit")?;

    // 6. Operator-visible summary per ADR-0027 §5. The terminal-
    //    SAVED message NAMES THE RECEIVER-CONFIRMATION GAP LOUD
    //    (CLAUDE.md rule 12) — load-bearing message text per
    //    ADR-0027 §"Adversarial review #2"; a future contributor
    //    removing the caveat would mislead an operator into
    //    interpreting wire-SAVED as end-to-end confirmation. The
    //    integration test pins the substring "receiver must
    //    still confirm" so the intent survives editorial
    //    rewording but a content-dropping edit fails loud at
    //    commit time.
    match terminus {
        LoopTerminus::LastStatus(ProcessingStatus::Saved) => {
            tracing::error!(
                invoice_id = %args.invoice_id,
                transaction_id = %inputs.transaction_id,
                "annulment-poll terminal SAVED at the wire level; receiver must still confirm in NAV web UI per ADR-0009 §6"
            );
            println!(
                "poll-annulment-ack OK: invoice {} -> NAV annulment transactionId {} reached SAVED \
                 (audit chain verified across {} entries). \
                 NOTE: NAV-side SAVED means the annulment submission has been accepted for \
                 processing; the receiver must still confirm the annulment in the NAV web UI \
                 per ADR-0009 §6. ABERP does NOT yet observe receiver confirmation; a future \
                 query-receiver-confirmation PR per ADR-0027 §\"Surfaced conflict 3\" will \
                 close that gap.",
                args.invoice_id, inputs.transaction_id, verified,
            );
        }
        LoopTerminus::LastStatus(ProcessingStatus::Aborted) => {
            tracing::error!(
                invoice_id = %args.invoice_id,
                transaction_id = %inputs.transaction_id,
                "annulment-poll terminal ABORTED — NAV rejected the annulment submission; operator action required"
            );
            println!(
                "poll-annulment-ack REJECTED: invoice {} -> NAV annulment transactionId {} \
                 reached ABORTED (audit chain verified across {} entries). \
                 NAV did not accept the annulment submission; the receiver-confirmation \
                 question is moot. Operator action required — inspect the verbatim NAV \
                 response in the audit ledger to diagnose the rejection.",
                args.invoice_id, inputs.transaction_id, verified,
            );
        }
        LoopTerminus::LastStatus(intermediate) => {
            tracing::error!(
                invoice_id = %args.invoice_id,
                transaction_id = %inputs.transaction_id,
                last_status = intermediate.as_nav_str(),
                "annulment-poll attempts exhausted with intermediate status, STUCK"
            );
            println!(
                "poll-annulment-ack STUCK: invoice {} -> annulment transactionId {} -> last \
                 status {} after {} attempts (audit chain verified across {} entries) — \
                 operator action required",
                args.invoice_id,
                inputs.transaction_id,
                intermediate.as_nav_str(),
                MAX_POLL_ATTEMPTS,
                verified,
            );
        }
        LoopTerminus::NonRetryableError(diagnostic) => {
            tracing::error!(
                invoice_id = %args.invoice_id,
                transaction_id = %inputs.transaction_id,
                "annulment-poll: NAV non-retryable error, STUCK: {}",
                diagnostic,
            );
            println!(
                "poll-annulment-ack STUCK: invoice {} -> annulment transactionId {} -> NAV \
                 non-retryable error: {} (audit chain verified across {} entries) — operator \
                 action required",
                args.invoice_id, inputs.transaction_id, diagnostic, verified,
            );
        }
        LoopTerminus::AllAttemptsErrored(diagnostic) => {
            tracing::error!(
                invoice_id = %args.invoice_id,
                transaction_id = %inputs.transaction_id,
                "annulment-poll: every attempt errored, STUCK: {}",
                diagnostic,
            );
            println!(
                "poll-annulment-ack STUCK: invoice {} -> annulment transactionId {} -> all {} \
                 attempts errored, last: {} (audit chain verified across {} entries) — \
                 operator action required",
                args.invoice_id, inputs.transaction_id, MAX_POLL_ATTEMPTS, diagnostic, verified,
            );
        }
    }

    Ok(())
}

/// Walk the audit ledger and resolve the inputs for the
/// annulment poll per ADR-0027 §4. Loud-fail if no prior
/// `InvoiceAnnulmentSubmissionResponse` exists for this invoice
/// (the operator must run `aberp submit-annulment` first) — the
/// named-error message is the operator-visible review surface
/// per CLAUDE.md rule 12.
fn lookup_annulment_poll_inputs(ledger: &Ledger, invoice_id: &str) -> Result<AnnulmentPollInputs> {
    let entries = ledger.entries().context("read audit ledger entries")?;
    let inputs = entries
        .iter()
        .rev() // most-recent first per ADR-0027 §4
        .find_map(|entry| extract_annulment_poll_inputs(entry, invoice_id))
        .ok_or_else(|| {
            anyhow!(
                "no InvoiceAnnulmentSubmissionResponse audit entry found for invoice {} \
                 — there is no wire-submitted annulment to poll. \
                 Run `aberp submit-annulment --annulment-xml ... --invoice-id {} ...` first \
                 (ADR-0027 §4 precondition).",
                invoice_id,
                invoice_id
            )
        })?;
    if inputs.transaction_id.is_empty() {
        return Err(anyhow!(
            "InvoiceAnnulmentSubmissionResponse for invoice {invoice_id} has empty transaction_id"
        ));
    }
    Ok(inputs)
}

/// Inspect one audit entry: if it is an
/// `InvoiceAnnulmentSubmissionResponse` whose payload's
/// `invoice_id` matches the target, return its `transaction_id`
/// AND `idempotency_key`. Else `None`. Typed-payload decode per
/// F9 (same posture as `poll_ack::extract_transaction_id`); a
/// parse error returns None so the caller's "no entry found"
/// loud-fail surfaces the real problem.
fn extract_annulment_poll_inputs(entry: &Entry, invoice_id: &str) -> Option<AnnulmentPollInputs> {
    if entry.kind != EventKind::InvoiceAnnulmentSubmissionResponse {
        return None;
    }
    let parsed: audit_payloads::InvoiceAnnulmentSubmissionResponsePayload =
        serde_json::from_slice(&entry.payload).ok()?;
    if parsed.invoice_id == invoice_id {
        Some(AnnulmentPollInputs {
            transaction_id: parsed.transaction_id,
            annulment_idempotency_key: parsed.idempotency_key,
        })
    } else {
        None
    }
}

/// Bounded poll loop. `queryTransactionStatus` is REUSED across
/// the invoice-poll and annulment-poll flows per ADR-0027 §3 +
/// §"Surfaced conflict 1" — NAV v3.0 documents one poll endpoint
/// that takes any `transactionId`. The discriminator-level fork
/// lives at the audit-ledger `InvoiceAnnulmentAckStatus` variant
/// per ADR-0027 §2; this loop writes that kind on each attempt.
///
/// `queryTransactionStatus` is a NAV *query* operation per
/// ADR-0009 §4 — it authenticates via the per-request `<user>`
/// block (passwordHash + non-`manageInvoice` requestSignature)
/// and does NOT consume an `exchangeToken`. The poll loop makes
/// one HTTP call per attempt.
#[allow(clippy::too_many_arguments)]
async fn poll_loop(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    transaction_id: &str,
    invoice_id: &str,
    annulment_idempotency_key: &str,
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: &Actor,
) -> Result<LoopTerminus> {
    let transport = NavTransport::new(endpoint).context("build NAV transport")?;

    // Ensure the audit-ledger schema exists once up-front so the
    // per-poll tx body can call `append_in_tx` directly. Same
    // posture as `poll_ack::poll_loop`.
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for poll-annulment-ack")?;

    let mut last_error: Option<String> = None;
    let mut last_intermediate_status: Option<ProcessingStatus> = None;

    for attempt in 1..=MAX_POLL_ATTEMPTS {
        let _span = tracing::info_span!("annulment_poll_attempt", attempt).entered();

        let outcome = run_one_attempt(&transport, credentials, tax_number_8, transaction_id).await;

        match outcome {
            Ok(query_outcome) => {
                let status = query_outcome.processing_status;
                tracing::info!(
                    status = status.as_nav_str(),
                    response_bytes = query_outcome.response_xml.len(),
                    "queryTransactionStatus OK (annulment)"
                );

                // Audit-write per poll. The annulment-request's
                // idempotency_key flows on each append per
                // ADR-0027 §6 — divergence from `poll_ack` (which
                // passes None) that closes the per-annulment
                // audit lineage; the audit-evidence-bundle reader
                // walks back from any ack-status entry to the
                // operator-decision entry via shared key.
                write_annulment_ack_audit_entry(
                    conn,
                    ledger_meta,
                    actor,
                    invoice_id,
                    transaction_id,
                    annulment_idempotency_key,
                    &query_outcome,
                )?;

                if status.is_terminal() {
                    return Ok(LoopTerminus::LastStatus(status));
                }
                last_intermediate_status = Some(status);
            }
            Err(AttemptError::Retryable(diag)) => {
                tracing::warn!(
                    attempt,
                    "queryTransactionStatus (annulment) retryable error: {}",
                    diag
                );
                last_error = Some(diag);
            }
            Err(AttemptError::NonRetryable(diag)) => {
                tracing::error!(
                    attempt,
                    "queryTransactionStatus (annulment) non-retryable error: {}",
                    diag
                );
                return Ok(LoopTerminus::NonRetryableError(diag));
            }
        }

        if attempt < MAX_POLL_ATTEMPTS {
            let delay = Duration::from_millis(BACKOFF_BASE_MILLIS * (1u64 << (attempt - 1)));
            tracing::info!(
                next_attempt = attempt + 1,
                backoff_millis = delay.as_millis() as u64,
                "backing off before next annulment poll attempt"
            );
            tokio::time::sleep(delay).await;
        }
    }

    Ok(match (last_intermediate_status, last_error) {
        (Some(status), _) => LoopTerminus::LastStatus(status),
        (None, Some(diag)) => LoopTerminus::AllAttemptsErrored(diag),
        (None, None) => LoopTerminus::AllAttemptsErrored(
            "internal: annulment poll loop exhausted attempts with neither status nor error captured"
                .to_string(),
        ),
    })
}

/// One attempt: a single `queryTransactionStatus` call. On NAV-
/// side success returns the typed outcome; on any error, maps it
/// to the internal [`AttemptError`] classification. Mirror of
/// `poll_ack::run_one_attempt` — same NAV-transport-error
/// routing.
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
            // Same loud-fail posture as `poll_ack::run_one_attempt`
            // — with the XSD validator landed (PR-9-0 / ADR-0022)
            // on our own emitted bytes, a parse failure on NAV's
            // response side means NAV is sending a shape we
            // cannot parse. Retrying does not help. Per ADR-0027
            // §"Adversarial review #1", this is exactly the
            // surface that catches a NAV-side annulment-response
            // shape divergence loud rather than silently coercing
            // into the wrong terminal state.
            Err(AttemptError::NonRetryable(format!("parse: {msg}")))
        }
        Err(other) => Err(AttemptError::Retryable(format!("nav-transport: {other}"))),
    }
}

/// Open a fresh per-poll DuckDB tx, append one
/// `InvoiceAnnulmentAckStatus` entry carrying the verbatim NAV
/// response_xml + parsed ack_status, commit. The annulment-
/// request's idempotency_key flows on the append per ADR-0027 §6
/// — divergence from `poll_ack`'s `None` posture that closes the
/// per-annulment audit lineage.
fn write_annulment_ack_audit_entry(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: &Actor,
    invoice_id: &str,
    transaction_id: &str,
    annulment_idempotency_key: &str,
    outcome: &QueryTransactionStatusOutcome,
) -> Result<()> {
    let tx = conn
        .transaction()
        .context("begin per-poll DuckDB transaction (InvoiceAnnulmentAckStatus append)")?;

    let payload = audit_payloads::InvoiceAnnulmentAckStatusPayload::new(
        invoice_id,
        transaction_id,
        outcome.processing_status.as_nav_str(),
        outcome.response_xml.clone(),
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceAnnulmentAckStatus,
        payload.to_bytes(),
        actor.clone(),
        // ADR-0027 §6: pass the annulment-request idempotency
        // key on EACH poll entry. This is a deliberate divergence
        // from `poll_ack`'s `None` posture (which anchors entries
        // on invoice_id + transaction_id because the invoice's
        // issuance key is already on the chain). The annulment
        // poll's walker would otherwise have no shared key
        // connecting its entries to the originating
        // InvoiceTechnicalAnnulmentRequested entry — the wire-
        // response entry's idempotency_key already matches the
        // request's per ADR-0026 §F8; we preserve that chain on
        // the poll entries too.
        Some(annulment_idempotency_key.to_string()),
    )
    .context("audit_ledger::append_in_tx InvoiceAnnulmentAckStatus")?;

    tx.commit()
        .context("commit per-poll DuckDB transaction (InvoiceAnnulmentAckStatus append)")?;
    Ok(())
}

/// 8-digit base of a Hungarian tax number. Mirror of
/// `submit_invoice::parse_tax_number_8` /
/// `poll_ack::parse_tax_number_8` per the operator-facing-twin
/// posture (rule 2). If the copies drift they will produce
/// confusingly different errors on the same operator input; the
/// contract pin in `mod tests` below catches that drift at
/// commit time.
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
// Tests — parse_tax_number_8 contract pin + lookup discipline +
// backoff invariant.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};
    use aberp_billing::IdempotencyKey;

    /// Sanity: the backoff schedule matches ADR-0009 §5 exactly
    /// — same pin as `poll_ack::tests::backoff_schedule_matches…`.
    /// If the two operator-facing twins ever drift on the
    /// regulator-facing invariant, this test fails loud here so
    /// the divergence is intentional, not silent.
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
        let total_realized = 1_000 + 2_000 + 4_000 + 8_000;
        assert_eq!(total_realized, 15_000);
    }

    /// `parse_tax_number_8` MUST match
    /// `submit_invoice::parse_tax_number_8` /
    /// `poll_ack::parse_tax_number_8` /
    /// `submit_annulment::parse_tax_number_8` per the operator-
    /// facing-twin posture. If the five copies drift they will
    /// produce confusingly different errors on the same input.
    #[test]
    fn tax_number_8_parses_same_as_submit_invoice() {
        assert_eq!(parse_tax_number_8("12345678").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1-42").unwrap(), "12345678");
        assert!(parse_tax_number_8("1234567").is_err());
        assert!(parse_tax_number_8("1234567X").is_err());
        assert!(parse_tax_number_8("123456789-1-42").is_err());
    }

    fn ledger_with_entries(entries: Vec<(EventKind, Vec<u8>, Option<String>)>) -> Ledger {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        for (kind, payload, idem) in entries {
            ledger.append(kind, payload, actor.clone(), idem).unwrap();
        }
        ledger
    }

    fn annulment_response_payload(
        invoice_id: &str,
        idem: IdempotencyKey,
        wire_txid: &str,
    ) -> Vec<u8> {
        audit_payloads::InvoiceAnnulmentSubmissionResponsePayload::new(
            invoice_id,
            idem,
            wire_txid,
            b"<ManageAnnulmentResponse/>".to_vec(),
        )
        .to_bytes()
    }

    /// Happy path per ADR-0027 §4: a prior
    /// `InvoiceAnnulmentSubmissionResponse` for the invoice
    /// resolves the annulment-side `transactionId` AND the
    /// annulment-request idempotency key (F8 carry-forward per
    /// ADR-0026 §F8 + ADR-0027 §6).
    #[test]
    fn lookup_resolves_transaction_id_and_idempotency_key() {
        let idem = IdempotencyKey::new();
        let entries = vec![(
            EventKind::InvoiceAnnulmentSubmissionResponse,
            annulment_response_payload("inv_A", idem, "WIRE-TXID-1"),
            Some(idem.to_canonical_string()),
        )];
        let ledger = ledger_with_entries(entries);
        let inputs = lookup_annulment_poll_inputs(&ledger, "inv_A")
            .expect("a wire-submitted annulment must be pollable");
        assert_eq!(inputs.transaction_id, "WIRE-TXID-1");
        assert_eq!(inputs.annulment_idempotency_key, idem.to_canonical_string());
    }

    /// ADR-0027 §4 / CLAUDE.md rule 12: an invoice with no prior
    /// `InvoiceAnnulmentSubmissionResponse` loud-fails with a
    /// message that steers the operator to run `submit-annulment`
    /// first. The named-error message is part of the operator-
    /// visible artifact (rule 9 — load-bearing review surface,
    /// per ADR-0027 §"Adversarial review #4").
    #[test]
    fn lookup_rejects_no_prior_wire_submission() {
        let entries = vec![]; // empty ledger
        let ledger = ledger_with_entries(entries);
        let err = lookup_annulment_poll_inputs(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no InvoiceAnnulmentSubmissionResponse"),
            "error must name the missing wire-response entry: got {msg}"
        );
        assert!(
            msg.contains("submit-annulment"),
            "error must steer the operator to submit-annulment: got {msg}"
        );
    }

    /// Cross-invoice contamination: a wire response against
    /// inv_B must NOT resolve inputs for inv_A. Defence-in-depth
    /// pin mirroring
    /// `check_annulment_is_submittable_does_not_cross_invoice_ids`
    /// in `submit_annulment.rs`.
    #[test]
    fn lookup_does_not_cross_invoice_ids() {
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        let entries = vec![
            (
                EventKind::InvoiceAnnulmentSubmissionResponse,
                annulment_response_payload("inv_B", idem_b, "WIRE-TXID-B"),
                Some(idem_b.to_canonical_string()),
            ),
            (
                EventKind::InvoiceAnnulmentSubmissionResponse,
                annulment_response_payload("inv_A", idem_a, "WIRE-TXID-A"),
                Some(idem_a.to_canonical_string()),
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let inputs = lookup_annulment_poll_inputs(&ledger, "inv_A")
            .expect("inv_A's poll inputs must resolve regardless of inv_B's wire state");
        assert_eq!(inputs.transaction_id, "WIRE-TXID-A");
        assert_eq!(
            inputs.annulment_idempotency_key,
            idem_a.to_canonical_string()
        );
    }

    /// ADR-0027 §4: when multiple wire responses exist for the
    /// same invoice (which the submit-annulment precondition
    /// walker rejects — but the audit ledger is append-only, so
    /// a misbehaved future writer could land them), the LATEST
    /// by seq is the one to poll against. Verifies the reverse-
    /// walk discipline.
    #[test]
    fn lookup_picks_latest_when_multiple_responses_exist() {
        let idem_old = IdempotencyKey::new();
        let idem_new = IdempotencyKey::new();
        let entries = vec![
            (
                EventKind::InvoiceAnnulmentSubmissionResponse,
                annulment_response_payload("inv_A", idem_old, "OLD-WIRE-TXID"),
                Some(idem_old.to_canonical_string()),
            ),
            (
                EventKind::InvoiceAnnulmentSubmissionResponse,
                annulment_response_payload("inv_A", idem_new, "NEW-WIRE-TXID"),
                Some(idem_new.to_canonical_string()),
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let inputs = lookup_annulment_poll_inputs(&ledger, "inv_A")
            .expect("must resolve when at least one wire response exists");
        assert_eq!(
            inputs.transaction_id, "NEW-WIRE-TXID",
            "latest-by-seq wire response must win"
        );
        assert_eq!(
            inputs.annulment_idempotency_key,
            idem_new.to_canonical_string()
        );
    }

    /// An empty `transaction_id` field on the wire-response
    /// entry loud-fails (defence-in-depth — submit-annulment
    /// won't ordinarily write such an entry per
    /// `manage_annulment::call`'s loud-fail on missing
    /// `<transactionId>`, but a tampered ledger or future-
    /// regression could). CLAUDE.md rule 12.
    #[test]
    fn lookup_rejects_empty_transaction_id() {
        let idem = IdempotencyKey::new();
        let entries = vec![(
            EventKind::InvoiceAnnulmentSubmissionResponse,
            annulment_response_payload("inv_A", idem, ""),
            Some(idem.to_canonical_string()),
        )];
        let ledger = ledger_with_entries(entries);
        let err = lookup_annulment_poll_inputs(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty transaction_id"),
            "error must name the empty-transaction_id failure: got {msg}"
        );
    }

    /// `extract_annulment_poll_inputs` MUST ignore entries whose
    /// kind is not `InvoiceAnnulmentSubmissionResponse` — a
    /// future audit-ledger schema-drift that stores something
    /// else with the same payload shape must not be confused
    /// with the wire-response entry. Defence-in-depth pin.
    #[test]
    fn extract_inputs_ignores_non_wire_response_kinds() {
        use aberp_audit_ledger::Entry;
        let idem = IdempotencyKey::new();
        let payload = audit_payloads::InvoiceAnnulmentSubmissionResponsePayload::new(
            "inv_A",
            idem,
            "WIRE-TXID-1",
            b"<x/>".to_vec(),
        )
        .to_bytes();
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        // Append the payload bytes under the WRONG kind on
        // purpose — extract must refuse to return inputs from
        // it.
        ledger
            .append(EventKind::InvoiceAckStatus, payload, actor, None)
            .unwrap();
        let entries = ledger.entries().unwrap();
        let entry: &Entry = entries.last().unwrap();
        let got = extract_annulment_poll_inputs(entry, "inv_A");
        assert!(
            got.is_none(),
            "extract must refuse a non-AnnulmentSubmissionResponse entry even if the JSON parses"
        );
    }
}
