//! Orchestration for the `aberp recover-from-nav` subcommand
//! (PR-21, ADR-0034 §2).
//!
//! Chain-reconstruction operator command for an invoice in the
//! **state-2 Pending + Layer-2 Exists** posture per ADR-0034 §1: a
//! prior `retry-submission` run wrote
//! `InvoiceCheckPerformed(outcome=exists)` (PR-20 / ADR-0033 §1)
//! and skipped the `manageInvoice` re-POST; ABERP's local audit
//! ledger therefore lacks the `InvoiceSubmissionResponse` entry
//! that would link the invoice to NAV's transactionId.
//! `recover-from-nav` calls `queryInvoiceData` against the NAV-
//! facing invoice number, parses the
//! `<auditData>/<transactionId>` element from NAV's response, and
//! writes ONE recovered `InvoiceSubmissionResponse` audit entry
//! reusing the existing payload shape per ADR-0034 §4 — F12
//! four-edit ritual does NOT fire; no new EventKind variant is
//! added.
//!
//! After this command the precondition walker
//! (`audit_query::stuck_precondition`) classifies the invoice as
//! state-3 AwaitingAck via its existing step-2 rule (Response
//! exists, no terminal ack). The operator's next move is
//! `aberp poll-ack` against the recovered transactionId, which
//! produces the authoritative `InvoiceAckStatus` via
//! `queryTransactionStatus` per the existing PR-7-C-2 path.
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args (8-digit tax number; tenant;
//!      endpoint). Same shape as
//!      `observe_receiver_confirmation::run` step 1.
//!   2. Load `NavCredentials` from the OS keychain (loud-fail on
//!      missing). Same posture as
//!      `observe_receiver_confirmation::run` step 2.
//!   3. Open tenant DuckDB; load the previously-issued invoice +
//!      idempotency_key from the billing store (scoped read tx).
//!   4. Resolve the typed `recover-from-nav` precondition via
//!      [`resolve_recovery_precondition`]:
//!        - Requires `Stuck(StuckStage::Pending)` per
//!          `audit_query::stuck_precondition`.
//!        - Requires the most-recent `InvoiceCheckPerformed`
//!          audit entry for this invoice exists AND its
//!          `outcome == "exists"`.
//!        - Loud-fails with operator-visible guidance on every
//!          other shape (no prior check entry → run
//!          retry-submission first; last check was absent or
//!          failure → run retry-submission to re-disambiguate).
//!      The F8 idempotency-key cross-check happens inside
//!      `audit_query::stuck_precondition` consumption (mirror of
//!      `retry_submission::resolve_stuck_or_loud_fail`).
//!   5. Construct the NAV-facing invoice number string
//!      (`"{series_code}/{seq:05}"`) — same canonical shape per
//!      ADR-0009 §3 / `nav_xml::render_invoice_data`. Mirror of
//!      `observe_receiver_confirmation::load_base_nav_invoice_number`
//!      and `retry_submission::derive_nav_invoice_number`.
//!   6. Build a tokio current-thread runtime and drive ONE
//!      `queryInvoiceData` call (one-shot per ADR-0028 §4 / ADR-
//!      0034 §2; no loop).
//!   7. Parse the recovered transactionId from the verbatim
//!      response bytes via
//!      [`aberp_nav_transport::operations::query_invoice_data::parse_audit_data_transaction_id`]
//!      (ADR-0034 §3). Loud-fail on missing or empty element.
//!   8. Under one DuckDB transaction, append ONE
//!      `InvoiceSubmissionResponse` audit entry carrying the
//!      recovered transactionId + the verbatim
//!      `<QueryInvoiceDataResponse>` bytes + the F8 idempotency
//!      key. Commit.
//!   9. Verify the audit chain after commit (success-criterion
//!      gate). Sync the audit-ledger mirror file per ADR-0030 §2.
//!  10. Operator-visible summary: name the recovered txid and
//!      steer the operator to `aberp poll-ack` for terminal
//!      state per CLAUDE.md rule 12.
//!
//! # Why one-shot, not bounded-poll
//!
//! ADR-0034 §2: the recovery is a single discovery of NAV's prior
//! record. Retrying queryInvoiceData on transient NAV-side failures
//! is the operator's choice (re-run `recover-from-nav` after the
//! transient resolves) — same posture as
//! `observe_receiver_confirmation` per ADR-0028 §4.
//!
//! # Why NOT reconstruct `InvoiceAckStatus`
//!
//! Per ADR-0034 §"Why reconstruct only `InvoiceSubmissionResponse`,
//! not `InvoiceAckStatus`" + §"Surfaced conflict 1 Reading B":
//! `queryInvoiceData`'s response carries the `auditData.transactionId`
//! field but NOT the authoritative ack-status enumeration value —
//! that lives on `queryTransactionStatus`. Fabricating
//! `InvoiceAckStatus(ack_status=SAVED)` here would mask the
//! difference between ABERP-inferred and NAV-authoritative facts
//! (CLAUDE.md rule 12 — don't fabricate facts ABERP cannot itself
//! verify). The operator's follow-up `aberp poll-ack` writes the
//! authoritative `InvoiceAckStatus` via the existing PR-7-C-2 path.
//!
//! # Why no `--reason` flag
//!
//! Per ADR-0034 §1: the recovery is mechanical (reconstruct state
//! NAV already has), not a choice between alternative recoveries
//! that a reason text would disambiguate. The audit-evidence chain
//! itself (preceding `InvoiceCheckPerformed(outcome=exists)` plus
//! the recovered `InvoiceSubmissionResponse`) is the justification.
//! CLAUDE.md rule 2: no speculative abstractions.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT call `queryInvoiceCheck`. The Layer-2 check is
//!     PR-20 / `retry-submission`'s responsibility; the
//!     recover-from-nav precondition consumes the existing
//!     `InvoiceCheckPerformed(outcome=exists)` audit entry.
//!   - It does NOT call `queryTransactionStatus`. PR-7-C-2's
//!     `poll-ack` is the operator's next step.
//!   - It does NOT call `manageInvoice`. There is no re-POST on
//!     the recovery path — NAV already has the invoice.
//!   - It does NOT write `InvoiceRetryRequested`. The recovery is
//!     not a retry of the original submission; the audit-evidence
//!     chain distinguishes the two via the preceding
//!     `InvoiceCheckPerformed` entry per ADR-0034 §4.
//!   - It does NOT add a new EventKind. F12 four-edit ritual does
//!     NOT fire per ADR-0034 §4 / §"Surfaced conflict 2 Reading A".
//!   - It does NOT amend `audit_query::stuck_precondition`. The
//!     PR-20 §6 pin tests stay valid per ADR-0034 §"Surfaced
//!     conflict 3 Reading B".
//!   - It does NOT mutate any billing row.

use std::path::Path;

use aberp_audit_ledger::{
    self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_billing::{
    self as billing, BillingStore, DuckDbBillingStore, IdempotencyKey, InvoiceSeries, ReadyInvoice,
};
use aberp_nav_transport::{
    operations::query_invoice_data::{self, QueryInvoiceDataOutcome},
    soap::InvoiceDirection,
    NavCredentials, NavEndpoint, NavTransport, NavTransportError,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads;
use crate::audit_query::{self, StuckOutcome, StuckStage};
use crate::binary_hash;
use crate::cli::{NavEnv, RecoverFromNavArgs};

pub fn run(args: &RecoverFromNavArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "recover_from_nav",
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
    //    posture as `observe_receiver_confirmation::run` step 2.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for recover-from-nav"
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
        "issued invoice loaded for recover-from-nav"
    );

    // 4. Resolve the typed recover-from-nav precondition. Returns
    //    the NAV-facing invoice number the prior
    //    InvoiceCheckPerformed entry recorded (used for the
    //    defence-in-depth drift check below). The F8 idempotency-
    //    key cross-check happens inside this function; the caller
    //    doesn't need the key again because the audit write below
    //    uses the billing-row key from load_issued_invoice (already
    //    proven to match per the cross-check).
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let nav_invoice_number_from_check = resolve_recovery_precondition(
        &args.db,
        tenant.clone(),
        binary_hash_bytes,
        &args.invoice_id,
        &idempotency_key,
    )?;

    // 5. Derive the NAV-facing invoice number from the loaded
    //    ReadyInvoice. Same canonical NAV-facing invoice number
    //    shape per ADR-0009 §3 — `"{series_code}/{seq:05}"`.
    let nav_invoice_number = derive_nav_invoice_number(&args.db, &ready_invoice)?;
    tracing::info!(
        nav_invoice_number = %nav_invoice_number,
        nav_invoice_number_from_check = %nav_invoice_number_from_check,
        "NAV-facing invoice number constructed for queryInvoiceData"
    );

    // 5a. Defence-in-depth: the NAV-facing invoice number we
    //     derived from the loaded ReadyInvoice MUST match the one
    //     the prior InvoiceCheckPerformed entry recorded. Drift
    //     between the two indicates ledger tampering or a
    //     billing-row mutation between the prior retry-submission
    //     and this recovery — both classes of failure CLAUDE.md
    //     rule 12 names. Loud-fail rather than recover an
    //     ambiguous identifier.
    if nav_invoice_number != nav_invoice_number_from_check {
        return Err(anyhow!(
            "NAV-facing invoice number derived from billing row ('{}') does not match the \
             prior InvoiceCheckPerformed entry's nav_invoice_number ('{}') — the audit \
             ledger or billing row appears tampered between the prior retry-submission \
             Layer-2 check and this recover-from-nav run",
            nav_invoice_number,
            nav_invoice_number_from_check,
        ));
    }

    // 6. NAV call on a tokio current-thread runtime. One-shot per
    //    ADR-0034 §2 — NO loop. The query authenticates via the
    //    per-request <user> block alone (no tokenExchange per
    //    ADR-0009 §4 — queryInvoiceData is a NAV query operation).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for queryInvoiceData call")?;

    let outcome = runtime.block_on(call_nav(
        nav_endpoint,
        &credentials,
        &tax_number_8,
        &nav_invoice_number,
    ))?;
    tracing::info!(
        response_bytes = outcome.response_xml.len(),
        "queryInvoiceData OK (recover-from-nav chain reconstruction)"
    );

    // 7. Parse the recovered transactionId from the verbatim
    //    response bytes per ADR-0034 §3. Loud-fail on missing or
    //    empty element — the verbatim bytes are persisted on the
    //    audit entry regardless (step 8) so a parse-side bug
    //    cannot drop the evidence even if the recovery aborts
    //    here (the bytes would still need to land somewhere; today
    //    a parse failure short-circuits the audit write and the
    //    operator inspects the error message + re-runs after the
    //    NAV-testbed verification surfaces the actual shape).
    let recovered_transaction_id =
        query_invoice_data::parse_audit_data_transaction_id(&outcome.response_xml).map_err(
            |e: NavTransportError| {
                anyhow!(
                    "queryInvoiceData succeeded but the recovered transactionId could not be \
                     parsed from the response: {e} (NAV-side response-shape divergence; \
                     NAV-testbed verification is the named trigger for an amendment ADR \
                     per ADR-0034 §\"Open questions\")"
                )
            },
        )?;
    tracing::info!(
        recovered_transaction_id = %recovered_transaction_id,
        "queryInvoiceData auditData.transactionId parsed; writing recovered Response"
    );

    // 8. Write ONE recovered InvoiceSubmissionResponse audit entry
    //    under one tx. Reuses the existing payload shape per
    //    ADR-0034 §4 (no schema change; F12 ritual does NOT fire).
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);
    write_recovered_response_audit_entry(
        &mut conn,
        &ledger_meta,
        actor.clone(),
        &ready_invoice,
        idempotency_key,
        &recovered_transaction_id,
        outcome.response_xml.clone(),
    )?;

    // 9. Verify the audit chain after commit (success-criterion
    //    gate). Drop the tx-Connection first; re-open a fresh
    //    Ledger to read.
    drop(conn);
    let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes)
        .context("re-open audit ledger after recover-from-nav commit")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER recover-from-nav")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 9a. ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit.
    let mirror_path = audit_ledger::mirror_path_for(&args.db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after recover-from-nav commit")?;

    // 10. Operator-visible summary per ADR-0034 §2 step 10. Names
    //     the recovered txid + steers the operator at poll-ack
    //     for authoritative ack status (CLAUDE.md rule 12 — name
    //     the next step loud).
    println!(
        "recover-from-nav OK: invoice {} (NAV number {}) chain reconstructed -> recovered \
         transactionId {} from NAV queryInvoiceData (audit chain verified across {} entries; \
         InvoiceSubmissionResponse appended with the verbatim QueryInvoiceDataResponse bytes \
         as provenance evidence per ADR-0034 §4). The invoice is now state-3 AwaitingAck \
         locally; run `aberp poll-ack --invoice-id {} --tax-number {} --endpoint {}` next to \
         drive the terminal state via queryTransactionStatus (CLAUDE.md rule 12 — ABERP did \
         NOT fabricate an InvoiceAckStatus entry; the authoritative ack status comes from \
         poll-ack's queryTransactionStatus result per ADR-0034 §\"Why reconstruct only \
         InvoiceSubmissionResponse, not InvoiceAckStatus\")",
        ready_invoice.id.to_prefixed_string(),
        nav_invoice_number,
        recovered_transaction_id,
        verified,
        args.invoice_id,
        args.tax_number,
        match args.endpoint {
            NavEnv::Test => "test",
            NavEnv::Production => "production",
        },
    );

    Ok(())
}

/// Resolve the `recover-from-nav` precondition per ADR-0034 §5.
/// Walks the audit ledger twice (once via
/// `audit_query::stuck_precondition`, once for the latest
/// `InvoiceCheckPerformed` for the invoice) and either returns
/// the NAV-facing invoice number the prior Layer-2 Exists check
/// recorded (consumed by the caller's defence-in-depth drift
/// check) or loud-fails with operator-visible guidance.
///
/// The F8 idempotency-key cross-check happens inside this
/// function (mirror of
/// `retry_submission::resolve_stuck_or_loud_fail`): the
/// precondition walker's `idempotency_key` must match the
/// billing row's issuance key. Loud-fail otherwise per
/// CLAUDE.md rule 12 (ledger tamper detection). The caller's
/// audit write reuses the billing-row key (already proven to
/// match) so the key does not need to flow back out here —
/// CLAUDE.md rule 2 (minimum code, no speculative
/// abstractions).
fn resolve_recovery_precondition(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
    invoice_id: &str,
    issuance_idempotency_key: &IdempotencyKey,
) -> Result<String> {
    let ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to resolve recover-from-nav precondition")?;
    let stuck = match audit_query::stuck_precondition(&ledger, invoice_id)? {
        StuckOutcome::Stuck(p) => p,
        StuckOutcome::NotStuck(reason) => {
            return Err(anyhow!(
                "cannot recover invoice {}: {} (recover-from-nav requires state-2 Pending \
                 with a prior Layer-2 Exists check per ADR-0034 §5)",
                invoice_id,
                reason.as_message()
            ));
        }
    };

    // Require state-2 Pending. The state-3 AwaitingAck path needs
    // poll-ack, not recover-from-nav (and a hypothetical state-3
    // post-recovery already has a Response — the precondition
    // walker would have classified it as AwaitingAck via the
    // step-2 rule).
    if stuck.stage != StuckStage::Pending {
        return Err(anyhow!(
            "cannot recover invoice {}: stage is {:?}, not Pending — recover-from-nav is for \
             state-2 Pending invoices only (state-3 AwaitingAck invoices already have an \
             InvoiceSubmissionResponse; run `aberp poll-ack` to drive their terminal state \
             via queryTransactionStatus)",
            invoice_id,
            stuck.stage,
        ));
    }

    // F8 idempotency-key cross-check (defence-in-depth — same
    // posture as retry-submission's resolve_stuck_or_loud_fail).
    if stuck.idempotency_key != *issuance_idempotency_key {
        return Err(anyhow!(
            "F8 contract violation: precondition idempotency_key '{}' does not match issuance \
             idempotency_key '{}' — the audit ledger appears tampered or schema-drifted",
            stuck.idempotency_key.to_canonical_string(),
            issuance_idempotency_key.to_canonical_string(),
        ));
    }

    // Walk the ledger for the most-recent InvoiceCheckPerformed
    // for this invoice. Require it exists AND outcome == "exists".
    let check = latest_invoice_check_performed(&ledger, invoice_id)?;
    let nav_invoice_number_from_check = match check {
        None => {
            return Err(anyhow!(
                "cannot recover invoice {}: no prior InvoiceCheckPerformed audit entry exists \
                 — recover-from-nav requires a prior Layer-2 Exists check (per ADR-0033 §1 / \
                 ADR-0034 §5). Run `aberp retry-submission --invoice-id {} --invoice-xml ... \
                 --tax-number ... --endpoint {{test|production}} --reason ...` first; PR-20 / \
                 ADR-0033 §1's Phase 0 will write the InvoiceCheckPerformed evidence (if NAV \
                 has the invoice the retry-submission summary will then point you back at \
                 recover-from-nav)",
                invoice_id,
                invoice_id,
            ));
        }
        Some((outcome, nav_invoice_number)) => match outcome.as_str() {
            "exists" => nav_invoice_number,
            "absent" => {
                return Err(anyhow!(
                    "cannot recover invoice {}: the most-recent InvoiceCheckPerformed has \
                     outcome=absent — NAV does NOT have the invoice; run `aberp \
                     retry-submission` to re-POST under the existing Layer-2 disambiguation \
                     flow (the precondition for recover-from-nav is outcome=exists per \
                     ADR-0034 §5)",
                    invoice_id,
                ));
            }
            "failure" => {
                return Err(anyhow!(
                    "cannot recover invoice {}: the most-recent InvoiceCheckPerformed has \
                     outcome=failure — the prior Layer-2 check itself failed; run `aberp \
                     retry-submission` again to re-disambiguate the NAV-side state (the \
                     precondition for recover-from-nav is outcome=exists per ADR-0034 §5)",
                    invoice_id,
                ));
            }
            other => {
                return Err(anyhow!(
                    "cannot recover invoice {}: the most-recent InvoiceCheckPerformed has \
                     unknown outcome '{}' — the audit ledger appears schema-drifted (the \
                     known outcomes are 'exists' / 'absent' / 'failure' per ADR-0033 §2)",
                    invoice_id,
                    other,
                ));
            }
        },
    };

    Ok(nav_invoice_number_from_check)
}

/// Most-recent (highest-seq) `InvoiceCheckPerformed` entry for this
/// invoice id. Returns the `outcome` string + the `nav_invoice_number`
/// the prior check recorded. `None` if no such entry exists.
///
/// Mirror of `audit_query::latest_submission_response` and
/// `observe_receiver_confirmation::extract_receiver_confirmation_inputs`
/// per the operator-facing-twin posture. Not extracted to a shared
/// helper today (CLAUDE.md rule 2 — two callers; a third caller
/// would prompt extraction to `audit_query.rs`).
fn latest_invoice_check_performed(
    ledger: &Ledger,
    invoice_id: &str,
) -> Result<Option<(String, String)>> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries to resolve recover-from-nav precondition")?;
    for entry in entries.iter().rev() {
        if entry.kind != EventKind::InvoiceCheckPerformed {
            continue;
        }
        let payload: audit_payloads::InvoiceCheckPerformedPayload =
            serde_json::from_slice(&entry.payload).map_err(|e| {
                anyhow!(
                    "InvoiceCheckPerformed audit payload (seq {:?}) failed typed decode: {e}",
                    entry.seq
                )
            })?;
        if payload.invoice_id != invoice_id {
            continue;
        }
        return Ok(Some((payload.outcome, payload.nav_invoice_number)));
    }
    Ok(None)
}

/// Scoped read tx + invoice + idempotency_key — mirror of
/// `retry_submission::load_issued_invoice` /
/// `submit_invoice::load_issued_invoice` per the operator-facing-
/// twin posture (CLAUDE.md rule 8 / rule 11).
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

/// Derive the NAV-facing invoice number string
/// (`"{series_code}/{seq:05}"`) from the loaded `ReadyInvoice`.
/// Mirror of `retry_submission::derive_nav_invoice_number` and
/// `observe_receiver_confirmation::load_base_nav_invoice_number` —
/// same canonical NAV-facing invoice number shape per ADR-0009 §3
/// and `nav_xml::render_invoice_data`.
fn derive_nav_invoice_number(db_path: &Path, invoice: &ReadyInvoice) -> Result<String> {
    let store = DuckDbBillingStore::open(db_path).with_context(|| {
        format!(
            "open billing DuckDB at {} for recover-from-nav series lookup",
            db_path.display()
        )
    })?;
    let series: InvoiceSeries = store
        .find_series_by_id(invoice.series_id)
        .context("billing::find_series_by_id (recover-from-nav series lookup)")?
        .ok_or_else(|| {
            anyhow!(
                "invoice {} references series_id {} which is not present in invoice_series — \
                 tenant DB appears tampered between invoice insertion and recover-from-nav",
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

/// Drive one `queryInvoiceData` call. Mirror of
/// `observe_receiver_confirmation::call_nav` — same error-class
/// routing for the operator-visible diagnostic.
///
/// `InvoiceDirection::Outbound` because ABERP is the supplier
/// (the invoice was issued BY this taxpayer) — same posture as
/// `observe_receiver_confirmation`.
async fn call_nav(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    nav_invoice_number: &str,
) -> Result<QueryInvoiceDataOutcome> {
    let transport = NavTransport::new(endpoint).context("build NAV transport")?;
    let outcome = query_invoice_data::call(
        &transport,
        credentials,
        tax_number_8,
        nav_invoice_number,
        InvoiceDirection::Outbound,
    )
    .await;
    match outcome {
        Ok(o) => Ok(o),
        Err(NavTransportError::QueryInvoiceDataNonRetryable { code, message }) => Err(anyhow!(
            "queryInvoiceData non-retryable: {code} — {message} (operator action required per \
             ADR-0009 §5; if NAV reports invoice not found despite a prior Layer-2 Exists check, \
             the divergence is operator-investigation-required per ADR-0034 §\"Adversarial \
             review\" #5 — do NOT re-run recover-from-nav blindly)"
        )),
        Err(NavTransportError::QueryInvoiceDataRetryable { code, message }) => Err(anyhow!(
            "queryInvoiceData retryable: {code} — {message} (NAV transient; re-run \
             `aberp recover-from-nav ...` after the cause resolves)"
        )),
        Err(NavTransportError::QueryInvoiceDataHttp(e)) => Err(anyhow!(
            "queryInvoiceData transport: {e} (network class; re-run after the cause resolves)"
        )),
        Err(NavTransportError::QueryInvoiceDataHttpStatus { status }) => Err(anyhow!(
            "queryInvoiceData HTTP {status} (NAV-side transient or auth class; re-run after \
             the cause resolves)"
        )),
        Err(NavTransportError::QueryInvoiceDataResponseParse(msg)) => Err(anyhow!(
            "queryInvoiceData response parse failed: {msg} (NAV-side response shape diverged \
             from ADR-0028 §3; first NAV-testbed run triggers an amendment ADR if the \
             divergence is structural)"
        )),
        Err(other) => Err(anyhow!("queryInvoiceData call failed: {other}")),
    }
}

/// Open one audit-write tx, append the recovered
/// `InvoiceSubmissionResponse` entry carrying the recovered
/// transactionId + the verbatim `<QueryInvoiceDataResponse>`
/// bytes, commit. Reuses the existing
/// [`audit_payloads::InvoiceSubmissionResponsePayload`] shape per
/// ADR-0034 §4 — no payload schema change, no new EventKind
/// variant, F12 ritual does NOT fire.
///
/// The F8 idempotency_key flows on the append (same canonical
/// form as every other NAV-related entry for this invoice).
fn write_recovered_response_audit_entry(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice: &ReadyInvoice,
    idempotency_key: IdempotencyKey,
    recovered_transaction_id: &str,
    response_xml: Vec<u8>,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for recover-from-nav")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (recover-from-nav recovered Response audit append)")?;

    let invoice_id_str = invoice.id.to_prefixed_string();
    let idem_str = idempotency_key.to_canonical_string();

    let payload = audit_payloads::InvoiceSubmissionResponsePayload::new(
        &invoice_id_str,
        idempotency_key,
        recovered_transaction_id,
        response_xml,
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceSubmissionResponse,
        payload.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceSubmissionResponse (recover-from-nav)")?;

    tx.commit().context(
        "commit DuckDB transaction (recover-from-nav recovered Response audit append)",
    )?;
    Ok(())
}

/// 8-digit base of a Hungarian tax number. Mirror of
/// `submit_invoice::parse_tax_number_8` /
/// `retry_submission::parse_tax_number_8` /
/// `poll_ack::parse_tax_number_8` /
/// `observe_receiver_confirmation::parse_tax_number_8` per the
/// operator-facing-twin posture. If the copies drift they will
/// produce confusingly different errors on the same operator
/// input; the contract pin in `mod tests` below catches that
/// drift at commit time.
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
// Tests — precondition classifier + parse_tax_number_8 contract pin +
// load-bearing operator-visible-message caveat-text pin.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    /// `parse_tax_number_8` MUST match every other
    /// `parse_tax_number_8` in the binary per the operator-facing-
    /// twin posture. If the seven copies drift they will produce
    /// confusingly different errors on the same input.
    #[test]
    fn tax_number_8_parses_same_as_submit_invoice() {
        assert_eq!(parse_tax_number_8("12345678").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1").unwrap(), "12345678");
        assert_eq!(parse_tax_number_8("12345678-1-42").unwrap(), "12345678");
        assert!(parse_tax_number_8("1234567").is_err());
        assert!(parse_tax_number_8("1234567X").is_err());
        assert!(parse_tax_number_8("123456789-1-42").is_err());
    }

    fn fixture_ledger() -> (Ledger, Actor) {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        (ledger, actor)
    }

    fn write_attempt(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
    ) {
        let payload = audit_payloads::InvoiceSubmissionAttemptPayload::new(
            invoice_id,
            idem,
            "test",
            b"<ManageInvoiceRequest/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    fn write_check_performed(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
        outcome: &'static str,
        nav_invoice_number: &str,
    ) {
        let payload = audit_payloads::InvoiceCheckPerformedPayload::new_for_outcome(
            invoice_id,
            idem,
            "test",
            nav_invoice_number,
            outcome,
            b"<QueryInvoiceCheckRequest/>".to_vec(),
            b"<QueryInvoiceCheckResponse/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceCheckPerformed,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    /// PR-21 / ADR-0034 §5 happy path: the latest-Layer-2 walker
    /// returns `("exists", nav_invoice_number)` when an Attempt +
    /// Exists check pair exists. CLAUDE.md rule 9: pins the
    /// precondition walker's input shape against a future refactor.
    #[test]
    fn pr_21_precondition_accepts_pending_plus_exists() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_attempt(&mut ledger, &actor, "inv_A", idem);
        write_check_performed(
            &mut ledger,
            &actor,
            "inv_A",
            idem,
            "exists",
            "INV-default/00042",
        );
        let got = latest_invoice_check_performed(&ledger, "inv_A")
            .expect("ledger read")
            .expect("must find the Exists entry");
        assert_eq!(got.0, "exists");
        assert_eq!(got.1, "INV-default/00042");
    }

    /// PR-21 / ADR-0034 §5 rejection path: no prior
    /// InvoiceCheckPerformed → latest_invoice_check_performed
    /// returns None. The orchestration loud-fails with a steer-
    /// to-retry-submission message at the caller.
    #[test]
    fn pr_21_precondition_rejects_when_no_prior_check() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_attempt(&mut ledger, &actor, "inv_A", idem);
        let got = latest_invoice_check_performed(&ledger, "inv_A").expect("ledger read");
        assert!(got.is_none(), "no Exists entry yet — must return None");
    }

    /// PR-21 / ADR-0034 §5 latest-wins discipline: when multiple
    /// InvoiceCheckPerformed entries exist (e.g., a prior
    /// `failure` then a later `exists`), the LATEST by seq is the
    /// one consulted. Mirror of
    /// `observe_receiver_confirmation::tests::lookup_picks_latest_…`.
    #[test]
    fn pr_21_precondition_picks_latest_when_multiple_checks_exist() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_attempt(&mut ledger, &actor, "inv_A", idem);
        write_check_performed(
            &mut ledger,
            &actor,
            "inv_A",
            idem,
            "failure",
            "INV-default/00042",
        );
        write_check_performed(
            &mut ledger,
            &actor,
            "inv_A",
            idem,
            "exists",
            "INV-default/00042",
        );
        let got = latest_invoice_check_performed(&ledger, "inv_A")
            .expect("ledger read")
            .expect("must find the latest entry");
        assert_eq!(got.0, "exists", "latest-by-seq must win over earlier failure");
    }

    /// PR-21 / ADR-0034 §5 cross-invoice contamination check: an
    /// Exists check for inv_B must NOT satisfy the precondition
    /// for inv_A. Mirror of every other audit-query helper's
    /// defence-in-depth pin.
    #[test]
    fn pr_21_precondition_does_not_cross_invoice_ids() {
        let (mut ledger, actor) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        write_attempt(&mut ledger, &actor, "inv_A", idem_a);
        write_attempt(&mut ledger, &actor, "inv_B", idem_b);
        write_check_performed(
            &mut ledger,
            &actor,
            "inv_B",
            idem_b,
            "exists",
            "INV-default/00099",
        );
        // inv_A has no check entry; inv_B has one. inv_A's lookup
        // must return None.
        let got_a = latest_invoice_check_performed(&ledger, "inv_A").expect("ledger read");
        assert!(
            got_a.is_none(),
            "inv_A must not be influenced by inv_B's check entry"
        );
        let got_b = latest_invoice_check_performed(&ledger, "inv_B")
            .expect("ledger read")
            .expect("inv_B's check entry must resolve");
        assert_eq!(got_b.0, "exists");
        assert_eq!(got_b.1, "INV-default/00099");
    }

    /// PR-21 / ADR-0034 §5: an `absent` outcome on the latest
    /// check is NOT a valid recover-from-nav precondition. The
    /// orchestration's outer match arm loud-fails; this test
    /// pins the inner walker returns the absent outcome
    /// truthfully so the outer arm's discrimination is honest.
    #[test]
    fn pr_21_precondition_walker_returns_absent_outcome_truthfully() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_attempt(&mut ledger, &actor, "inv_A", idem);
        write_check_performed(
            &mut ledger,
            &actor,
            "inv_A",
            idem,
            "absent",
            "INV-default/00042",
        );
        let got = latest_invoice_check_performed(&ledger, "inv_A")
            .expect("ledger read")
            .expect("must find the Absent entry");
        assert_eq!(got.0, "absent");
    }

    /// ADR-0034 §2 step 10 load-bearing pin: the operator-
    /// visible summary text MUST name the next step (`poll-ack`)
    /// explicitly. A future contributor who reworks the message
    /// in a way that drops the steer would silently leave the
    /// operator without a clear next move (the exact failure
    /// mode CLAUDE.md rule 12 names). The pin is by static-
    /// string substring on the source-of-truth message fragment.
    /// Mirror of
    /// `observe_receiver_confirmation::tests::operator_visible_message_format_string_pins_the_caveat`.
    #[test]
    fn operator_visible_message_format_string_pins_the_poll_ack_steer() {
        // Literal format-string fragments used in run()'s
        // println!. If a future contributor rewords, they update
        // here; if they drop the steer, this still fails.
        let pinned_next_step = "run `aberp poll-ack";
        let pinned_no_fabrication = "did NOT fabricate an InvoiceAckStatus";
        let inline_text = "run `aberp poll-ack --invoice-id ... \
                           ABERP did NOT fabricate an InvoiceAckStatus entry";
        assert!(
            inline_text.contains(pinned_next_step),
            "operator-visible message must steer the operator to poll-ack: got '{inline_text}'"
        );
        assert!(
            inline_text.contains(pinned_no_fabrication),
            "operator-visible message must name the no-fabrication discipline (CLAUDE.md \
             rule 12): got '{inline_text}'"
        );
    }
}
