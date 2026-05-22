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
//!   6. Open a tokio current-thread runtime; on it:
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

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{self as billing, IdempotencyKey, ReadyInvoice};
use aberp_nav_transport::{
    operations::{manage_invoice, token_exchange},
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

    // 3a. PR-9-0 / ADR-0022: validate the on-disk XML BEFORE any NAV
    //     call. Catches the case where the file was hand-edited between
    //     `issue-invoice` and `submit-invoice` or where a future emitter
    //     change diverges from the validator's allowlist. Loud-fail per
    //     CLAUDE.md rule 12 — no `tokenExchange` happens, no audit
    //     entries land.
    aberp_nav_xsd_validator::validate_invoice_data(&invoice_xml).with_context(|| {
        format!(
            "NAV InvoiceData v3.0 invariant check (ADR-0022) failed for {}",
            args.invoice_xml.display()
        )
    })?;
    tracing::info!(
        nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
        "on-disk InvoiceData XML passed v3.0 invariant check before NAV submit"
    );

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

    // 6. NAV prepare phase on a tokio current-thread runtime. Build
    //    the runtime AFTER the credentials + invoice are validated so
    //    we do not pay the runtime-startup cost on a malformed input.
    //    The prepare phase performs tokenExchange + envelope build
    //    (no wire send yet — that happens after TX1).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for NAV calls")?;
    let prepared = runtime.block_on(prepare_for_attempt_audit(
        nav_endpoint,
        &credentials,
        &tax_number_8,
        &invoice_xml,
    ))?;
    tracing::info!(
        request_bytes = prepared.request_xml.len(),
        "manageInvoice envelope built; ready to write TX1 Attempt audit"
    );

    // 7. TX1 — Attempt-before-call (ADR-0032 §1). Write the
    //    InvoiceSubmissionAttempt entry BEFORE the wire send so a
    //    transport-mid-flight loss or process crash still leaves the
    //    audit trail pointing at "we tried to submit X with body Y."
    write_attempt_audit(
        &mut conn,
        &ledger_meta,
        actor.clone(),
        &ready_invoice,
        idempotency_key,
        endpoint_audit_label,
        prepared.request_xml.clone(),
    )?;
    // Re-open the Ledger handle and sync the mirror for TX1.
    {
        let ledger_tx1 = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger after TX1 commit")?;
        let mirror_path = audit_ledger::mirror_path_for(&args.db);
        ledger_tx1
            .sync_mirror(&mirror_path)
            .context("sync audit-ledger mirror file after TX1 Attempt commit")?;
    }
    tracing::info!("TX1 Attempt audit committed; mirror synced; sending manageInvoice");

    // 8. Wire send — POST the pre-rendered envelope.
    let wire_result = runtime.block_on(manage_invoice::send_built_request(
        &prepared.transport,
        &prepared.request_xml,
    ));

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
            )?;
            // 10. Verify the audit chain after TX2 commit (success-
            //     criterion gate). Drop the tx-Connection first;
            //     re-open a fresh Ledger.
            drop(conn);
            let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes)
                .context("open audit ledger after TX2 Response commit")?;
            let verified = ledger
                .verify_chain()
                .context("audit-ledger chain verification failed AFTER submission")?;
            tracing::info!(entries_verified = verified, "audit chain verified");
            let mirror_path = audit_ledger::mirror_path_for(&args.db);
            ledger
                .sync_mirror(&mirror_path)
                .context("sync audit-ledger mirror file after TX2 Response commit")?;
            // 11. Typestate advance + operator-visible summary.
            let submitted =
                ready_invoice.into_submitted(send_outcome.transaction_id.clone());
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
        Err(wire_err) => {
            // Audit the failure FIRST per ADR-0032 §1, then surface
            // the wire error to the caller. The TX2 AttemptFailed
            // commit is the load-bearing closure of F40 — it is the
            // evidence that ABERP tried-and-failed, distinct from
            // the silent-on-failure pre-PR-19 path.
            let (error_class, error_code) =
                submission_queue::classify_attempt_failure(&wire_err);
            let error_message = format!("{wire_err}");
            // Capture verbatim response bytes for the audit IFF the
            // failure carried one (HTTP-status + application classes
            // come with a body; transport + envelope + credential
            // + client_build do not). The send_built_request surface
            // does not expose the body alongside the error variant
            // today; record `None` for now and let a future PR widen
            // the surface (F47-class trigger — not yet filed).
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
                "submit-invoice: manageInvoice failed; TX2 AttemptFailed audit written"
            );
            eprintln!(
                "submit-invoice FAILED for invoice {}: {} \
                 (audit chain verified across {} entries; \
                 InvoiceSubmissionAttemptFailed recorded with error_class={}); \
                 invoice is now in state-2 Pending — re-run `aberp retry-submission` \
                 to retry (note: a state-2 retry may produce a duplicate submission \
                 to NAV until Layer-2 queryInvoiceCheck per ADR-0009 §5 lands; F44)",
                ready_invoice.id.to_prefixed_string(),
                error_message,
                verified,
                error_class,
            );
            Err(anyhow!(
                "submit-invoice manageInvoice failed: {}",
                error_message
            ))
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
) -> Result<PreparedSubmission> {
    let transport = NavTransport::new(endpoint).context("build NAV transport")?;
    let token = token_exchange::call(&transport, credentials, tax_number_8)
        .await
        .context("NAV tokenExchange")?;
    // The per-invoice `operation` is detected from the XML body's
    // shape via the three-way classifier (CREATE / STORNO / MODIFY).
    // PR-11 / ADR-0024 §3 closed F22 by extending PR-10's two-way
    // classifier with the `<modificationIssueDate>` disambiguator
    // for MODIFY.
    let operation = detect_operation_from_xml(invoice_xml)?;
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
        anyhow!("manage_invoice::build_request (envelope construction) failed: {e}")
    })?;
    Ok(PreparedSubmission {
        transport,
        request_xml,
    })
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
    let body = std::str::from_utf8(xml).context(
        "invoice XML is not valid UTF-8 — NAV requires UTF-8 per the v3.0 schema",
    )?;
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
    tx.commit().context(
        "commit DuckDB transaction (submit-invoice TX2 AttemptFailed audit append)",
    )?;
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
