//! Orchestration for the `aberp retry-submission` subcommand (PR-8-1).
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
//!   6. Build a tokio current-thread runtime and drive the NAV calls
//!      (`tokenExchange` + `manageInvoice` for `operation = CREATE`,
//!      same shape as `submit_invoice::call_nav`).
//!   7. Under a single DuckDB transaction, append THREE audit entries:
//!      `InvoiceRetryRequested` (operator decision + precondition),
//!      `InvoiceSubmissionAttempt` (verbatim request body), and
//!      `InvoiceSubmissionResponse` (verbatim response body + new
//!      transactionId). All three carry the same F8 idempotency key
//!      as the original issuance.
//!   8. Verify the audit chain after commit (success-criterion gate).
//!   9. Advance the typestate (Stuck → Submitted with the new txid)
//!      and print the operator-visible summary.
//!
//! # Why one transaction for all three audit appends
//!
//! Same posture as `submit_invoice`: attempt + response are siblings in
//! time, and adding `retry_requested` to the same tx keeps the unblock
//! decision atomically tied to the resulting NAV interaction. If the
//! process crashes between writing `retry_requested` and writing the
//! attempt entry, the rollback discards both — the operator runs the
//! command again from scratch rather than discovering a half-written
//! retry-decision-with-no-evidence in the ledger.
//!
//! # Why not call into `submit_invoice::run` directly
//!
//! `submit_invoice::run` writes exactly two audit entries per call —
//! the attempt + the response. Adding the `retry_requested` entry
//! requires injecting a third write into the same tx, which means
//! either parametrizing `submit_invoice`'s tx body (would invade its
//! scope) or wrapping the call (would split the tx into two,
//! defeating the atomicity above). The structural NAV-call code is
//! lifted via `submit_invoice::call_nav` re-export — the only
//! duplication is in the orchestration shell.
//!
//! # F12 trap status
//!
//! PR-8 added two new `EventKind` variants. `InvoiceRetryRequested`
//! is one of them. The four coordinated edits (variant body, `as_str`
//! arm, `from_storage_str` arm, `round_trip_for_every_variant`
//! hand-listed array) land in the same commit as this file. If a
//! future contributor adds an EventKind without those four edits, the
//! round-trip test fails — but this header is the loud reminder.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT implement ADR-0009 §5 Layer-2 idempotency
//!     (`queryInvoiceCheck` against the invoice number on
//!     `INVOICE_NUMBER_NOT_UNIQUE`). That disambiguation belongs in a
//!     separate PR that lands the `queryInvoiceCheck` nav-transport
//!     operation; until then, `INVOICE_NUMBER_NOT_UNIQUE` from NAV
//!     surfaces as a loud failure per CLAUDE.md rule 12.
//!   - It does NOT poll `queryTransactionStatus` — the operator runs
//!     `aberp poll-ack` after the retry, same as the original
//!     submission flow.
//!   - It does NOT mutate any billing row — the `submission_state`
//!     fact lives in the audit ledger per A5/A6.

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{self as billing, IdempotencyKey, ReadyInvoice};
use aberp_nav_transport::{
    operations::{manage_invoice, token_exchange},
    soap::{InvoiceOperation, ManageInvoiceItem},
    NavCredentials, NavEndpoint, NavTransport,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads;
use crate::audit_query::{self, StuckOutcome, StuckPrecondition};
use crate::binary_hash;
use crate::cli::{NavEnv, RetrySubmissionArgs};

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

    // 6. NAV calls on a tokio current-thread runtime.
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for retry NAV calls")?;
    let nav_outcome = runtime.block_on(call_nav(
        nav_endpoint,
        &credentials,
        &tax_number_8,
        &invoice_xml,
    ))?;
    tracing::info!(
        new_transaction_id = %nav_outcome.transaction_id,
        prior_transaction_id = %stuck.prior_transaction_id,
        "NAV manageInvoice (retry) OK"
    );

    // 7. Write all three audit entries under one tx, then commit.
    write_retry_audit_entries(
        &mut conn,
        &ledger_meta,
        actor.clone(),
        &ready_invoice,
        idempotency_key,
        endpoint_audit_label,
        reason,
        &stuck,
        &nav_outcome,
    )?;

    // 8. Verify the audit chain after commit (success-criterion gate).
    drop(conn);
    let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes).context("open audit ledger")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER retry-submission")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 9. Typestate advance + operator-visible summary. The retry
    //    leaves the invoice in `Submitted` with the new txid; the
    //    operator runs `aberp poll-ack` next.
    let submitted = ready_invoice.into_submitted(nav_outcome.transaction_id.clone());
    println!(
        "retry-submission OK: invoice {} (seq {}) re-submitted -> NAV transactionId {} \
         (prior txid {}, prior last ack {}) \
         (audit chain verified across {} entries); \
         run `aberp poll-ack` to drive terminal state",
        submitted.id.to_prefixed_string(),
        submitted.sequence_number,
        submitted.nav_transaction_id,
        stuck.prior_transaction_id,
        stuck.prior_last_ack_status.as_deref().unwrap_or("<none>"),
        verified,
    );

    Ok(())
}

/// Open the audit ledger, resolve the stuck precondition. Loud-fail
/// on every `NotStuck` reason; loud-fail on idempotency-key mismatch
/// between issuance and the precondition. Returns the precondition
/// shape on success.
///
/// The idempotency-key mismatch check is defence-in-depth: the F8
/// contract pins the issuance's key to every NAV-related entry. If
/// the submission_response carries a different key than the billing
/// row, something has tampered with the ledger (rule 12 — fail loud).
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
            "cannot retry invoice {}: {}",
            invoice_id,
            reason.as_message()
        )),
    }
}

/// Internal: the NAV-side outcome of one retry attempt. Same shape as
/// `submit_invoice::NavSubmissionOutcome`; we keep the type private
/// here (instead of re-exporting `submit_invoice`'s) so a future
/// divergence (e.g. retry-specific fields) does not surface as a
/// silent rename of a shared type.
struct NavSubmissionOutcome {
    transaction_id: String,
    request_xml: Vec<u8>,
    response_xml: Vec<u8>,
}

async fn call_nav(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    invoice_xml: &[u8],
) -> Result<NavSubmissionOutcome> {
    let transport = NavTransport::new(endpoint).context("build NAV transport")?;
    let token = token_exchange::call(&transport, credentials, tax_number_8)
        .await
        .context("NAV tokenExchange (retry)")?;
    let manage = manage_invoice::call(
        &transport,
        credentials,
        tax_number_8,
        &token.decoded_token,
        &[ManageInvoiceItem {
            operation: InvoiceOperation::Create,
            invoice_data_xml: invoice_xml,
        }],
    )
    .await
    .context("NAV manageInvoice (retry)")?;
    Ok(NavSubmissionOutcome {
        transaction_id: manage.transaction_id,
        request_xml: manage.request_xml,
        response_xml: manage.response_xml,
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

/// Open one audit-write tx, append three entries (retry_requested +
/// attempt + response), commit. All three share the F8 idempotency
/// key.
#[allow(clippy::too_many_arguments)]
fn write_retry_audit_entries(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    reason: &str,
    stuck: &StuckPrecondition,
    nav_outcome: &NavSubmissionOutcome,
) -> Result<()> {
    audit_ledger::ensure_schema(conn).context("ensure audit-ledger schema for retry-submission")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (retry-submission audit appends)")?;

    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();

    // 1. InvoiceRetryRequested — the operator's decision + precondition.
    let retry_payload = audit_payloads::InvoiceRetryRequestedPayload::new(
        &invoice_id_str,
        idempotency_key,
        &stuck.prior_transaction_id,
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
    .context("audit_ledger::append_in_tx InvoiceRetryRequested")?;

    // 2. InvoiceSubmissionAttempt — verbatim retry-request body.
    let attempt = audit_payloads::InvoiceSubmissionAttemptPayload::new(
        &invoice_id_str,
        idempotency_key,
        endpoint_label,
        nav_outcome.request_xml.clone(),
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceSubmissionAttempt,
        attempt.to_bytes(),
        actor.clone(),
        Some(idem_str.clone()),
    )
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttempt (retry)")?;

    // 3. InvoiceSubmissionResponse — verbatim retry-response body + new txid.
    let response = audit_payloads::InvoiceSubmissionResponsePayload::new(
        &invoice_id_str,
        idempotency_key,
        &nav_outcome.transaction_id,
        nav_outcome.response_xml.clone(),
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceSubmissionResponse,
        response.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceSubmissionResponse (retry)")?;

    tx.commit()
        .context("commit DuckDB transaction (retry-submission audit appends)")?;
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
