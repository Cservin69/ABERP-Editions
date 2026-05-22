//! Orchestration for the `aberp drain-submission-queue` subcommand
//! (PR-18, ADR-0031 §3).
//!
//! Walks the audit ledger, classifies pending invoices via
//! [`crate::submission_queue::pending_from_ledger`], and submits
//! them to NAV in FIFO order by issue date. Per-invoice pipeline
//! mirrors `submit_invoice::run` field-for-field — same NAV
//! handshake, same audit-write transaction posture, same mirror-
//! sync hook.
//!
//! # Pipeline (whole run)
//!
//!   1. Parse + validate CLI args.
//!   2. Load `NavCredentials` from the OS keychain (loud-fail on any
//!      missing artifact per ADR-0020 §3) — same posture as every
//!      other NAV-touching subcommand.
//!   3. Compute the binary hash + build `LedgerMeta`.
//!   4. Resolve pending invoices via
//!      [`crate::submission_queue::pending_from_ledger`]. FIFO by
//!      issue date.
//!   5. Surface alert-threshold WARN lines (ADR-0031 §6).
//!   6. Build the per-run XML-path-override map from CLI args.
//!   7. Drive the per-invoice pipeline in a loop (see below).
//!   8. Print the run summary.
//!
//! # Pipeline (per invoice, PR-19 / ADR-0032 §1 — two-tx posture)
//!
//!   a. Resolve the on-disk XML path: payload's `nav_xml_path`
//!      (Some) OR per-invocation override OR loud-fail.
//!   b. Read the XML bytes from disk.
//!   c. Validate via `aberp_nav_xsd_validator::validate_invoice_data`
//!      — same pre-NAV gate every existing `submit-*` runs.
//!   d. tokenExchange + `manage_invoice::build_request` — render the
//!      envelope (no wire send yet).
//!   e. **TX1 — Attempt-before-call** (ADR-0032 §1). Write the
//!      `InvoiceSubmissionAttempt` audit entry under one tx. Commit.
//!      Sync mirror per ADR-0030 §2.
//!   f. **Wire send** — POST the pre-rendered envelope via
//!      `manage_invoice::send_built_request`.
//!   g. **TX2 — Response on success, AttemptFailed on failure**
//!      (ADR-0032 §1). Append `InvoiceSubmissionResponse` or
//!      `InvoiceSubmissionAttemptFailed` under one tx. Commit. Sync
//!      mirror.
//!   h. Re-open the Ledger; verify the chain; print the per-invoice
//!      OK or FAILED line.
//!
//! # Transport-vs-application error classification (ADR-0031 §4)
//!
//! Step d's NAV error path is forked. A transport error
//! ([`crate::submission_queue::is_transport_error`]) short-circuits
//! the FIFO loop — the remaining pending invoices stay pending for
//! the next drain run. An application error surfaces per-invoice
//! LOUD and the loop continues to the next invoice. The
//! application-vs-transport distinction is a `match` on the typed
//! `NavTransportError` — deterministic code per CLAUDE.md rule 5.
//!
//! # Why NOT re-use `submit_invoice::run`
//!
//! `submit_invoice::run` takes `&SubmitInvoiceArgs` (which the drain
//! does not have per-invoice) and writes the same audit entries the
//! drain writes — but per CLAUDE.md rule 8 ("read before write")
//! and the operator-facing-twin posture
//! (`retry_submission.rs::load_issued_invoice`'s comment names the
//! same trade-off): the drain mirrors `submit_invoice`'s shape
//! inline rather than refactoring to share. A future PR that adds
//! divergent behaviour (e.g. F40's Attempt-before-call posture) is
//! easier to land when the two orchestrations are independently
//! editable.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT process `SubmissionStuck` invoices (they have an
//!     `InvoiceSubmissionResponse`, so they're excluded from the
//!     pending set by construction). `retry-submission` is the
//!     operator-confirmed path for those.
//!   - It does NOT enforce ADR-0009 §7's 24h / 72h submission
//!     deadlines — F41 named-trigger.
//!   - It does NOT process state-2 Pending invoices (they have an
//!     `InvoiceSubmissionAttempt`, so they're excluded from the
//!     pending set per ADR-0032 §5's fourth-predicate clause).
//!     `retry-submission` accepts state-2 per ADR-0032 §4.
//!   - It does NOT amortise tokenExchange across multiple invoices
//!     — NAV v3.0 protocol assigns per-request tokens.

use std::collections::HashMap;
use std::path::Path;

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::IdempotencyKey;
use aberp_nav_transport::{
    operations::{manage_invoice, token_exchange},
    soap::{InvoiceOperation, ManageInvoiceItem},
    NavCredentials, NavEndpoint, NavTransport, NavTransportError,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::{DrainSubmissionQueueArgs, NavEnv};
use crate::submission_queue::{
    self, PendingInvoice, ALERT_OLDEST_PENDING, ALERT_PENDING_COUNT,
};

// ──────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────

pub fn run(args: &DrainSubmissionQueueArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "drain_submission_queue",
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
        "NAV credentials loaded; actor derived for drain-submission-queue"
    );

    // 3. Compute binary hash + LedgerMeta.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 4. Resolve pending invoices via the audit-ledger walker. FIFO
    //    by issue date.
    let pending = {
        let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger for pending-submissions walk")?;
        submission_queue::pending_from_ledger(&ledger)?
    };
    let pending_count = pending.len();
    tracing::info!(
        pending_count = pending_count,
        "drain-submission-queue: pending invoices resolved"
    );

    if pending.is_empty() {
        println!("drain-submission-queue: 0 pending invoices; nothing to do.");
        return Ok(());
    }

    // 5. Alert thresholds (ADR-0031 §6). WARN-only; no control flow.
    surface_alert_thresholds(&pending);

    // 6. Per-invocation override map for pre-PR-18 entries (or
    //    operator-relocated XML files).
    let override_map = parse_xml_path_overrides(&args.xml_path_overrides)?;

    // 7. Drive the per-invoice pipeline.
    let limit = if args.max_invoices == 0 {
        pending_count
    } else {
        args.max_invoices.min(pending_count)
    };
    let mut ok_count: usize = 0;
    let mut application_error_count: usize = 0;
    let mut transport_error: Option<String> = None;
    let mut stop_index: Option<usize> = None;

    for (idx, invoice) in pending.iter().take(limit).enumerate() {
        let outcome = drive_one_invoice(
            invoice,
            &override_map,
            &args.db,
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
            Ok(()) => {
                ok_count += 1;
            }
            Err(DrainPerInvoiceError::Transport(msg)) => {
                transport_error = Some(msg.clone());
                stop_index = Some(idx);
                tracing::error!(
                    invoice_id = %invoice.invoice_id,
                    "drain-submission-queue: NAV transport error; stopping. {}",
                    msg
                );
                eprintln!(
                    "drain-submission-queue: NAV transport error on invoice {}; \
                     {} invoice(s) drained, {} pending after this run. \
                     Re-run when NAV is reachable. Error: {}",
                    invoice.invoice_id,
                    ok_count,
                    pending_count - ok_count,
                    msg
                );
                break;
            }
            Err(DrainPerInvoiceError::Application(msg)) => {
                application_error_count += 1;
                tracing::error!(
                    invoice_id = %invoice.invoice_id,
                    "drain-submission-queue: per-invoice application error; continuing. {}",
                    msg
                );
                eprintln!(
                    "drain-submission-queue: invoice {} FAILED (continuing to next): {}",
                    invoice.invoice_id, msg
                );
            }
        }
    }

    // 8. Run summary. LOUD per CLAUDE.md rule 12: every count is
    //    surfaced and any short-circuit is named.
    println!(
        "drain-submission-queue: drained {} of {} pending invoices \
         (application errors: {}, transport error: {}, max-invoices: {}). \
         Stopped early: {}.",
        ok_count,
        pending_count,
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

    // If a transport error fired, exit non-zero by returning Err.
    if let Some(msg) = transport_error {
        return Err(anyhow!(
            "drain-submission-queue: transport error short-circuited the run: {}",
            msg
        ));
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Per-invoice driver
// ──────────────────────────────────────────────────────────────────────

/// Typed error class so the run loop can fork on transport vs.
/// application. The `String` carries the operator-visible
/// diagnostic; the structured `anyhow::Error` does not need to
/// propagate beyond per-invoice scope.
#[derive(Debug)]
enum DrainPerInvoiceError {
    /// Stop the drain. Transport-layer failure (HTTP / TLS / DNS /
    /// reqwest::Client). ADR-0031 §4.
    Transport(String),
    /// Continue the drain. NAV-side application error, credential
    /// error, XSD-validation error, file-read error, audit-write
    /// error — anything that's not a transport failure. ADR-0031 §4.
    Application(String),
}

#[allow(clippy::too_many_arguments)]
fn drive_one_invoice(
    invoice: &PendingInvoice,
    override_map: &HashMap<String, String>,
    db_path: &Path,
    nav_endpoint: NavEndpoint,
    endpoint_audit_label: &'static str,
    credentials: &NavCredentials,
    tax_number_8: &str,
    ledger_meta: &LedgerMeta,
    tenant: TenantId,
    binary_hash_bytes: aberp_audit_ledger::BinaryHash,
    actor: Actor,
) -> Result<(), DrainPerInvoiceError> {
    // a. Resolve the on-disk XML path.
    let xml_path = resolve_xml_path(invoice, override_map).map_err(|e| {
        DrainPerInvoiceError::Application(format!("{e:#}"))
    })?;

    // b. Read the XML bytes.
    let invoice_xml = std::fs::read(&xml_path).map_err(|e| {
        DrainPerInvoiceError::Application(format!(
            "read NAV InvoiceData XML from {}: {e}",
            xml_path
        ))
    })?;
    if invoice_xml.is_empty() {
        return Err(DrainPerInvoiceError::Application(format!(
            "invoice XML at {} is empty",
            xml_path
        )));
    }

    // c. Validate via the v3.0 invariant check (ADR-0022).
    aberp_nav_xsd_validator::validate_invoice_data(&invoice_xml).map_err(|e| {
        DrainPerInvoiceError::Application(format!(
            "NAV InvoiceData v3.0 invariant check (ADR-0022) failed for {}: {e}",
            xml_path
        ))
    })?;

    // d. NAV prepare: tokenExchange + build_request. NO wire send yet.
    //    PR-19 / ADR-0032 §1.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            DrainPerInvoiceError::Application(format!(
                "build tokio current-thread runtime for drain NAV calls: {e}"
            ))
        })?;
    let prepared = runtime
        .block_on(prepare_for_attempt_audit(
            nav_endpoint,
            credentials,
            tax_number_8,
            &invoice_xml,
        ))
        .map_err(classify_nav_error)?;

    // e. TX1 — Attempt-before-call.
    let mut conn = Connection::open(db_path).map_err(|e| {
        DrainPerInvoiceError::Application(format!(
            "open tenant DuckDB at {} for drain TX1 audit-write: {e}",
            db_path.display()
        ))
    })?;
    write_attempt_audit(
        &mut conn,
        ledger_meta,
        actor.clone(),
        &invoice.invoice_id,
        invoice.idempotency_key,
        endpoint_audit_label,
        prepared.request_xml.clone(),
    )
    .map_err(|e| DrainPerInvoiceError::Application(format!("{e:#}")))?;
    drop(conn);
    // Sync mirror for TX1.
    {
        let ledger_tx1 = Ledger::open(db_path, tenant.clone(), binary_hash_bytes).map_err(|e| {
            DrainPerInvoiceError::Application(format!(
                "re-open audit ledger after drain TX1 commit for invoice {}: {e}",
                invoice.invoice_id
            ))
        })?;
        let mirror_path = audit_ledger::mirror_path_for(db_path);
        ledger_tx1.sync_mirror(&mirror_path).map_err(|e| {
            DrainPerInvoiceError::Application(format!(
                "sync audit-ledger mirror after drain TX1 commit for invoice {}: {e}",
                invoice.invoice_id
            ))
        })?;
    }
    tracing::info!(
        invoice_id = %invoice.invoice_id,
        "drain TX1 Attempt audit committed; sending manageInvoice"
    );

    // f. Wire send.
    let wire_result = runtime.block_on(manage_invoice::send_built_request(
        &prepared.transport,
        &prepared.request_xml,
    ));

    // g. TX2 — Response on success, AttemptFailed on failure.
    let mut conn = Connection::open(db_path).map_err(|e| {
        DrainPerInvoiceError::Application(format!(
            "open tenant DuckDB at {} for drain TX2 audit-write: {e}",
            db_path.display()
        ))
    })?;
    match wire_result {
        Ok(send_outcome) => {
            write_response_audit(
                &mut conn,
                ledger_meta,
                actor,
                &invoice.invoice_id,
                invoice.idempotency_key,
                &send_outcome.transaction_id,
                send_outcome.response_xml,
            )
            .map_err(|e| DrainPerInvoiceError::Application(format!("{e:#}")))?;
            drop(conn);
            let ledger = Ledger::open(db_path, tenant, binary_hash_bytes).map_err(|e| {
                DrainPerInvoiceError::Application(format!(
                    "re-open audit ledger after drain TX2 Response commit for invoice {}: {e}",
                    invoice.invoice_id
                ))
            })?;
            let verified = ledger.verify_chain().map_err(|e| {
                DrainPerInvoiceError::Application(format!(
                    "audit-ledger chain verification failed AFTER drain TX2 Response commit for invoice {}: {e:#}",
                    invoice.invoice_id
                ))
            })?;
            let mirror_path = audit_ledger::mirror_path_for(db_path);
            ledger.sync_mirror(&mirror_path).map_err(|e| {
                DrainPerInvoiceError::Application(format!(
                    "sync audit-ledger mirror after drain TX2 Response commit for invoice {}: {e}",
                    invoice.invoice_id
                ))
            })?;
            tracing::info!(
                invoice_id = %invoice.invoice_id,
                transaction_id = %send_outcome.transaction_id,
                "NAV manageInvoice OK (drain)"
            );
            println!(
                "drain: invoice {} -> NAV transactionId {} (audit chain verified across {} entries)",
                invoice.invoice_id, send_outcome.transaction_id, verified
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
                ledger_meta,
                actor,
                &invoice.invoice_id,
                invoice.idempotency_key,
                endpoint_audit_label,
                error_class,
                error_code,
                error_message.clone(),
                response_xml,
            )
            .map_err(|e| DrainPerInvoiceError::Application(format!("{e:#}")))?;
            drop(conn);
            let ledger = Ledger::open(db_path, tenant, binary_hash_bytes).map_err(|e| {
                DrainPerInvoiceError::Application(format!(
                    "re-open audit ledger after drain TX2 AttemptFailed commit for invoice {}: {e}",
                    invoice.invoice_id
                ))
            })?;
            let _ = ledger.verify_chain().map_err(|e| {
                DrainPerInvoiceError::Application(format!(
                    "audit-ledger chain verification failed AFTER drain TX2 AttemptFailed commit for invoice {}: {e:#}",
                    invoice.invoice_id
                ))
            })?;
            let mirror_path = audit_ledger::mirror_path_for(db_path);
            ledger.sync_mirror(&mirror_path).map_err(|e| {
                DrainPerInvoiceError::Application(format!(
                    "sync audit-ledger mirror after drain TX2 AttemptFailed commit for invoice {}: {e}",
                    invoice.invoice_id
                ))
            })?;
            // Now classify the wire error into the drain's loop fork
            // (transport → break, application → continue). The audit
            // entry has been written either way per ADR-0032 §1.
            Err(classify_nav_error(wire_err))
        }
    }
}

/// PR-19 / ADR-0032 §1: open transport, tokenExchange, build envelope.
/// Mirror of `submit_invoice::prepare_for_attempt_audit` per the
/// operator-facing-twin posture (CLAUDE.md rule 2 — neither extracted
/// nor speculatively shared until a third caller appears with the same
/// shape; the two are kept aligned by inspection).
async fn prepare_for_attempt_audit(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    invoice_xml: &[u8],
) -> Result<PreparedSubmission, NavTransportError> {
    let transport = NavTransport::new(endpoint)?;
    let token = token_exchange::call(&transport, credentials, tax_number_8).await?;
    let operation = detect_operation_from_xml(invoice_xml);
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

/// PR-19 / ADR-0032 §1: the drain prepare-for-attempt-audit bundle.
/// Mirror of `submit_invoice::PreparedSubmission` per the operator-
/// facing-twin posture.
struct PreparedSubmission {
    transport: NavTransport,
    request_xml: Vec<u8>,
}

/// Translate a `NavTransportError` into the drain's fork choice.
/// Centralised here so the run loop's match is one-armed against
/// the typed result.
fn classify_nav_error(err: NavTransportError) -> DrainPerInvoiceError {
    let msg = format!("{err}");
    if submission_queue::is_transport_error(&err) {
        DrainPerInvoiceError::Transport(msg)
    } else {
        DrainPerInvoiceError::Application(msg)
    }
}

// ──────────────────────────────────────────────────────────────────────
// XML-path resolution + override-map parsing
// ──────────────────────────────────────────────────────────────────────

/// Resolve the on-disk XML path for `invoice`. Precedence:
///
///   1. Per-invocation override map (`--xml-path-override` flags).
///   2. Recorded `nav_xml_path` on the audit payload (PR-18+).
///   3. Loud-fail per CLAUDE.md rule 12.
///
/// Returning a `String` (not `PathBuf`) keeps the eventual operator-
/// visible error message stable across platforms.
fn resolve_xml_path(
    invoice: &PendingInvoice,
    override_map: &HashMap<String, String>,
) -> Result<String> {
    if let Some(p) = override_map.get(&invoice.invoice_id) {
        return Ok(p.clone());
    }
    if let Some(p) = invoice.nav_xml_path.as_deref() {
        return Ok(p.to_string());
    }
    Err(anyhow!(
        "no NAV XML path available for invoice {}: \
         the audit payload's nav_xml_path is None (this invoice was issued by a pre-PR-18 binary) \
         and no --xml-path-override {}=<path> was supplied. \
         Re-run with --xml-path-override {}=<path> pointing at the operator-saved \
         InvoiceData XML for this invoice.",
        invoice.invoice_id,
        invoice.invoice_id,
        invoice.invoice_id
    ))
}

/// Parse the repeated `--xml-path-override <invoice-id>=<path>`
/// CLI flags into a `HashMap`. Loud-fail per CLAUDE.md rule 12 on
/// malformed entries (missing `=`, empty key, empty value) and on
/// duplicate invoice ids in the same invocation (operator typo
/// surface — "alice meant to override inv_X but typed inv_Y twice"
/// is detected here rather than silently dropping one mapping).
fn parse_xml_path_overrides(raw: &[String]) -> Result<HashMap<String, String>> {
    let mut map: HashMap<String, String> = HashMap::new();
    for entry in raw {
        let (key, value) = entry.split_once('=').ok_or_else(|| {
            anyhow!(
                "--xml-path-override '{}' is malformed: expected <invoice-id>=<path>",
                entry
            )
        })?;
        if key.is_empty() {
            return Err(anyhow!(
                "--xml-path-override '{}' has an empty invoice id (left of '=')",
                entry
            ));
        }
        if value.is_empty() {
            return Err(anyhow!(
                "--xml-path-override '{}' has an empty path (right of '=')",
                entry
            ));
        }
        if map.contains_key(key) {
            return Err(anyhow!(
                "--xml-path-override invoice id '{}' appears twice in the same invocation; \
                 only one path per invoice id is permitted",
                key
            ));
        }
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

// ──────────────────────────────────────────────────────────────────────
// Alert thresholds
// ──────────────────────────────────────────────────────────────────────

/// Surface ADR-0031 §6 alert thresholds at the start of the drain
/// loop. WARN-only; no control flow.
fn surface_alert_thresholds(pending: &[PendingInvoice]) {
    if pending.len() >= ALERT_PENDING_COUNT {
        tracing::warn!(
            threshold = "count",
            count = pending.len(),
            limit = ALERT_PENDING_COUNT,
            "drain-submission-queue: pending count at or above the ADR-0009 §7 alert threshold"
        );
        eprintln!(
            "drain-submission-queue: WARN: {} invoice(s) pending (threshold {})",
            pending.len(),
            ALERT_PENDING_COUNT
        );
    }
    if let Some(oldest) = pending.first() {
        let now = OffsetDateTime::now_utc();
        let age_signed = now - oldest.issue_date;
        // The oldest entry's age can be negative if the system clock
        // jumped backwards between issuance and drain; treat negative
        // ages as "not yet beyond threshold" rather than panicking.
        if let Ok(age) = std::time::Duration::try_from(age_signed) {
            if age >= ALERT_OLDEST_PENDING {
                tracing::warn!(
                    threshold = "age",
                    oldest_invoice_id = %oldest.invoice_id,
                    age_seconds = age.as_secs(),
                    limit_seconds = ALERT_OLDEST_PENDING.as_secs(),
                    "drain-submission-queue: oldest pending invoice age at or above the ADR-0009 §7 alert threshold"
                );
                eprintln!(
                    "drain-submission-queue: WARN: oldest pending invoice {} is {} seconds old (threshold {} seconds)",
                    oldest.invoice_id,
                    age.as_secs(),
                    ALERT_OLDEST_PENDING.as_secs()
                );
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// NAV operation classifier + audit-write helpers
// ──────────────────────────────────────────────────────────────────────

/// Three-way classifier on the body shape. Mirror of
/// `submit_invoice::detect_operation_from_xml` per ADR-0024 §3.
/// Non-UTF-8 bodies default to `Create` (the safe direction —
/// detection failure should not cause NAV to receive an
/// inconsistent `<operation>`; UTF-8 is enforced by the prior
/// `validate_invoice_data` step).
fn detect_operation_from_xml(xml: &[u8]) -> InvoiceOperation {
    let body = match std::str::from_utf8(xml) {
        Ok(s) => s,
        Err(_) => return InvoiceOperation::Create,
    };
    if !body.contains("<invoiceReference>") {
        return InvoiceOperation::Create;
    }
    if body.contains("<modificationIssueDate>") {
        InvoiceOperation::Modify
    } else {
        InvoiceOperation::Storno
    }
}

/// PR-19 / ADR-0032 §1: TX1 audit-write — open one audit tx, append
/// the `InvoiceSubmissionAttempt` entry, commit. Mirror of
/// `submit_invoice::write_attempt_audit` per the operator-facing-twin
/// posture.
fn write_attempt_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice_id: &str,
    idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    request_xml: Vec<u8>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for drain TX1 Attempt")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (drain TX1 Attempt audit append)")?;
    let idem_str = idempotency_key.to_canonical_string();
    let attempt = audit_payloads::InvoiceSubmissionAttemptPayload::new(
        invoice_id,
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
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttempt (drain TX1)")?;
    tx.commit()
        .context("commit DuckDB transaction (drain TX1 Attempt audit append)")?;
    Ok(())
}

/// PR-19 / ADR-0032 §1: TX2 success audit-write — open one audit tx,
/// append the `InvoiceSubmissionResponse` entry, commit. Mirror of
/// `submit_invoice::write_response_audit`.
fn write_response_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice_id: &str,
    idempotency_key: IdempotencyKey,
    transaction_id: &str,
    response_xml: Vec<u8>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for drain TX2 Response")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (drain TX2 Response audit append)")?;
    let idem_str = idempotency_key.to_canonical_string();
    let response = audit_payloads::InvoiceSubmissionResponsePayload::new(
        invoice_id,
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
    .context("audit_ledger::append_in_tx InvoiceSubmissionResponse (drain TX2)")?;
    tx.commit()
        .context("commit DuckDB transaction (drain TX2 Response audit append)")?;
    Ok(())
}

/// PR-19 / ADR-0032 §1 + §2: TX2 failure audit-write — open one audit
/// tx, append the `InvoiceSubmissionAttemptFailed` entry, commit.
/// Mirror of `submit_invoice::write_attempt_failed_audit`.
#[allow(clippy::too_many_arguments)]
fn write_attempt_failed_audit(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice_id: &str,
    idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    error_class: &'static str,
    error_code: Option<String>,
    error_message: String,
    response_xml: Option<Vec<u8>>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for drain TX2 AttemptFailed")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (drain TX2 AttemptFailed audit append)")?;
    let idem_str = idempotency_key.to_canonical_string();
    let failed = audit_payloads::InvoiceSubmissionAttemptFailedPayload::new(
        invoice_id,
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
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttemptFailed (drain TX2)")?;
    tx.commit()
        .context("commit DuckDB transaction (drain TX2 AttemptFailed audit append)")?;
    Ok(())
}

/// 8-digit base of a Hungarian tax number. Mirror of
/// `submit_invoice::parse_tax_number_8`. Duplicated for the same
/// operator-facing-twin reason `retry_submission` /
/// `submit_annulment` / `poll_ack` duplicate it.
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

    /// `--xml-path-override inv_A=/tmp/A.xml` parses to a one-entry
    /// map.
    #[test]
    fn parse_xml_path_overrides_accepts_single_pair() {
        let raw = vec!["inv_A=/tmp/A.xml".to_string()];
        let map = parse_xml_path_overrides(&raw).unwrap();
        assert_eq!(map.get("inv_A").map(String::as_str), Some("/tmp/A.xml"));
    }

    /// Two distinct invoice ids parse to a two-entry map.
    #[test]
    fn parse_xml_path_overrides_accepts_two_distinct() {
        let raw = vec![
            "inv_A=/tmp/A.xml".to_string(),
            "inv_B=/tmp/B.xml".to_string(),
        ];
        let map = parse_xml_path_overrides(&raw).unwrap();
        assert_eq!(map.len(), 2);
    }

    /// Missing `=` is rejected loud.
    #[test]
    fn parse_xml_path_overrides_rejects_missing_equals() {
        let raw = vec!["inv_A/tmp/A.xml".to_string()];
        let err = parse_xml_path_overrides(&raw).unwrap_err();
        assert!(err.to_string().contains("malformed"));
    }

    /// Empty key (`=/tmp/A.xml`) is rejected loud.
    #[test]
    fn parse_xml_path_overrides_rejects_empty_key() {
        let raw = vec!["=/tmp/A.xml".to_string()];
        let err = parse_xml_path_overrides(&raw).unwrap_err();
        assert!(err.to_string().contains("empty invoice id"));
    }

    /// Empty value (`inv_A=`) is rejected loud. Load-bearing per
    /// CLAUDE.md rule 12: a silent-accept of empty path would cause
    /// `std::fs::read("")` to fail later with a confusing message.
    #[test]
    fn parse_xml_path_overrides_rejects_empty_value() {
        let raw = vec!["inv_A=".to_string()];
        let err = parse_xml_path_overrides(&raw).unwrap_err();
        assert!(err.to_string().contains("empty path"));
    }

    /// Duplicate invoice ids are rejected loud — operator typo
    /// surface per CLAUDE.md rule 12.
    #[test]
    fn parse_xml_path_overrides_rejects_duplicate_invoice_id() {
        let raw = vec![
            "inv_A=/tmp/A.xml".to_string(),
            "inv_A=/tmp/A-other.xml".to_string(),
        ];
        let err = parse_xml_path_overrides(&raw).unwrap_err();
        assert!(err.to_string().contains("appears twice"));
    }

    /// Path resolution prefers the override map over the recorded
    /// payload path. Pins ADR-0031 §3's precedence rule.
    #[test]
    fn resolve_xml_path_prefers_override_over_payload_recorded() {
        let invoice = PendingInvoice {
            invoice_id: "inv_A".to_string(),
            idempotency_key: IdempotencyKey::new(),
            nav_xml_path: Some("/payload/recorded.xml".to_string()),
            issue_date: OffsetDateTime::now_utc(),
        };
        let mut map = HashMap::new();
        map.insert("inv_A".to_string(), "/operator/override.xml".to_string());
        let resolved = resolve_xml_path(&invoice, &map).unwrap();
        assert_eq!(resolved, "/operator/override.xml");
    }

    /// Path resolution falls back to the payload recorded path when
    /// no override is supplied.
    #[test]
    fn resolve_xml_path_falls_back_to_payload_recorded() {
        let invoice = PendingInvoice {
            invoice_id: "inv_A".to_string(),
            idempotency_key: IdempotencyKey::new(),
            nav_xml_path: Some("/payload/recorded.xml".to_string()),
            issue_date: OffsetDateTime::now_utc(),
        };
        let map: HashMap<String, String> = HashMap::new();
        let resolved = resolve_xml_path(&invoice, &map).unwrap();
        assert_eq!(resolved, "/payload/recorded.xml");
    }

    /// Path resolution loud-fails when both the payload-recorded
    /// path and the override map are absent. Pins ADR-0031 §3's
    /// loud-fail-third-arm rule + the operator-visible message
    /// naming `--xml-path-override` as the recovery flag (per
    /// CLAUDE.md rule 12).
    #[test]
    fn resolve_xml_path_loud_fails_when_both_sources_absent() {
        let invoice = PendingInvoice {
            invoice_id: "inv_A".to_string(),
            idempotency_key: IdempotencyKey::new(),
            nav_xml_path: None,
            issue_date: OffsetDateTime::now_utc(),
        };
        let map: HashMap<String, String> = HashMap::new();
        let err = resolve_xml_path(&invoice, &map).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--xml-path-override"));
        assert!(msg.contains("inv_A"));
    }

    /// Tax-number parser mirrors the existing operator-facing-twin
    /// shape. Same contract as
    /// `submit_invoice::parse_tax_number_8` /
    /// `retry_submission::parse_tax_number_8` /
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

    /// Operation detection mirrors `submit_invoice`'s three-way
    /// classifier byte-for-byte. The drain's detector must agree
    /// with submit-invoice's so the same on-disk XML produces the
    /// same `<operation>` regardless of which command POSTs it.
    #[test]
    fn detect_operation_create_on_plain_invoice() {
        let xml = b"<?xml version=\"1.0\"?>\
            <InvoiceData><invoiceNumber>X/00001</invoiceNumber>\
            <invoiceMain><invoice><invoiceHead/></invoice></invoiceMain></InvoiceData>";
        assert_eq!(detect_operation_from_xml(xml), InvoiceOperation::Create);
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
        assert_eq!(detect_operation_from_xml(xml), InvoiceOperation::Storno);
    }

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
        assert_eq!(detect_operation_from_xml(xml), InvoiceOperation::Modify);
    }
}
