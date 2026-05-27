//! Orchestration for the `aberp submit-invoice` subcommand (PR-7-B-3,
//! amended by PR-19 / ADR-0032 §1 to the two-tx Attempt-before-call
//! posture).
//!
//! # Pipeline (PR-19 / ADR-0032 §1 — two-tx posture)
//!
//!   1. Parse + validate CLI args: 8-digit tax number, invoice id
//!      shape, env vs prod endpoint choice.
//!   2. Load `NavCredentials` from the OS keychain (loud-fail on any
//!      missing artifact per ADR-0020 §3).
//!   3. Read the NAV InvoiceData XML bytes from disk — the same file
//!      `aberp issue-invoice --out ...` produced.
//!   4. Open the tenant DuckDB + load the previously-issued invoice +
//!      its idempotency key from the billing store (one tx scoped to
//!      the read so the connection is free for the audit-ledger tx
//!      after).
//!   5. Build the actor + ledger meta with the keychain-derived login
//!      (Actor::from_local_cli — F15 closed in PR-7-A).
//!   6. NAV prepare phase (async, on the caller's runtime — owned by
//!      the CLI's top-level `run`, or the axum handler's runtime for
//!      the SPA-side `POST /invoices/:id/submit`):
//!      - tokenExchange against the chosen NAV endpoint.
//!      - `manage_invoice::build_request` — render the
//!        `<ManageInvoiceRequest>` envelope bytes (no wire activity
//!        yet).
//!   7. **TX1 — Attempt-before-call** (ADR-0032 §1). Under one DuckDB
//!      transaction: append `InvoiceSubmissionAttempt` (verbatim
//!      request bytes from step 6). Commit. Sync mirror per ADR-0030
//!      §2.
//!   8. **Wire send** — POST the pre-rendered envelope via
//!      `manage_invoice::send_built_request`. Parse the response;
//!      classify errors.
//!   9. **TX2 — Response or AttemptFailed** (ADR-0032 §1). Under a
//!      second DuckDB transaction:
//!      - On success: append `InvoiceSubmissionResponse` (verbatim
//!        response bytes + parsed `transaction_id`). Commit. Sync
//!        mirror.
//!      - On failure: append `InvoiceSubmissionAttemptFailed`
//!        (typed `error_class` + optional `error_code` +
//!        `error_message` + optional `response_xml` per
//!        `submission_queue::classify_attempt_failure`). Commit.
//!        Sync mirror. Then surface the wire error to the caller.
//!  10. Verify the audit chain after commit (success-criterion gate).
//!  11. Print the typestate transition + transaction id.
//!
//! # Why two transactions instead of one
//!
//! ADR-0032 §1 names the design intent: ADR-0009 §8's
//! `invoice.submission_attempt` "Fires before the response is
//! received" wording is satisfied if and only if the Attempt audit
//! row is committed BEFORE the NAV POST. The single-tx posture
//! (PR-7-B-3) wrote both Attempt and Response in one tx AFTER the
//! NAV call returned success — which meant a failed manageInvoice
//! call left NO audit trail (F40). The two-tx posture closes F40 at
//! the issuing-path level: TX1 commits the Attempt unconditionally;
//! TX2 commits Response (success) or AttemptFailed (failure).
//!
//! A process crash between TX1 and TX2 leaves an Attempt-only audit
//! state (state-2 Pending per ADR-0032 §4) — operator-recoverable
//! via the existing `retry-submission` command, which now accepts
//! state-2 in addition to the pre-PR-19 state-3 (AwaitingAck)
//! precondition.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT poll `queryTransactionStatus` (PR-7-C).
//!   - It does NOT advance the invoice past `Submitted` — the
//!     terminal state lands when the ack poll terminal-positives.
//!   - It does NOT retry transient errors (PR-7-C's poll-side retry
//!     loop will land alongside).
//!   - It does NOT mutate any billing row — the `submission_state`
//!     fact lives in the audit ledger per the PR-7-B-3 design
//!     assumption A6.
//!   - It does NOT consult `queryInvoiceCheck` to disambiguate
//!     "NAV already has this submission" from "the wire broke" —
//!     Layer-2 idempotency per ADR-0009 §5 + ADR-0032 §"Open
//!     questions" remains named-deferred (F44).

use std::path::Path;

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{self as billing, IdempotencyKey, ReadyInvoice};
use aberp_nav_transport::{
    operations::{manage_invoice, token_exchange, TechnicalValidation},
    soap::{InvoiceOperation, ManageInvoiceItem},
    NavCredentials, NavEndpoint, NavTransport, NavTransportError,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::{NavEnv, SubmitInvoiceArgs};
use crate::submission_queue;

pub fn run(args: &SubmitInvoiceArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "submit_invoice",
        invoice_id = %args.invoice_id,
        tenant = %args.tenant,
        endpoint = ?args.endpoint,
    )
    .entered();

    // PR-44η / session-60 — thin wrapper over [`submit_from_inputs`].
    // The CLI-specific responsibilities (load NAV credentials, mint the
    // `Actor`, read XML bytes from `--invoice-xml`, print the operator-
    // visible summary line) stay here; the
    // prepare-attempt-wire-response-audit pipeline lives in the library
    // function so the new `POST /invoices/:id/submit` route
    // (`serve.rs::submit_invoice_request`) calls the same path.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for this CLI invocation"
    );

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
    tracing::info!(bytes = invoice_xml.len(), "InvoiceData XML loaded");

    let nav_endpoint = match args.endpoint {
        NavEnv::Test => NavEndpoint::Test,
        NavEnv::Production => NavEndpoint::Production,
    };
    let endpoint_audit_label = match args.endpoint {
        NavEnv::Test => "test",
        NavEnv::Production => "production",
    };

    // PR-56 / session-76 — build the tokio runtime at the CLI's
    // top-level so [`submit_from_inputs`] can stay async-native. Prior
    // to PR-56 the library helper built a current-thread runtime and
    // `block_on`'d its two NAV awaits internally, which panicked the
    // moment the helper was called from the axum handler's already-
    // running multi-thread runtime ("Cannot start a runtime from
    // within a runtime"). Owning the runtime here keeps the CLI's
    // sync `main` shape while letting the SPA-side handler `.await`
    // the same library function without nesting.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for submit-invoice CLI")?;
    match runtime.block_on(submit_from_inputs(SubmitFromInputs {
        db: &args.db,
        tenant_str: &args.tenant,
        invoice_id_str: &args.invoice_id,
        invoice_xml_origin: args.invoice_xml.display().to_string(),
        invoice_xml,
        tax_number_raw: &args.tax_number,
        nav_endpoint,
        endpoint_audit_label,
        credentials: &credentials,
        actor,
    })) {
        Ok(outcome) => {
            println!(
                "submitted invoice {} (seq {}) -> NAV transactionId {} \
                 (audit chain verified across {} entries)",
                outcome.invoice_id,
                outcome.sequence_number,
                outcome.transaction_id,
                outcome.entries_verified,
            );
            Ok(())
        }
        Err(SubmitFromInputsError::WireFailed {
            invoice_id,
            error_message,
            entries_verified,
            error_class,
        }) => {
            eprintln!(
                "submit-invoice FAILED for invoice {}: {} \
                 (audit chain verified across {} entries; \
                 InvoiceSubmissionAttemptFailed recorded with error_class={}); \
                 invoice is now in state-2 Pending — re-run `aberp retry-submission` \
                 to retry (note: a state-2 retry may produce a duplicate submission \
                 to NAV until Layer-2 queryInvoiceCheck per ADR-0009 §5 lands; F44)",
                invoice_id, error_message, entries_verified, error_class,
            );
            Err(anyhow!(
                "submit-invoice manageInvoice failed: {}",
                error_message
            ))
        }
        Err(SubmitFromInputsError::NavUpstreamFault {
            status,
            fault_code,
            fault_message,
            technical_validations,
            body_preview,
        }) => {
            // PR-58 / session-78 — operator-visible eprintln for the
            // CLI path. The fault code + Hungarian-localized message
            // (when present) are the actionable diagnostic; the
            // body_preview is the fallback evidence when parsing
            // could not extract a typed pair.
            // PR-59 / session-79 — also emit the per-rule
            // technical_validations array NAV carried for the rejection.
            // For `INVALID_REQUEST` (the most common NAV 400 wrapper)
            // the validation list is the actual diagnostic; the
            // top-level fault_code is just the generic envelope.
            eprintln!(
                "submit-invoice FAILED at NAV tokenExchange (HTTP {}): \
                 fault_code={} fault_message={} \
                 technical_validations={} body_preview=`{}`",
                status,
                fault_code.as_deref().unwrap_or("<none>"),
                fault_message.as_deref().unwrap_or("<none>"),
                technical_validations.len(),
                body_preview,
            );
            for (i, v) in technical_validations.iter().enumerate() {
                eprintln!(
                    "  [{}] result_code={} error_code={} tag={} message={}",
                    i,
                    v.result_code.as_deref().unwrap_or("<none>"),
                    v.error_code.as_deref().unwrap_or("<none>"),
                    v.tag.as_deref().unwrap_or("<none>"),
                    v.message.as_deref().unwrap_or("<none>"),
                );
            }
            Err(anyhow!(
                "NAV tokenExchange returned HTTP {status} \
                 (fault_code={fault_code:?}, \
                 technical_validations={})",
                technical_validations.len()
            ))
        }
        Err(SubmitFromInputsError::Other(e)) => Err(e),
    }
}

/// PR-44η / session-60 — successful submission outcome returned by
/// [`submit_from_inputs`]. The CLI consumes this to print the
/// operator-facing summary line; the serve route surfaces
/// `transaction_id` + the new typestate label on the wire response.
#[derive(Debug)]
pub struct SubmitInvoiceOutcome {
    pub invoice_id: String,
    pub sequence_number: u64,
    pub transaction_id: String,
    pub entries_verified: u64,
}

/// PR-44η / session-60 — bundled input shape for
/// [`submit_from_inputs`]. Reduces the `too_many_arguments` lint noise
/// and keeps the call sites readable. Borrowed fields where possible
/// so callers don't pay an allocation per field; the `invoice_xml`
/// is moved in because the library consumes it.
#[allow(missing_docs)]
pub struct SubmitFromInputs<'a> {
    pub db: &'a Path,
    pub tenant_str: &'a str,
    pub invoice_id_str: &'a str,
    /// Operator-facing origin label for `invoice_xml` — used only in
    /// error messages so a malformed body's source location is
    /// visible. The CLI passes the on-disk path; the serve route
    /// passes the audit-ledger nav_xml_path resolved server-side.
    pub invoice_xml_origin: String,
    pub invoice_xml: Vec<u8>,
    pub tax_number_raw: &'a str,
    pub nav_endpoint: NavEndpoint,
    pub endpoint_audit_label: &'static str,
    pub credentials: &'a NavCredentials,
    pub actor: Actor,
}

/// PR-44η / session-60 — error returned by [`submit_from_inputs`]. The
/// happy / wire-failed split lets the CLI format its eprintln summary
/// AND the serve route surface a typed 500 body without duplicating
/// the `format!("{e:#}")` path. Every non-wire failure (bad creds, DB
/// error, audit-write error, etc.) is folded into
/// [`SubmitFromInputsError::Other`] which carries the inner anyhow
/// error verbatim.
///
/// PR-58 / session-78 — the `NavUpstreamFault` variant lifts NAV's
/// tokenExchange non-2xx HTTP response (HTTP-layer rejection BEFORE
/// any application-layer envelope was built) into a typed surface so
/// the route can return 502 with the parsed `fault_code` /
/// `fault_message` / `body_preview` instead of an opaque 500. Pre-PR-58
/// this rejection was anyhow-wrapped and squashed into "internal error"
/// at the route boundary, hiding the operator-actionable diagnostic
/// (e.g. NAV-portal IP-whitelist mismatch, expired technical-user
/// password, signature drift). No audit-ledger entry is written for
/// the tokenExchange failure path per ADR-0032 §1 — the invoice
/// remains NeverSubmitted and is operator-retriable.
///
/// PR-59 / session-79 — extended with `technical_validations`. NAV's
/// `INVALID_REQUEST` top-level wrapper is generic; the per-rule
/// diagnostic lives inside the repeating `<technicalValidationMessages>`
/// array (parser at `nav_transport::operations::parse_nav_fault`). The
/// SPA renders the list inside the existing fault panel.
#[derive(Debug)]
pub enum SubmitFromInputsError {
    WireFailed {
        invoice_id: String,
        error_message: String,
        entries_verified: u64,
        error_class: &'static str,
    },
    NavUpstreamFault {
        status: u16,
        fault_code: Option<String>,
        fault_message: Option<String>,
        technical_validations: Vec<TechnicalValidation>,
        body_preview: String,
    },
    Other(anyhow::Error),
}

impl std::fmt::Display for SubmitFromInputsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitFromInputsError::WireFailed {
                invoice_id,
                error_message,
                error_class,
                ..
            } => write!(
                f,
                "submit-invoice manageInvoice failed for {invoice_id} \
                 (error_class={error_class}): {error_message}"
            ),
            SubmitFromInputsError::NavUpstreamFault {
                status,
                fault_code,
                fault_message,
                technical_validations,
                body_preview,
            } => write!(
                f,
                "NAV tokenExchange returned HTTP {status} \
                 (fault_code={fault_code:?}, fault_message={fault_message:?}, \
                 technical_validations={technical_validations:?}) \
                 body_preview=`{body_preview}`"
            ),
            SubmitFromInputsError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SubmitFromInputsError {}

impl From<anyhow::Error> for SubmitFromInputsError {
    fn from(e: anyhow::Error) -> Self {
        SubmitFromInputsError::Other(e)
    }
}

/// PR-44η / session-60 — library-callable submission entry. Consumed
/// by [`run`] (the CLI path) AND by `serve::submit_invoice_request`
/// (the loopback `POST /invoices/:id/submit` route). Both surfaces
/// share one prepare + attempt + wire + response/audit pipeline so a
/// regression in submission surfaces at both gates.
///
/// Pipeline (steps map to the pre-PR-44η `run` numbering in this
/// module's doc comment):
///
///   1. Parse `tax_number_raw` to its 8-digit base; resolve `TenantId`.
///   3a. NAV v3.0 XSD invariant check on `invoice_xml`.
///   4. Load the previously-issued invoice + idempotency key.
///   5–6. Build ledger meta + NAV prepare (tokenExchange + envelope
///        construction; no wire send yet).
///   7. TX1 Attempt audit + mirror sync.
///   8. Wire send.
///   9. TX2 Response audit (success) or AttemptFailed audit (failure)
///      + mirror sync.
///   10. Verify-chain success-criterion gate.
///
/// On wire failure the [`SubmitFromInputsError::WireFailed`] variant
/// carries the operator-visible summary inputs (`invoice_id`,
/// `error_message`, `entries_verified`, `error_class`) so callers can
/// format the eprintln line or the route's typed JSON body without
/// re-walking the audit ledger. The TX2 AttemptFailed audit is
/// already committed; the invoice is left in state-2 Pending per
/// ADR-0032 §4.
pub async fn submit_from_inputs(
    inputs: SubmitFromInputs<'_>,
) -> std::result::Result<SubmitInvoiceOutcome, SubmitFromInputsError> {
    let SubmitFromInputs {
        db,
        tenant_str,
        invoice_id_str,
        invoice_xml_origin,
        invoice_xml,
        tax_number_raw,
        nav_endpoint,
        endpoint_audit_label,
        credentials,
        actor,
    } = inputs;

    // 1. Parse + validate inputs.
    let tenant = TenantId::new(tenant_str.to_string())
        .ok_or_else(|| anyhow!("tenant value '{}' is empty or has a null byte", tenant_str))
        .map_err(SubmitFromInputsError::Other)?;
    let tax_number_8 = parse_tax_number_8(tax_number_raw).map_err(SubmitFromInputsError::Other)?;

    // 3a. PR-9-0 / ADR-0022: validate on-disk XML BEFORE any NAV call.
    aberp_nav_xsd_validator::validate_invoice_data(&invoice_xml)
        .with_context(|| {
            format!(
                "NAV InvoiceData v3.0 invariant check (ADR-0022) failed for {invoice_xml_origin}"
            )
        })
        .map_err(SubmitFromInputsError::Other)?;
    tracing::info!(
        nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
        "on-disk InvoiceData XML passed v3.0 invariant check before NAV submit"
    );

    // 4. Load the previously-issued invoice + its idempotency_key.
    let mut conn = Connection::open(db)
        .with_context(|| format!("open tenant DuckDB at {}", db.display()))
        .map_err(SubmitFromInputsError::Other)?;
    let (ready_invoice, idempotency_key) =
        load_issued_invoice(&mut conn, invoice_id_str).map_err(SubmitFromInputsError::Other)?;
    if ready_invoice.id.to_prefixed_string() != invoice_id_str {
        return Err(SubmitFromInputsError::Other(anyhow!(
            "loaded invoice id {} does not match requested {}",
            ready_invoice.id.to_prefixed_string(),
            invoice_id_str
        )));
    }
    tracing::info!(
        seq = ready_invoice.sequence_number,
        idempotency_key = %idempotency_key.to_canonical_string(),
        "issued invoice loaded for submission"
    );

    // 5. Build ledger meta.
    let binary_hash_bytes = binary_hash::compute()
        .context("compute binary hash")
        .map_err(SubmitFromInputsError::Other)?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 6. NAV prepare phase — `.await`ed on whatever runtime the caller
    //    owns. PR-56 / session-76 — pre-PR-56 this function built its
    //    own current-thread runtime and `block_on`'d the two NAV calls
    //    inline, which panicked when called from the axum handler's
    //    already-running multi-thread runtime ("Cannot start a runtime
    //    from within a runtime"). The CLI now owns the runtime at the
    //    top of `run`; the HTTP handler simply `.await`s.
    let prepared =
        prepare_for_attempt_audit(nav_endpoint, credentials, &tax_number_8, &invoice_xml)
            .await
            .map_err(|e| match e {
                PrepareError::NavUpstreamFault {
                    status,
                    fault_code,
                    fault_message,
                    technical_validations,
                    body_preview,
                } => SubmitFromInputsError::NavUpstreamFault {
                    status,
                    fault_code,
                    fault_message,
                    technical_validations,
                    body_preview,
                },
                PrepareError::Other(inner) => SubmitFromInputsError::Other(inner),
            })?;
    tracing::info!(
        request_bytes = prepared.request_xml.len(),
        "manageInvoice envelope built; ready to write TX1 Attempt audit"
    );

    // 7. TX1 — Attempt-before-call.
    write_attempt_audit(
        &mut conn,
        &ledger_meta,
        actor.clone(),
        &ready_invoice,
        idempotency_key,
        endpoint_audit_label,
        prepared.request_xml.clone(),
    )
    .map_err(SubmitFromInputsError::Other)?;
    {
        let ledger_tx1 = Ledger::open(db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger after TX1 commit")
            .map_err(SubmitFromInputsError::Other)?;
        let mirror_path = audit_ledger::mirror_path_for(db);
        ledger_tx1
            .sync_mirror(&mirror_path)
            .context("sync audit-ledger mirror file after TX1 Attempt commit")
            .map_err(SubmitFromInputsError::Other)?;
    }
    tracing::info!("TX1 Attempt audit committed; mirror synced; sending manageInvoice");

    // 8. Wire send.
    let wire_result =
        manage_invoice::send_built_request(&prepared.transport, &prepared.request_xml).await;

    // 9. TX2 — Response on success, AttemptFailed on failure.
    match wire_result {
        Ok(send_outcome) => {
            tracing::info!(
                transaction_id = %send_outcome.transaction_id,
                "NAV manageInvoice OK"
            );
            write_response_audit(
                &mut conn,
                &ledger_meta,
                actor.clone(),
                &ready_invoice,
                idempotency_key,
                &send_outcome.transaction_id,
                send_outcome.response_xml,
            )
            .map_err(SubmitFromInputsError::Other)?;
            drop(conn);
            let ledger = Ledger::open(db, tenant, binary_hash_bytes)
                .context("open audit ledger after TX2 Response commit")
                .map_err(SubmitFromInputsError::Other)?;
            let verified = ledger
                .verify_chain()
                .context("audit-ledger chain verification failed AFTER submission")
                .map_err(SubmitFromInputsError::Other)?;
            tracing::info!(entries_verified = verified, "audit chain verified");
            let mirror_path = audit_ledger::mirror_path_for(db);
            ledger
                .sync_mirror(&mirror_path)
                .context("sync audit-ledger mirror file after TX2 Response commit")
                .map_err(SubmitFromInputsError::Other)?;
            let submitted = ready_invoice.into_submitted(send_outcome.transaction_id.clone());
            Ok(SubmitInvoiceOutcome {
                invoice_id: submitted.id.to_prefixed_string(),
                sequence_number: submitted.sequence_number,
                transaction_id: submitted.nav_transaction_id,
                entries_verified: verified,
            })
        }
        Err(wire_err) => {
            let (error_class, error_code) = submission_queue::classify_attempt_failure(&wire_err);
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
            )
            .map_err(SubmitFromInputsError::Other)?;
            drop(conn);
            let ledger = Ledger::open(db, tenant, binary_hash_bytes)
                .context("open audit ledger after TX2 AttemptFailed commit")
                .map_err(SubmitFromInputsError::Other)?;
            let verified = ledger
                .verify_chain()
                .context("audit-ledger chain verification failed AFTER AttemptFailed")
                .map_err(SubmitFromInputsError::Other)?;
            let mirror_path = audit_ledger::mirror_path_for(db);
            ledger
                .sync_mirror(&mirror_path)
                .context("sync audit-ledger mirror file after TX2 AttemptFailed commit")
                .map_err(SubmitFromInputsError::Other)?;
            tracing::error!(
                invoice_id = %ready_invoice.id.to_prefixed_string(),
                entries_verified = verified,
                error_class = error_class,
                "submit-invoice: manageInvoice failed; TX2 AttemptFailed audit written"
            );
            Err(SubmitFromInputsError::WireFailed {
                invoice_id: ready_invoice.id.to_prefixed_string(),
                error_message,
                entries_verified: verified,
                error_class,
            })
        }
    }
}

/// PR-19 / ADR-0032 §1: the NAV prepare-for-attempt-audit bundle. Holds
/// the open `NavTransport` (so the subsequent send_built_request reuses
/// the trust-pinned client) and the rendered request envelope bytes
/// (the load-bearing input for TX1's Attempt audit write).
struct PreparedSubmission {
    transport: NavTransport,
    request_xml: Vec<u8>,
}

/// PR-19 / ADR-0032 §1 + §3: open the transport, tokenExchange, build
/// the `<ManageInvoiceRequest>` envelope. NO wire send for manageInvoice.
///
/// tokenExchange itself IS a wire call — it must succeed before
/// manageInvoice's envelope (which carries the decrypted token in its
/// signature) can be built. A tokenExchange failure leaves NO
/// Attempt audit (the manageInvoice envelope was never built);
/// classifies as `client_build` / `transport` per
/// `submission_queue::classify_attempt_failure` and surfaces loud
/// per CLAUDE.md rule 12. ADR-0032 §1 + §"Adversarial review" — the
/// invoice's audit ledger has no Attempt for the failed tokenExchange
/// path, so the precondition walker classifies the invoice as
/// NeverSubmitted (drain may pick it up on next run), NOT as state-2
/// Pending.
async fn prepare_for_attempt_audit(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    invoice_xml: &[u8],
) -> std::result::Result<PreparedSubmission, PrepareError> {
    let transport = NavTransport::new(endpoint)
        .context("build NAV transport")
        .map_err(PrepareError::Other)?;
    let token = match token_exchange::call(&transport, credentials, tax_number_8).await {
        Ok(t) => t,
        // PR-58 / session-78 — surface NAV's HTTP-layer rejection as a
        // typed fault so the route boundary returns 502 with the
        // parsed fault_code / fault_message / body_preview instead of
        // anyhow-wrapping it into an opaque 500. Every other
        // tokenExchange failure (transport / parse / decrypt) folds
        // into `PrepareError::Other`.
        Err(NavTransportError::TokenExchangeHttpStatus {
            status,
            fault_code,
            fault_message,
            technical_validations,
            body_preview,
        }) => {
            tracing::error!(
                status,
                fault_code = ?fault_code,
                fault_message = ?fault_message,
                technical_validations = ?technical_validations,
                body_preview = %body_preview,
                "NAV tokenExchange rejected: non-2xx HTTP status"
            );
            return Err(PrepareError::NavUpstreamFault {
                status,
                fault_code,
                fault_message,
                technical_validations,
                body_preview,
            });
        }
        Err(other) => {
            return Err(PrepareError::Other(
                anyhow::Error::new(other).context("NAV tokenExchange"),
            ));
        }
    };
    // The per-invoice `operation` is detected from the XML body's
    // shape via the three-way classifier (CREATE / STORNO / MODIFY).
    // PR-11 / ADR-0024 §3 closed F22 by extending PR-10's two-way
    // classifier with the `<modificationIssueDate>` disambiguator
    // for MODIFY.
    let operation = detect_operation_from_xml(invoice_xml).map_err(PrepareError::Other)?;
    let request_xml = manage_invoice::build_request(
        credentials,
        tax_number_8,
        &token.decoded_token,
        &[ManageInvoiceItem {
            operation,
            invoice_data_xml: invoice_xml,
        }],
    )
    .map_err(|e: NavTransportError| {
        PrepareError::Other(anyhow!(
            "manage_invoice::build_request (envelope construction) failed: {e}"
        ))
    })?;
    Ok(PreparedSubmission {
        transport,
        request_xml,
    })
}

/// PR-58 / session-78 — typed error returned by
/// [`prepare_for_attempt_audit`]. The `NavUpstreamFault` variant lifts
/// NAV's HTTP-layer rejection (tokenExchange non-2xx) so the calling
/// layers can route it into a typed 502 instead of an opaque 500. Every
/// other failure (transport, envelope construction, XML parse) folds
/// into `Other` and surfaces as 500.
enum PrepareError {
    NavUpstreamFault {
        status: u16,
        fault_code: Option<String>,
        fault_message: Option<String>,
        technical_validations: Vec<TechnicalValidation>,
        body_preview: String,
    },
    Other(anyhow::Error),
}

/// Detect the per-invoice `<operation>` value from the
/// `<InvoiceData>` body's shape. Deterministic code, not an LLM
/// classification (CLAUDE.md rule 5).
///
/// Three-way classifier (PR-11, ADR-0024 §3 — closes F22):
///
/// | Body shape | Result |
/// |---|---|
/// | No `<invoiceReference>` | `Create` |
/// | Contains `<invoiceReference>` AND `<modificationIssueDate>` | `Modify` |
/// | Contains `<invoiceReference>` and NOT `<modificationIssueDate>` | `Storno` |
///
/// Match the OPENING tag with no attributes; the emitter always
/// writes both `<invoiceReference>` and `<modificationIssueDate>`
/// bare. A future emitter that adds an attribute to either would
/// change `<x>` to `<x attr="...">` and the contains-check would
/// miss — the round-trip pair-up tests
/// (`apps/aberp/tests/issue_storno_xml_round_trip.rs` +
/// `apps/aberp/tests/issue_modification_xml_round_trip.rs`) close the
/// trap by construction: the validator + emitter pair fail together
/// if a structural assumption breaks.
fn detect_operation_from_xml(xml: &[u8]) -> Result<InvoiceOperation> {
    let body = std::str::from_utf8(xml)
        .context("invoice XML is not valid UTF-8 — NAV requires UTF-8 per the v3.0 schema")?;
    if !body.contains("<invoiceReference>") {
        return Ok(InvoiceOperation::Create);
    }
    if body.contains("<modificationIssueDate>") {
        Ok(InvoiceOperation::Modify)
    } else {
        Ok(InvoiceOperation::Storno)
    }
}

/// Open a scoped read tx, look up the issued invoice, and return it
/// alongside its persisted idempotency key (F8 — the same key flows
/// from issuance into the submit audit entries).
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
    // Commit the read tx as a no-op so the Connection is returned
    // to a clean state; rolling back a read-only tx is also fine,
    // commit() is cheaper.
    tx.commit().context("commit read transaction")?;
    Ok(pair)
}

/// PR-19 / ADR-0032 §1: TX1 audit-write — open one audit tx, append
/// the `InvoiceSubmissionAttempt` entry, commit. Called BEFORE the
/// wire send so a transport-mid-flight loss leaves the Attempt row
/// committed. F8 carry: the payload carries the issuance idempotency
/// key.
fn write_attempt_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    request_xml: Vec<u8>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for submit-invoice TX1 Attempt")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (submit-invoice TX1 Attempt audit append)")?;
    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();
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
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttempt (TX1)")?;
    tx.commit()
        .context("commit DuckDB transaction (submit-invoice TX1 Attempt audit append)")?;
    Ok(())
}

/// PR-19 / ADR-0032 §1: TX2 success audit-write — open one audit tx,
/// append the `InvoiceSubmissionResponse` entry, commit. Called
/// AFTER the wire send returns success. Pairs with the TX1 Attempt
/// row via the F8 idempotency key.
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
        .context("ensure audit-ledger schema for submit-invoice TX2 Response")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (submit-invoice TX2 Response audit append)")?;
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
    .context("audit_ledger::append_in_tx InvoiceSubmissionResponse (TX2)")?;
    tx.commit()
        .context("commit DuckDB transaction (submit-invoice TX2 Response audit append)")?;
    Ok(())
}

/// PR-19 / ADR-0032 §1 + §2: TX2 failure audit-write — open one
/// audit tx, append the `InvoiceSubmissionAttemptFailed` entry,
/// commit. Called AFTER the wire send returns an error. Pairs with
/// the TX1 Attempt row via the F8 idempotency key; the
/// `error_class` discriminator carries the failure-class per
/// `submission_queue::classify_attempt_failure`.
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
        .context("ensure audit-ledger schema for submit-invoice TX2 AttemptFailed")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (submit-invoice TX2 AttemptFailed audit append)")?;
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
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttemptFailed (TX2)")?;
    tx.commit()
        .context("commit DuckDB transaction (submit-invoice TX2 AttemptFailed audit append)")?;
    Ok(())
}

/// Extract the 8-digit base of a Hungarian tax number.
///
/// Hungarian tax numbers have the form `BBBBBBBB-V-CC` where:
///
///   - `BBBBBBBB` is the 8-digit base identifier (the bit NAV's
///     `<taxNumber>` element accepts).
///   - `V` is a single VAT-type digit.
///   - `CC` is the two-digit county code.
///
/// All three accepted input shapes (`12345678`, `12345678-1`,
/// `12345678-1-42`) collapse to the same 8-digit base for NAV. Any
/// other shape is loud-failed — passing the dashed full form
/// unchanged to NAV produces `INVALID_SECURITY_USER` and surfacing
/// the wrong-shape input HERE keeps that confusing failure off the
/// wire.
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
    fn tax_number_8_accepts_bare_form() {
        assert_eq!(parse_tax_number_8("12345678").unwrap(), "12345678");
    }

    #[test]
    fn tax_number_8_accepts_dash_one_form() {
        assert_eq!(parse_tax_number_8("12345678-1").unwrap(), "12345678");
    }

    #[test]
    fn tax_number_8_accepts_full_dashed_form() {
        assert_eq!(parse_tax_number_8("12345678-1-42").unwrap(), "12345678");
    }

    #[test]
    fn tax_number_8_rejects_short_base() {
        let err = parse_tax_number_8("1234567").unwrap_err();
        assert!(err.to_string().contains("not 8 ASCII digits"));
    }

    #[test]
    fn tax_number_8_rejects_non_digit_base() {
        let err = parse_tax_number_8("1234567X").unwrap_err();
        assert!(err.to_string().contains("not 8 ASCII digits"));
    }

    #[test]
    fn tax_number_8_rejects_long_base() {
        let err = parse_tax_number_8("123456789-1-42").unwrap_err();
        assert!(err.to_string().contains("not 8 ASCII digits"));
    }

    #[test]
    fn tax_number_8_rejects_leading_dash() {
        let err = parse_tax_number_8("-12345678").unwrap_err();
        assert!(err.to_string().contains("not 8 ASCII digits"));
    }

    // ── PR-10 / F20: operation detection from XML body ──────────────

    #[test]
    fn detect_operation_create_on_plain_invoice() {
        let xml = b"<?xml version=\"1.0\"?>\
            <InvoiceData><invoiceNumber>X/00001</invoiceNumber>\
            <invoiceMain><invoice><invoiceHead/></invoice></invoiceMain></InvoiceData>";
        assert_eq!(
            detect_operation_from_xml(xml).unwrap(),
            InvoiceOperation::Create
        );
    }

    #[test]
    fn detect_operation_storno_when_invoice_reference_present() {
        let xml = b"<?xml version=\"1.0\"?>\
            <InvoiceData><invoiceNumber>X/00002</invoiceNumber>\
            <invoiceMain><invoice>\
            <invoiceReference><originalInvoiceNumber>X/00001</originalInvoiceNumber>\
            <modifyWithoutMaster>false</modifyWithoutMaster>\
            <modificationIndex>1</modificationIndex></invoiceReference>\
            <invoiceHead/></invoice></invoiceMain></InvoiceData>";
        assert_eq!(
            detect_operation_from_xml(xml).unwrap(),
            InvoiceOperation::Storno
        );
    }

    /// PR-11 / ADR-0024 §3 / F22: MODIFY-shape body carries BOTH
    /// `<invoiceReference>` AND `<modificationIssueDate>`. The
    /// detector flips to `Modify` on the second substring's presence.
    /// CLAUDE.md rule 9: this is the intent-pinning test for the
    /// MODIFY arm — without it a future regression flattening
    /// `Modify` back to `Storno` would still pass the two-arm test
    /// list above.
    #[test]
    fn detect_operation_modify_when_modification_issue_date_present() {
        let xml = b"<?xml version=\"1.0\"?>\
            <InvoiceData><invoiceNumber>X/00003</invoiceNumber>\
            <invoiceMain><invoice>\
            <invoiceReference><originalInvoiceNumber>X/00001</originalInvoiceNumber>\
            <modificationIssueDate>2026-05-21</modificationIssueDate>\
            <modifyWithoutMaster>false</modifyWithoutMaster>\
            <modificationIndex>2</modificationIndex></invoiceReference>\
            <invoiceHead/></invoice></invoiceMain></InvoiceData>";
        assert_eq!(
            detect_operation_from_xml(xml).unwrap(),
            InvoiceOperation::Modify
        );
    }

    /// Defence-in-depth: a body that carries `<modificationIssueDate>`
    /// WITHOUT `<invoiceReference>` must still classify as `Create`
    /// (the modification field on its own does not assert chain
    /// membership; the chain link is the `<invoiceReference>` block).
    /// This shape should not arise from the ABERP emitters, but if a
    /// future operator-edited file carries it, the deterministic rule
    /// is "no invoice reference => no chain => Create".
    #[test]
    fn detect_operation_create_when_modification_date_without_reference() {
        let xml = b"<?xml version=\"1.0\"?>\
            <InvoiceData><invoiceNumber>X/00004</invoiceNumber>\
            <invoiceMain><invoice>\
            <invoiceHead><modificationIssueDate>2026-05-21</modificationIssueDate></invoiceHead>\
            </invoice></invoiceMain></InvoiceData>";
        assert_eq!(
            detect_operation_from_xml(xml).unwrap(),
            InvoiceOperation::Create
        );
    }

    /// Non-UTF-8 bytes must loud-fail rather than silently treating
    /// the body as CREATE. CLAUDE.md rule 12.
    #[test]
    fn detect_operation_loud_fails_on_non_utf8() {
        let xml = [0xff, 0xfe, 0xfd];
        let err = detect_operation_from_xml(&xml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("UTF-8"), "expected UTF-8 error, got: {msg}");
    }
}
