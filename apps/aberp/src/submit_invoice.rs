//! Orchestration for the `aberp submit-invoice` subcommand (PR-7-B-3).
//!
//! Pipeline:
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
//!   6. Open a tokio current-thread runtime; on it:
//!      - tokenExchange against the chosen NAV endpoint.
//!      - manageInvoice with the decrypted token + the XML body.
//!   7. Under a single DuckDB transaction:
//!      - Append `InvoiceSubmissionAttempt` (request bytes).
//!      - Append `InvoiceSubmissionResponse` (response bytes +
//!        NAV transaction id).
//!      - Commit.
//!   8. Verify the audit chain after commit (success-criterion gate).
//!   9. Print the typestate transition + transaction id.
//!
//! # Why two audit appends instead of one
//!
//! ADR-0009 §8 names two distinct events:
//! `invoice.submission_attempt` (the request) and
//! `invoice.submission_response` (the response). Splitting them gives
//! the audit-evidence bundle a coherent trace of "we tried" vs "we
//! succeeded"; collapsing them would hide the case where a future PR
//! adds retries between attempt and response (the retry loop lands in
//! PR-7-C).
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
use crate::binary_hash;
use crate::cli::{NavEnv, SubmitInvoiceArgs};

pub fn run(args: &SubmitInvoiceArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "submit_invoice",
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

    // 2. Load NAV credentials BEFORE touching the DB — missing creds
    //    leave the DB pristine instead of writing half a transaction
    //    and rolling back.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for this CLI invocation"
    );

    // 3. Read the NAV InvoiceData XML bytes.
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

    // 4. Load the previously-issued invoice + its idempotency_key.
    //    Scoped read tx so the connection is free for the audit-write
    //    tx below.
    let mut conn = Connection::open(&args.db)
        .with_context(|| format!("open tenant DuckDB at {}", args.db.display()))?;
    let (ready_invoice, idempotency_key) = load_issued_invoice(&mut conn, &args.invoice_id)?;
    if ready_invoice.id.to_prefixed_string() != args.invoice_id {
        // Defence-in-depth: the loader keys off the same string we
        // passed in, but if the DB round-trip ever produces a
        // different prefix the F8 contract is broken. Loud per CLAUDE.md
        // rule 12.
        return Err(anyhow!(
            "loaded invoice id {} does not match requested {}",
            ready_invoice.id.to_prefixed_string(),
            args.invoice_id
        ));
    }
    tracing::info!(
        seq = ready_invoice.sequence_number,
        idempotency_key = %idempotency_key.to_canonical_string(),
        "issued invoice loaded for submission"
    );

    // 5. Build ledger meta.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 6. NAV calls on a tokio current-thread runtime. Build the
    //    runtime AFTER the credentials + invoice are validated so we
    //    do not pay the runtime-startup cost on a malformed input.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for NAV calls")?;
    let nav_outcome = runtime.block_on(call_nav(
        nav_endpoint,
        &credentials,
        &tax_number_8,
        &invoice_xml,
    ))?;
    tracing::info!(
        transaction_id = %nav_outcome.transaction_id,
        "NAV manageInvoice OK"
    );

    // 7. Write both audit entries under one tx, then commit.
    write_submission_audit_entries(
        &mut conn,
        &ledger_meta,
        actor.clone(),
        &ready_invoice,
        idempotency_key,
        endpoint_audit_label,
        &nav_outcome,
    )?;

    // 8. Verify the audit chain after commit (success-criterion gate).
    //    Drop the tx-Connection first; re-open a fresh Ledger.
    drop(conn);
    let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes).context("open audit ledger")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER submission")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 9. Typestate advance + operator-visible summary.
    let submitted = ready_invoice.into_submitted(nav_outcome.transaction_id.clone());
    println!(
        "submitted invoice {} (seq {}) -> NAV transactionId {} \
         (audit chain verified across {} entries)",
        submitted.id.to_prefixed_string(),
        submitted.sequence_number,
        submitted.nav_transaction_id,
        verified,
    );

    Ok(())
}

/// Internal: the NAV-side outcome bundle. Holds everything the audit
/// writes need so the audit-write function has a single typed input.
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

    // 6a. tokenExchange → decrypted token + verbatim bytes for audit
    //     (PR-7-B-3 does NOT write a separate audit entry for the
    //     tokenExchange round-trip; ADR-0009 §8 names the submission
    //     attempt + response as the load-bearing audit pair. A future
    //     PR may add `invoice.token_exchange` if the audit-evidence
    //     bundle wants it).
    let token = token_exchange::call(&transport, credentials, tax_number_8)
        .await
        .context("NAV tokenExchange")?;

    // 6b. manageInvoice with the decrypted token.
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
    .context("NAV manageInvoice")?;

    Ok(NavSubmissionOutcome {
        transaction_id: manage.transaction_id,
        request_xml: manage.request_xml,
        response_xml: manage.response_xml,
    })
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

/// Open one audit-write tx, append the two PR-7-B-3 entries, commit.
/// Both entries carry the same `idempotency_key` per F8.
fn write_submission_audit_entries(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    nav_outcome: &NavSubmissionOutcome,
) -> Result<()> {
    audit_ledger::ensure_schema(conn).context("ensure audit-ledger schema for submit-invoice")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (submit-invoice audit appends)")?;

    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();

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
    .context("audit_ledger::append_in_tx InvoiceSubmissionAttempt")?;

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
    .context("audit_ledger::append_in_tx InvoiceSubmissionResponse")?;

    tx.commit()
        .context("commit DuckDB transaction (submit-invoice audit appends)")?;
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
}
