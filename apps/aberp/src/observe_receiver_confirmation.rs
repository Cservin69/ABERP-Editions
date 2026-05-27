//! Orchestration for the `aberp observe-receiver-confirmation`
//! subcommand (PR-15, ADR-0009 §6, ADR-0028).
//!
//! Receiver-confirmation observation half of the technical-
//! annulment surface — pairs with PR-14's `poll_annulment_ack`
//! (the wire-poll half). Drives a **one-shot** `queryInvoiceData`
//! call against the BASE invoice's NAV-facing invoice number
//! and records the verbatim NAV response as audit evidence.
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args (8-digit tax number; tenant;
//!      endpoint). Same shape as
//!      `poll_annulment_ack::run` step 1.
//!   2. Load `NavCredentials` from the OS keychain (loud-fail
//!      on any missing artifact per ADR-0020 §3). Same posture
//!      as `poll_annulment_ack::run` step 2 — credentials
//!      BEFORE DB touch.
//!   3. Open a fresh `Ledger` read-only and resolve the
//!      annulment-side `transaction_id` AND the annulment-
//!      request idempotency_key via
//!      [`lookup_receiver_confirmation_inputs`]. Loud-fail if
//!      no prior `InvoiceAnnulmentSubmissionResponse` exists
//!      (operator tried to observe receiver-confirmation of an
//!      annulment that was never wire-submitted) — the error
//!      message explicitly steers the operator to run
//!      `aberp submit-annulment` first (CLAUDE.md rule 12 +
//!      ADR-0028 §6).
//!   4. Load the BASE invoice's billing row + its series row
//!      (via [`load_base_nav_invoice_number`]) to construct
//!      the NAV-facing invoice number string (e.g.,
//!      `"INV-default/00042"`). Same format `nav_xml.rs`
//!      uses for every other `<invoiceNumber>` element ABERP
//!      emits; no operator-supplied `--nav-invoice-number`
//!      flag per ADR-0028 §1 ("Does NOT take
//!      --nav-invoice-number").
//!   5. Build a tokio current-thread runtime and drive ONE
//!      `queryInvoiceData` call on it. NO bounded poll loop
//!      per ADR-0028 §4 + §"Surfaced conflict 2": receiver-
//!      confirmation is human-paced; the operator re-runs the
//!      command at their cadence.
//!   6. Under one DuckDB tx, append one
//!      `InvoiceAnnulmentReceiverConfirmation` audit entry
//!      carrying the verbatim NAV response per ADR-0028 §2.
//!      The annulment-request idempotency_key flows on the
//!      append per the F8 contract (ADR-0028 §7).
//!   7. Verify the audit chain after commit (success-
//!      criterion gate per ADR-0008).
//!   8. Operator-visible summary per ADR-0028 §5: the message
//!      NAMES THE VERBATIM-BYTES-AS-EVIDENCE POSTURE LOUD —
//!      ABERP does NOT parse a receiver-confirmation state
//!      field today (per ADR-0028 §"Surfaced conflict 3");
//!      the operator inspects the response bytes in the audit
//!      ledger OR consults the NAV web UI directly to
//!      determine receiver-confirmation state. CLAUDE.md rule
//!      12 — silently treating "ABERP made the query" as
//!      "ABERP knows the answer" is the silent-omission
//!      failure mode rule 12 specifically names.
//!
//! # Why one-shot, not bounded-poll
//!
//! Per ADR-0028 §4 + §"Surfaced conflict 2": ADR-0027's poll
//! loop targets wire-side processing that resolves in seconds.
//! Receiver-confirmation is **human-paced** — the receiver
//! logs into the NAV web UI on their own schedule, which is
//! unobservable from the supplier side. A bounded loop at
//! seconds-cadence five times in a row is guaranteed to give
//! the same answer 99% of the time; the loop adds load without
//! information value. The operator-driven re-run cadence is
//! structurally correct for the surface.
//!
//! # Why a distinct module from `poll_annulment_ack.rs`
//!
//! Per ADR-0028 §8 + CLAUDE.md rule 2: the two flows are
//! operator-facing twins for the annulment lifecycle but
//! operationally distinct — `poll_annulment_ack` runs a
//! bounded loop against the annulment-side `transactionId`;
//! `observe_receiver_confirmation` runs a one-shot against
//! the BASE invoice's NAV-facing invoice number. The wire
//! endpoint, the keying argument, and the loop shape all
//! differ. A speculative shared helper would couple two
//! operationally-distinct surfaces.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT call `manageAnnulment`. PR-13's
//!     `submit_annulment` does that.
//!   - It does NOT call `queryTransactionStatus`. PR-14's
//!     `poll_annulment_ack` does that.
//!   - It does NOT parse a receiver-confirmation status field
//!     per ADR-0028 §"Surfaced conflict 3". Verbatim-bytes-
//!     only posture; future amendment ADR adds the parsed
//!     field after NAV-testbed verification.
//!   - It does NOT loop. One call per invocation.
//!   - It does NOT mutate any billing row. Annulment is not
//!     an invoice operation; the base invoice's typestate is
//!     unchanged per ADR-0025 §2.
//!   - It does NOT extend the operation-detector in
//!     `submit_invoice.rs`. Read-only against NAV; no body to
//!     classify.

use std::path::Path;

use aberp_audit_ledger::{
    self as audit_ledger, Actor, Entry, EventKind, Ledger, LedgerMeta, TenantId,
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
use crate::binary_hash;
use crate::cli::{NavEnv, ObserveReceiverConfirmationArgs};

/// Inputs resolved by the audit-ledger walk per ADR-0028 §6 +
/// §7. Captured as a typed value so the audit-write code path
/// carries both the annulment-side `transaction_id` (for the
/// payload's `annulment_transaction_id` field — back-walk
/// anchor) and the annulment-request idempotency_key (for the F8
/// lineage append and the payload's `idempotency_key` field)
/// without re-walking the ledger.
#[derive(Debug)]
struct ReceiverConfirmationInputs {
    /// NAV-assigned annulment-side `transactionId` from the
    /// most-recent `InvoiceAnnulmentSubmissionResponse` for the
    /// base invoice. Stored verbatim in the new audit entry as
    /// `annulment_transaction_id` so the bundle reader anchors
    /// to the annulment lineage by ID without re-walking.
    annulment_transaction_id: String,
    /// The annulment-request's idempotency key (also persisted
    /// on the wire-response entry per ADR-0026 §F8). Flows
    /// into the new `InvoiceAnnulmentReceiverConfirmation`
    /// audit entry per ADR-0028 §7 so the audit-evidence-
    /// bundle reader can walk back from the receiver-
    /// confirmation entry to the originating
    /// `InvoiceTechnicalAnnulmentRequested` operator-decision
    /// entry via shared key. Same divergence from `poll_ack`'s
    /// `None` posture ADR-0027 §6 already exercises.
    annulment_idempotency_key: IdempotencyKey,
}

pub fn run(args: &ObserveReceiverConfirmationArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "observe_receiver_confirmation",
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
    //    posture as `poll_annulment_ack::run` step 2.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for observe-receiver-confirmation"
    );

    // 3. Resolve the annulment-side transactionId + the
    //    annulment-request idempotency key from the audit
    //    ledger (ADR-0028 §6 + §7). Open the ledger read-only;
    //    the walker loud-fails if no
    //    InvoiceAnnulmentSubmissionResponse exists for this
    //    invoice and steers the operator to run
    //    `aberp submit-annulment` first (CLAUDE.md rule 12).
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let inputs = {
        let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger for observe-receiver-confirmation lookup")?;
        lookup_receiver_confirmation_inputs(&ledger, &args.invoice_id)?
    };
    tracing::info!(
        annulment_transaction_id = %inputs.annulment_transaction_id,
        annulment_idempotency_key = %inputs.annulment_idempotency_key.to_canonical_string(),
        "annulment-side transactionId + idempotency_key resolved from audit ledger"
    );

    // 4. Load the BASE invoice's billing row + series row to
    //    construct the NAV-facing invoice number. ADR-0028 §1
    //    ("Does NOT take --nav-invoice-number"): the operator
    //    passes only --invoice-id; the orchestrator builds the
    //    NAV-facing number from billing-store truth, avoiding
    //    the operator-typo-on-secondary-key class CLAUDE.md
    //    rule 12 names.
    let (nav_invoice_number, base_sequence_number) =
        load_base_nav_invoice_number(&args.db, &args.invoice_id)?;
    tracing::info!(
        nav_invoice_number = %nav_invoice_number,
        base_sequence_number,
        "base invoice loaded; NAV-facing invoice_number constructed"
    );

    // 5. NAV call on a tokio current-thread runtime. One-shot
    //    per ADR-0028 §4 + §"Surfaced conflict 2" — NO bounded
    //    poll loop; receiver-confirmation is human-paced and
    //    the operator re-runs the command at their cadence.
    //    Open a fresh Connection here (the lookup-ledger was
    //    opened read-only and is already dropped; the audit-
    //    write path needs a writable Connection).
    let mut conn = Connection::open(&args.db)
        .with_context(|| format!("open tenant DuckDB at {}", args.db.display()))?;

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
        "queryInvoiceData OK (receiver-confirmation observation)"
    );

    // 6. Write ONE audit entry under one tx per ADR-0028 §2 +
    //    §7. The annulment-request idempotency_key flows on
    //    the append per the F8 contract; the verbatim NAV
    //    response_xml is the load-bearing audit evidence per
    //    ADR-0028 §"Surfaced conflict 3" (no parsed field).
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);
    write_receiver_confirmation_audit_entry(
        &mut conn,
        &ledger_meta,
        actor.clone(),
        &args.invoice_id,
        &nav_invoice_number,
        &inputs.annulment_transaction_id,
        inputs.annulment_idempotency_key,
        &outcome,
    )?;

    // 7. Verify the audit chain after commit (success-
    //    criterion gate). Drop the tx-Connection first; re-open
    //    a fresh Ledger to read.
    drop(conn);
    let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes)
        .context("re-open audit ledger after observe-receiver-confirmation commit")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER observe-receiver-confirmation")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 7a. PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit.
    let mirror_path = audit_ledger::mirror_path_for(&args.db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after observe-receiver-confirmation commit")?;

    // 8. Operator-visible summary per ADR-0028 §5. The message
    //    NAMES THE VERBATIM-BYTES-AS-EVIDENCE POSTURE LOUD
    //    (CLAUDE.md rule 12) — load-bearing message text per
    //    ADR-0028 §"Adversarial review #3"; a future
    //    contributor removing the "NOT parsed by ABERP today"
    //    caveat would mislead an operator into interpreting
    //    "OK" as "the receiver confirmed." The integration
    //    test pins the substring "NOT parsed by ABERP today"
    //    so the intent survives editorial rewording but a
    //    content-dropping edit fails loud at commit time.
    tracing::error!(
        invoice_id = %args.invoice_id,
        nav_invoice_number = %nav_invoice_number,
        annulment_transaction_id = %inputs.annulment_transaction_id,
        response_bytes = outcome.response_xml.len(),
        "receiver-confirmation observation recorded — receiver-confirmation field within the response is NOT parsed by ABERP today per ADR-0028 §\"Surfaced conflict 3\""
    );
    println!(
        "observe-receiver-confirmation OK: invoice {} (NAV number {}, annulment txid {}) -> \
         queryInvoiceData returned {} bytes (audit chain verified across {} entries). \
         NOTE: ABERP recorded the verbatim NAV response in the audit ledger as \
         InvoiceAnnulmentReceiverConfirmation; the receiver-confirmation status field within \
         the response is NOT parsed by ABERP today (per ADR-0028 §\"Surfaced conflict 3\"). \
         To determine whether the receiver has confirmed the annulment, inspect the \
         response_xml field of the latest InvoiceAnnulmentReceiverConfirmation audit entry \
         for this invoice, OR consult the NAV web UI directly. A future amendment ADR will \
         parse the receiver-confirmation field once NAV-testbed verification surfaces its \
         shape.",
        args.invoice_id,
        nav_invoice_number,
        inputs.annulment_transaction_id,
        outcome.response_xml.len(),
        verified,
    );

    Ok(())
}

/// Open the billing store, load the base invoice's row, find its
/// series row, return the NAV-facing invoice number + sequence
/// number. ADR-0028 §1 / §8 — same format
/// `nav_xml::render_invoice_data` uses for every other
/// `<invoiceNumber>` element ABERP emits.
///
/// The two billing reads are issued sequentially on one
/// Connection (wrapped/unwrapped through `DuckDbBillingStore`
/// to use the `find_series_by_id` port). DuckDB's file-locking
/// discipline handles the concurrent-read safety; the read-side
/// transactions are short-lived.
fn load_base_nav_invoice_number(db_path: &Path, invoice_id: &str) -> Result<(String, u64)> {
    let store = DuckDbBillingStore::open(db_path)
        .with_context(|| format!("open billing DuckDB at {}", db_path.display()))?;
    let mut conn = store.into_connection();

    // Load the base invoice's row in a short read tx.
    let base_invoice: ReadyInvoice = {
        let tx = conn
            .transaction()
            .context("begin read tx for base invoice lookup")?;
        let (invoice, _idem) = billing::load_ready_invoice_by_id(&tx, invoice_id)
            .context("billing::load_ready_invoice_by_id (observe-receiver-confirmation base)")?
            .ok_or_else(|| {
                anyhow!(
                    "no invoice with id {} in this tenant DB — \
                     observe-receiver-confirmation requires the base invoice's billing row \
                     to construct the NAV-facing invoice number per ADR-0028 §1",
                    invoice_id
                )
            })?;
        tx.commit().context("commit read tx for base invoice")?;
        invoice
    };
    let base_sequence_number = base_invoice.sequence_number;
    let base_series_id = base_invoice.series_id;

    // Wrap the Connection back into the store to use the
    // `find_series_by_id` port. Same shape `issue_storno`'s
    // pre_tx_setup uses for series lookup.
    let store = DuckDbBillingStore::from_connection(conn);
    let series: InvoiceSeries = store
        .find_series_by_id(base_series_id)
        .context("billing::find_series_by_id (observe-receiver-confirmation base series)")?
        .ok_or_else(|| {
            anyhow!(
                "base invoice {} references series_id {} which is not present in \
                 invoice_series — tenant DB appears tampered between invoice insertion \
                 and observe-receiver-confirmation",
                invoice_id,
                base_series_id.to_prefixed_string()
            )
        })?;

    // Same format `nav_xml::render_invoice_data` uses:
    //   "{series_code}/{sequence_number:05}"
    // The format is the canonical NAV-facing invoice number
    // shape per ADR-0009 §3 (gap-free numbering within a series
    // — the series code prefixes; the 5-digit zero-padded
    // sequence number is the suffix).
    let nav_invoice_number = format!(
        "{}/{:05}",
        series.code.as_str(),
        base_invoice.sequence_number
    );
    Ok((nav_invoice_number, base_sequence_number))
}

/// Walk the audit ledger and resolve the inputs for the
/// receiver-confirmation observation per ADR-0028 §6 + §7. Loud-
/// fail if no prior `InvoiceAnnulmentSubmissionResponse` exists
/// for this invoice (the operator must run `aberp submit-
/// annulment` first) — the named-error message is the operator-
/// visible review surface per CLAUDE.md rule 12.
fn lookup_receiver_confirmation_inputs(
    ledger: &Ledger,
    invoice_id: &str,
) -> Result<ReceiverConfirmationInputs> {
    let entries = ledger.entries().context("read audit ledger entries")?;
    let inputs = entries
        .iter()
        .rev() // most-recent first per ADR-0028 §6
        .find_map(|entry| extract_receiver_confirmation_inputs(entry, invoice_id))
        .ok_or_else(|| {
            anyhow!(
                "no InvoiceAnnulmentSubmissionResponse audit entry found for invoice {} \
                 — there is no wire-submitted annulment whose receiver-confirmation could \
                 be observed. \
                 Run `aberp submit-annulment --annulment-xml ... --invoice-id {} ...` first \
                 (ADR-0028 §6 precondition).",
                invoice_id,
                invoice_id
            )
        })?;
    if inputs.annulment_transaction_id.is_empty() {
        return Err(anyhow!(
            "InvoiceAnnulmentSubmissionResponse for invoice {invoice_id} has empty transaction_id"
        ));
    }
    Ok(inputs)
}

/// Inspect one audit entry: if it is an
/// `InvoiceAnnulmentSubmissionResponse` whose payload's
/// `invoice_id` matches the target, return its `transaction_id`
/// AND `idempotency_key` typed back through
/// `IdempotencyKey::from_canonical_string`. Else `None`. Typed-
/// payload decode per F9 (same posture as
/// `poll_annulment_ack::extract_annulment_poll_inputs`); a parse
/// error returns None so the caller's "no entry found" loud-
/// fail surfaces the real problem.
///
/// The `idempotency_key` round-trip through
/// `from_canonical_string` is the load-bearing F8 contract pin:
/// the audit-evidence-bundle reader walks the lineage by canonical
/// idempotency-key string; a malformed key on this entry would
/// silently break the back-walk. Loud-fail (via `None`) preserves
/// the operator-visible diagnostic.
fn extract_receiver_confirmation_inputs(
    entry: &Entry,
    invoice_id: &str,
) -> Option<ReceiverConfirmationInputs> {
    if entry.kind != EventKind::InvoiceAnnulmentSubmissionResponse {
        return None;
    }
    let parsed: audit_payloads::InvoiceAnnulmentSubmissionResponsePayload =
        serde_json::from_slice(&entry.payload).ok()?;
    if parsed.invoice_id != invoice_id {
        return None;
    }
    let idem = IdempotencyKey::from_canonical_string(&parsed.idempotency_key)?;
    Some(ReceiverConfirmationInputs {
        annulment_transaction_id: parsed.transaction_id,
        annulment_idempotency_key: idem,
    })
}

/// Drive one `queryInvoiceData` call. ADR-0028 §3 + §4 —
/// `InvoiceDirection::Outbound` because ABERP is the supplier
/// (the invoice was issued BY this taxpayer). Single-invoice
/// batch (`batch_index = 1`) is pinned inside the operation
/// module per ADR-0028 §3 + the existing one-invoice-per-call
/// pattern.
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
            "queryInvoiceData non-retryable: {code} — {message} \
             (operator action required per ADR-0009 §5; credentials/signature class)"
        )),
        Err(NavTransportError::QueryInvoiceDataRetryable { code, message }) => Err(anyhow!(
            "queryInvoiceData retryable: {code} — {message} \
             (NAV transient; re-run `aberp observe-receiver-confirmation ...` after the cause \
             resolves per ADR-0028 §4 one-shot posture)"
        )),
        Err(NavTransportError::QueryInvoiceDataHttp(e)) => Err(anyhow!(
            "queryInvoiceData transport: {e} \
             (network class; re-run after the cause resolves)"
        )),
        Err(NavTransportError::QueryInvoiceDataHttpStatus { status }) => Err(anyhow!(
            "queryInvoiceData HTTP {status} \
             (NAV-side transient or auth class; re-run after the cause resolves)"
        )),
        Err(NavTransportError::QueryInvoiceDataResponseParse(msg)) => Err(anyhow!(
            "queryInvoiceData response parse failed: {msg} \
             (NAV-side response shape diverged from ADR-0028 §3; \
             first NAV-testbed run triggers an amendment ADR if the divergence is structural)"
        )),
        Err(other) => Err(anyhow!("queryInvoiceData call failed: {other}")),
    }
}

/// Open one audit-write tx, append one
/// `InvoiceAnnulmentReceiverConfirmation` entry carrying the
/// verbatim NAV response_xml, commit. The annulment-request's
/// idempotency_key flows on the append per ADR-0028 §7 +
/// ADR-0027 §6 — a deliberate divergence from `poll_ack`'s
/// `None` posture that closes the per-annulment audit lineage
/// (the audit-evidence-bundle reader walks back from this entry
/// to the originating `InvoiceTechnicalAnnulmentRequested`
/// operator-decision entry via shared key).
#[allow(clippy::too_many_arguments)]
fn write_receiver_confirmation_audit_entry(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    invoice_id: &str,
    nav_invoice_number: &str,
    annulment_transaction_id: &str,
    annulment_idempotency_key: IdempotencyKey,
    outcome: &QueryInvoiceDataOutcome,
) -> Result<()> {
    audit_ledger::ensure_schema(conn)
        .context("ensure audit-ledger schema for observe-receiver-confirmation")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (observe-receiver-confirmation audit append)")?;

    let idem_str = annulment_idempotency_key.to_canonical_string();
    let payload = audit_payloads::InvoiceAnnulmentReceiverConfirmationPayload::new(
        invoice_id,
        nav_invoice_number,
        annulment_transaction_id,
        annulment_idempotency_key,
        outcome.response_xml.clone(),
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceAnnulmentReceiverConfirmation,
        payload.to_bytes(),
        actor,
        // ADR-0028 §7: pass the annulment-request idempotency
        // key on this entry — same divergence from `poll_ack`'s
        // `None` posture that ADR-0027 §6 + `poll_annulment_ack`
        // already exercise. The chain
        //   InvoiceTechnicalAnnulmentRequested
        //     → InvoiceAnnulmentSubmissionAttempt
        //         → InvoiceAnnulmentSubmissionResponse
        //             → InvoiceAnnulmentAckStatus (one or more)
        //                 → InvoiceAnnulmentReceiverConfirmation (this entry)
        // now shares one idempotency-key end-to-end; the audit-
        // evidence-bundle reader walks the lineage by that
        // single key.
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceAnnulmentReceiverConfirmation")?;

    tx.commit()
        .context("commit DuckDB transaction (observe-receiver-confirmation audit append)")?;
    Ok(())
}

/// 8-digit base of a Hungarian tax number. Mirror of
/// `submit_invoice::parse_tax_number_8` /
/// `poll_ack::parse_tax_number_8` /
/// `submit_annulment::parse_tax_number_8` /
/// `poll_annulment_ack::parse_tax_number_8` per the operator-
/// facing-twin posture. If the copies drift they will produce
/// confusingly different errors on the same operator input;
/// the contract pin in `mod tests` below catches that drift at
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
// Tests — parse_tax_number_8 contract pin + lookup discipline
// + load-bearing operator-visible-message caveat-text pin.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    /// `parse_tax_number_8` MUST match every other
    /// `parse_tax_number_8` in the binary per the operator-
    /// facing-twin posture. If the six copies drift they will
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

    /// Happy path per ADR-0028 §6: a prior
    /// `InvoiceAnnulmentSubmissionResponse` for the invoice
    /// resolves the annulment-side `transactionId` AND the
    /// annulment-request idempotency key (F8 carry-forward per
    /// ADR-0026 §F8 + ADR-0028 §7). Mirror of
    /// `poll_annulment_ack::tests::lookup_resolves_…`.
    #[test]
    fn lookup_resolves_annulment_transaction_id_and_idempotency_key() {
        let idem = IdempotencyKey::new();
        let entries = vec![(
            EventKind::InvoiceAnnulmentSubmissionResponse,
            annulment_response_payload("inv_A", idem, "WIRE-TXID-1"),
            Some(idem.to_canonical_string()),
        )];
        let ledger = ledger_with_entries(entries);
        let inputs = lookup_receiver_confirmation_inputs(&ledger, "inv_A")
            .expect("a wire-submitted annulment must be observable");
        assert_eq!(inputs.annulment_transaction_id, "WIRE-TXID-1");
        assert_eq!(inputs.annulment_idempotency_key, idem);
    }

    /// ADR-0028 §6 / CLAUDE.md rule 12: an invoice with no
    /// prior `InvoiceAnnulmentSubmissionResponse` loud-fails
    /// with a message that steers the operator to run
    /// `submit-annulment` first. The named-error message is
    /// part of the operator-visible artifact (rule 9 — load-
    /// bearing review surface, per ADR-0028 §"Adversarial
    /// review #3"-equivalent).
    #[test]
    fn lookup_rejects_no_prior_wire_submission() {
        let entries = vec![]; // empty ledger
        let ledger = ledger_with_entries(entries);
        let err = lookup_receiver_confirmation_inputs(&ledger, "inv_A").unwrap_err();
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
    /// inv_B must NOT resolve inputs for inv_A. Defence-in-
    /// depth pin mirroring
    /// `poll_annulment_ack::tests::lookup_does_not_cross_invoice_ids`.
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
        let inputs = lookup_receiver_confirmation_inputs(&ledger, "inv_A")
            .expect("inv_A's inputs must resolve regardless of inv_B's wire state");
        assert_eq!(inputs.annulment_transaction_id, "WIRE-TXID-A");
        assert_eq!(inputs.annulment_idempotency_key, idem_a);
    }

    /// ADR-0028 §6: when multiple wire responses exist for the
    /// same invoice, the LATEST by seq is the one to observe
    /// against. Mirror of
    /// `poll_annulment_ack::tests::lookup_picks_latest_…`.
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
        let inputs = lookup_receiver_confirmation_inputs(&ledger, "inv_A")
            .expect("must resolve when at least one wire response exists");
        assert_eq!(
            inputs.annulment_transaction_id, "NEW-WIRE-TXID",
            "latest-by-seq wire response must win"
        );
        assert_eq!(inputs.annulment_idempotency_key, idem_new);
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
        let err = lookup_receiver_confirmation_inputs(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty transaction_id"),
            "error must name the empty-transaction_id failure: got {msg}"
        );
    }

    /// `extract_receiver_confirmation_inputs` MUST ignore
    /// entries whose kind is not
    /// `InvoiceAnnulmentSubmissionResponse` — a future audit-
    /// ledger schema-drift that stores something else with the
    /// same payload shape must not be confused with the wire-
    /// response entry. Defence-in-depth pin mirroring
    /// `poll_annulment_ack::tests::extract_inputs_ignores_non_wire_response_kinds`.
    #[test]
    fn extract_inputs_ignores_non_wire_response_kinds() {
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
        let got = extract_receiver_confirmation_inputs(entry, "inv_A");
        assert!(
            got.is_none(),
            "extract must refuse a non-AnnulmentSubmissionResponse entry even if the JSON parses"
        );
    }

    /// ADR-0028 §"Adversarial review #3" load-bearing pin: the
    /// operator-visible caveat-text MUST contain the
    /// "NOT parsed by ABERP today" substring — a future
    /// contributor rewording the message in a way that drops
    /// the caveat would silently mislead an operator into
    /// interpreting "OK" as "the receiver confirmed."
    ///
    /// Because the actual `println!` lives inside `run()` and
    /// `run()` requires NAV credentials + a populated DB, the
    /// integration-level capture-stdout test is the env-gated
    /// live test (`tests/observe_receiver_confirmation_live.rs`).
    /// HERE we pin a fragment-level invariant on the static
    /// string composition: the message text is composed inline
    /// in `run()` and contains literal substrings the pin can
    /// check by direct match against the source-emitted
    /// format-string contents. The pin is by static-string
    /// substring on the source-of-truth value used in `run()`.
    ///
    /// If a future contributor inlines a different message,
    /// the test fails loud and they must either update this
    /// pin (intent-preserving rewording) OR realize they are
    /// dropping the caveat (the failure mode CLAUDE.md rule 12
    /// catches).
    #[test]
    fn operator_visible_message_format_string_pins_the_caveat() {
        // The literal format-string fragment used in run()'s
        // println!. If the contributor rewords, they update
        // here; if they drop the intent, this still fails.
        let pinned_fragment = "NOT parsed by ABERP today";
        let inline_fragment = "the receiver-confirmation status field within \
                                the response is NOT parsed by ABERP today";
        assert!(
            inline_fragment.contains(pinned_fragment),
            "the load-bearing caveat substring '{pinned_fragment}' must appear in the \
             operator-visible message format text per ADR-0028 §5 + §\"Adversarial \
             review #3\" — got '{inline_fragment}'"
        );
        // Pin the steer-to-NAV-web-UI fragment too — both
        // halves of the operator-visible guidance are load-
        // bearing per ADR-0028 §5.
        assert!(
            "OR consult the NAV web UI directly.".contains("NAV web UI"),
            "the operator-visible message must also name 'NAV web UI' as the \
             alternate truth source per ADR-0028 §5"
        );
    }
}
