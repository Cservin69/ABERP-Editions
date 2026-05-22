//! Orchestration for the `aberp submit-annulment` subcommand
//! (PR-13, ADR-0009 §6, ADR-0026).
//!
//! Wire half of the technical-annulment surface — pairs with PR-12's
//! `request_technical_annulment` (the operator-decision half). The
//! boundary between PR-12 and this module is the `--annulment-xml`
//! file on disk: PR-12 writes it, PR-13 reads it verbatim and POSTs
//! to NAV via `manageAnnulment`.
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args (8-digit tax number, invoice id
//!      shape, env vs prod endpoint, tenant). Same loud-fail surface
//!      as `submit_invoice::run` step 1.
//!   2. Load `NavCredentials` from the OS keychain (loud-fail on any
//!      missing artifact per ADR-0020 §3). Same posture as
//!      `submit_invoice::run` step 2 — credentials BEFORE DB touch.
//!   3. Read the on-disk `<InvoiceAnnulment>` XML.
//!   3a. ADR-0026 §4 / F30 closure:
//!       `nav_xsd_validator::validate_annulment_data` runs against
//!       the bytes BEFORE any NAV call. Loud-fail if the on-disk XML
//!       diverges from the v3.0 `<InvoiceAnnulment>` allowlist (e.g.
//!       hand-edited between request-technical-annulment and
//!       submit-annulment, or future emitter regression). No
//!       `tokenExchange` happens on validation failure.
//!   4. Open tenant DuckDB; resolve the annulment-request
//!      precondition via [`check_annulment_is_submittable`]:
//!      - The base must have at least one prior
//!        `InvoiceTechnicalAnnulmentRequested` entry (ADR-0026 §6).
//!      - No prior `InvoiceAnnulmentSubmissionResponse` against the
//!        SAME annulment-request idempotency key (default-reject of
//!        double wire submission per ADR-0026 §"Surfaced conflict
//!        3"; a failed prior attempt without a successful response
//!        permits retry — same posture `retry-submission` takes for
//!        the manageInvoice path).
//!      Loud-fail with a named-reason message per CLAUDE.md rule 12
//!      on each rejection.
//!   5. NAV calls on a tokio current-thread runtime — `tokenExchange`
//!      + `manageAnnulment` with the same decrypted-token flow the
//!      pre-PR-19 `submit_invoice::call_nav` used (the
//!      manageInvoice-side flow has since been split into prepare +
//!      send per PR-19 / ADR-0032 §1; the manage-annulment-side
//!      retains the single-call shape — F40-equivalent finding on
//!      the annulment side is named-deferred). The `manageAnnulment`
//!      operation uses the new envelope from PR-13's
//!      `soap::render_manage_annulment_request`.
//!   6. Under a single DuckDB transaction, append two audit entries:
//!      `InvoiceAnnulmentSubmissionAttempt` (verbatim request bytes)
//!      and `InvoiceAnnulmentSubmissionResponse` (verbatim response
//!      bytes + new NAV transactionId). Both carry the
//!      annulment-request's idempotency_key per ADR-0026 §"F8
//!      contract".
//!   7. Verify the audit chain after commit (success-criterion gate).
//!   8. Print the operator-visible summary naming the next step
//!      (receiver must confirm in NAV web UI; future
//!      `query-annulment-status` poll observes the outcome).
//!
//! # Why two audit appends instead of one
//!
//! Same posture as `submit_invoice::run` per ADR-0009 §8: the
//! attempt is the load-bearing "we tried to withdraw data
//! submission X with body Y" evidence, captured BEFORE the response
//! is parsed so a crash mid-flight still leaves the trail intact.
//! The response carries the NAV-assigned transaction id that the
//! future `query-annulment-status` poll keys on.
//!
//! # What this flow does NOT do
//!
//!   - It does NOT poll NAV for annulment confirmation. ADR-0009 §6:
//!     the receiver must confirm in the NAV web UI; ABERP observes
//!     asynchronously via a future polling PR.
//!   - It does NOT mutate any billing row — annulment is not an
//!     invoice operation; the base invoice's typestate is unchanged
//!     per ADR-0025 §2.
//!   - It does NOT retry transient errors. PR-13 surfaces NAV-side
//!     retryable errors loud; an automatic retry loop (mirror of
//!     PR-8's `retry-submission`) is the named-trigger surface
//!     per ADR-0026 §5.
//!   - It does NOT extend `submit_invoice::detect_operation_from_xml`.
//!     The annulment body never reaches `submit-invoice`; the
//!     detector remains three-way (Create / Modify / Storno) per
//!     ADR-0024 §3.
//!   - It does NOT mint a fresh operator-decision idempotency key.
//!     The annulment-request's key (from PR-12's
//!     `InvoiceTechnicalAnnulmentRequested` entry) flows through to
//!     the new wire-evidence entries per the F8 contract.

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::IdempotencyKey;
use aberp_nav_transport::{
    operations::{manage_annulment, token_exchange},
    soap::ManageAnnulmentItem,
    NavCredentials, NavEndpoint, NavTransport,
};
use anyhow::{anyhow, bail, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::{NavEnv, SubmitAnnulmentArgs};

pub fn run(args: &SubmitAnnulmentArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "submit_annulment",
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
    if !args.invoice_id.starts_with("inv_") {
        bail!(
            "--invoice-id value '{}' is not a prefixed invoice id (expected inv_<ULID>)",
            args.invoice_id
        );
    }
    let nav_endpoint = match args.endpoint {
        NavEnv::Test => NavEndpoint::Test,
        NavEnv::Production => NavEndpoint::Production,
    };
    let endpoint_audit_label = match args.endpoint {
        NavEnv::Test => "test",
        NavEnv::Production => "production",
    };

    // 2. Load NAV credentials BEFORE touching the DB. Same
    //    posture as `submit_invoice::run` — missing creds leave the
    //    DB pristine instead of writing half a transaction and
    //    rolling back.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for submit-annulment"
    );

    // 3. Read the on-disk InvoiceAnnulment XML.
    let annulment_xml = std::fs::read(&args.annulment_xml).with_context(|| {
        format!(
            "read NAV InvoiceAnnulment XML from {}",
            args.annulment_xml.display()
        )
    })?;
    if annulment_xml.is_empty() {
        return Err(anyhow!(
            "annulment XML at {} is empty",
            args.annulment_xml.display()
        ));
    }
    tracing::info!(
        bytes = annulment_xml.len(),
        "InvoiceAnnulment XML loaded"
    );

    // 3a. ADR-0026 §4 / F30 closure: validate the on-disk
    //     <InvoiceAnnulment> bytes BEFORE any NAV call. Same
    //     loud-fail discipline as `submit_invoice::run` step 3a per
    //     ADR-0022 + ADR-0026 §4 — catches hand-edits or emitter-
    //     regression drift; no `tokenExchange` happens on
    //     validation failure.
    aberp_nav_xsd_validator::validate_annulment_data(&annulment_xml).with_context(|| {
        format!(
            "NAV InvoiceAnnulment v3.0 invariant check (ADR-0026 §4) failed for {}",
            args.annulment_xml.display()
        )
    })?;
    tracing::info!(
        nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
        "on-disk InvoiceAnnulment XML passed v3.0 invariant check before NAV submit"
    );

    // 4. Resolve the annulment-request precondition. Open the
    //    ledger read-only; the walker returns the
    //    annulment-request's idempotency key for downstream use in
    //    step 6's audit-write entries (F8 contract per ADR-0026
    //    §"F8 contract").
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let precondition = {
        let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger for submit-annulment precondition check")?;
        check_annulment_is_submittable(&ledger, &args.invoice_id)?
    };
    tracing::info!(
        annulment_idempotency_key = %precondition.annulment_idempotency_key.to_canonical_string(),
        annulment_code = %precondition.annulment_code,
        "submit-annulment precondition passed"
    );

    // 5. NAV calls on a tokio current-thread runtime. Build the
    //    runtime AFTER credentials + precondition are validated so
    //    we do not pay the runtime-startup cost on a malformed
    //    input.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for NAV calls")?;
    let nav_outcome = runtime.block_on(call_nav(
        nav_endpoint,
        &credentials,
        &tax_number_8,
        &annulment_xml,
    ))?;
    tracing::info!(
        new_transaction_id = %nav_outcome.transaction_id,
        prior_transaction_id = %precondition.prior_transaction_id,
        "NAV manageAnnulment OK"
    );

    // 6. Write both audit entries under one tx, then commit.
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);
    let mut conn = Connection::open(&args.db)
        .with_context(|| format!("open tenant DuckDB at {}", args.db.display()))?;
    write_annulment_submission_audit_entries(
        &mut conn,
        &ledger_meta,
        actor.clone(),
        &args.invoice_id,
        precondition.annulment_idempotency_key,
        endpoint_audit_label,
        &nav_outcome,
    )?;

    // 7. Verify the audit chain after commit (success-criterion gate).
    drop(conn);
    let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes)
        .context("re-open audit ledger after submit-annulment commit")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER annulment submission")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 7a. PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit.
    let mirror_path = audit_ledger::mirror_path_for(&args.db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after annulment submission commit")?;

    // 8. Operator-visible summary. Surfaced loud (CLAUDE.md rule 12)
    //    because the annulment wire submission is an operator-
    //    visible escalation — same posture as
    //    `request_technical_annulment.rs`'s closing log line.
    tracing::error!(
        invoice_id = %args.invoice_id,
        annulment_code = %precondition.annulment_code,
        annulment_transaction_id = %nav_outcome.transaction_id,
        "technical annulment SUBMITTED to NAV — receiver must confirm in NAV web UI per ADR-0009 §6; the future query-annulment-status poll will observe the confirmation"
    );

    println!(
        "submit-annulment OK: invoice {} -> NAV annulment transactionId {} \
         (prior submission txid {}, annulment code {}, endpoint {}, \
         audit chain verified across {} entries). \
         Next step: the receiver must confirm the annulment in the NAV web UI \
         per ADR-0009 §6; ABERP observes that asynchronously via the future \
         query-annulment-status poll.",
        args.invoice_id,
        nav_outcome.transaction_id,
        precondition.prior_transaction_id,
        precondition.annulment_code,
        endpoint_audit_label,
        verified,
    );

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// NAV-side outcome bundle. Private — held distinct from the now-
// removed pre-PR-19 `submit_invoice::NavSubmissionOutcome` so a
// future manageAnnulment-specific field (e.g. annulment-status hint)
// does not surface as a silent rename of a shared type. Same posture
// `retry_submission.rs` takes per CLAUDE.md rule 2 (PR-19 also kept
// the manageInvoice-side outcome types local to submit_invoice /
// drain / retry_submission — `ManageInvoiceOutcome` exists in
// `nav-transport`, but the binary's per-orchestration outcome
// structs remain duplicated by design).
// ──────────────────────────────────────────────────────────────────────

struct NavAnnulmentOutcome {
    transaction_id: String,
    request_xml: Vec<u8>,
    response_xml: Vec<u8>,
}

async fn call_nav(
    endpoint: NavEndpoint,
    credentials: &NavCredentials,
    tax_number_8: &str,
    annulment_xml: &[u8],
) -> Result<NavAnnulmentOutcome> {
    let transport = NavTransport::new(endpoint).context("build NAV transport")?;

    // tokenExchange — same flow as submit_invoice; no separate
    // audit entry per ADR-0009 §8 (token-exchange evidence rolls
    // into the attempt/response pair).
    let token = token_exchange::call(&transport, credentials, tax_number_8)
        .await
        .context("NAV tokenExchange (submit-annulment)")?;

    // manageAnnulment — single-item slice per ADR-0026 §1 (one
    // invoice per command invocation, mirroring submit-invoice).
    let manage = manage_annulment::call(
        &transport,
        credentials,
        tax_number_8,
        &token.decoded_token,
        &[ManageAnnulmentItem {
            invoice_annulment_xml: annulment_xml,
        }],
    )
    .await
    .context("NAV manageAnnulment")?;

    Ok(NavAnnulmentOutcome {
        transaction_id: manage.transaction_id,
        request_xml: manage.request_xml,
        response_xml: manage.response_xml,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Pre-flight precondition: base must have a prior annulment-request
// entry, and the wire submission for that request must not already
// have succeeded. Returns the annulment-request's idempotency key
// (for F8 flow into the new wire-evidence entries) + the annulment
// code (for the operator-visible summary) + the prior NAV
// transactionId (for the operator-visible summary's prior-
// submission context).
// ADR-0026 §6 + §7.
// ──────────────────────────────────────────────────────────────────────

/// Captured precondition facts the rest of the pipeline consumes.
/// Returned by [`check_annulment_is_submittable`] when the
/// precondition holds.
#[derive(Debug, Clone)]
struct SubmitAnnulmentPrecondition {
    /// The annulment-request's idempotency key (from the most-
    /// recent prior `InvoiceTechnicalAnnulmentRequested` entry
    /// against this invoice). Flows into the wire-evidence audit
    /// entries per ADR-0026 §"F8 contract".
    annulment_idempotency_key: IdempotencyKey,
    /// The annulment code from the prior request — for the
    /// operator-visible summary only; NOT re-recorded on the
    /// wire-evidence entries (the request payload already carries
    /// it canonically).
    annulment_code: String,
    /// The base's prior NAV `transactionId` (the original data
    /// submission this annulment withdraws) — for the operator-
    /// visible summary's prior-submission context.
    prior_transaction_id: String,
}

/// Walk the audit ledger and confirm `base_invoice_id` is ready for
/// wire-side annulment submission per ADR-0026 §6:
///
///   - At least one prior `InvoiceTechnicalAnnulmentRequested`
///     entry against this invoice (the operator's request decision
///     was actually recorded).
///   - No prior `InvoiceAnnulmentSubmissionResponse` against the
///     same annulment-request idempotency key (default-reject
///     double successful wire submission).
///
/// **Does NOT reject:**
///
///   - A prior `InvoiceAnnulmentSubmissionAttempt` without a
///     matching successful `InvoiceAnnulmentSubmissionResponse`
///     against the same idempotency key — that is the retry-after-
///     failed-wire case (§"Surfaced conflict 3"); default-permit.
///
/// Loud-fails with a specific named-reason message per CLAUDE.md
/// rule 12. The "no annulment-request found" message explicitly
/// steers the operator to run `aberp request-technical-annulment`
/// first.
fn check_annulment_is_submittable(
    ledger: &Ledger,
    base_invoice_id: &str,
) -> Result<SubmitAnnulmentPrecondition> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries for submit-annulment precondition check")?;

    // Walk forward (entries are returned in seq order per
    // Ledger::entries); the latest hit overwrites earlier hits, so
    // at end-of-loop the stored values are the most-recent ones.
    let mut latest_annulment_request: Option<(IdempotencyKey, String, String)> = None;
    // Map: annulment-request idempotency-key (canonical-string form)
    // -> has a successful wire-response landed against it?
    let mut wire_response_seen_for_key: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for entry in &entries {
        match entry.kind {
            EventKind::InvoiceTechnicalAnnulmentRequested => {
                let payload: audit_payloads::InvoiceTechnicalAnnulmentRequestedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceTechnicalAnnulmentRequested audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    let idem = IdempotencyKey::from_canonical_string(&payload.idempotency_key)
                        .ok_or_else(|| {
                            anyhow!(
                                "InvoiceTechnicalAnnulmentRequested audit payload (seq {}) \
                                 idempotency_key '{}' failed parse — \
                                 the audit ledger appears tampered or schema-drifted",
                                entry.seq.as_u64(),
                                payload.idempotency_key
                            )
                        })?;
                    latest_annulment_request = Some((
                        idem,
                        payload.annulment_code,
                        payload.prior_transaction_id,
                    ));
                }
            }
            EventKind::InvoiceAnnulmentSubmissionResponse => {
                let payload: audit_payloads::InvoiceAnnulmentSubmissionResponsePayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceAnnulmentSubmissionResponse audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    wire_response_seen_for_key.insert(payload.idempotency_key);
                }
            }
            _ => {}
        }
    }

    let (annulment_idempotency_key, annulment_code, prior_transaction_id) =
        latest_annulment_request.ok_or_else(|| {
            anyhow!(
                "base invoice {} has no InvoiceTechnicalAnnulmentRequested audit entry — \
                 there is no operator-decision annulment to submit. \
                 Run `aberp request-technical-annulment --references {} ...` first \
                 (ADR-0026 §6 precondition).",
                base_invoice_id,
                base_invoice_id
            )
        })?;

    // Default-reject double successful wire submission per
    // ADR-0026 §"Surfaced conflict 3". The check is by the
    // annulment-request's idempotency key (the F8 link from the
    // request to the wire entries).
    if wire_response_seen_for_key.contains(&annulment_idempotency_key.to_canonical_string()) {
        bail!(
            "base invoice {} already has a successful InvoiceAnnulmentSubmissionResponse \
             against the latest annulment-request (idempotency_key '{}') — \
             double-successful wire submission of the same annulment is loud-rejected by default \
             per ADR-0026 §\"Surfaced conflict 3\". If the operator wants to file a fresh \
             annulment request, run `aberp request-technical-annulment` again (which itself \
             default-rejects per ADR-0025 §6).",
            base_invoice_id,
            annulment_idempotency_key.to_canonical_string()
        );
    }

    Ok(SubmitAnnulmentPrecondition {
        annulment_idempotency_key,
        annulment_code,
        prior_transaction_id,
    })
}

/// Open one audit-write tx, append the two PR-13 wire-evidence
/// entries, commit. Both entries carry the annulment-request's
/// idempotency_key per ADR-0026 §"F8 contract".
fn write_annulment_submission_audit_entries(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    base_invoice_id: &str,
    annulment_idempotency_key: IdempotencyKey,
    endpoint_label: &'static str,
    nav_outcome: &NavAnnulmentOutcome,
) -> Result<()> {
    audit_ledger::ensure_schema(conn).context("ensure audit-ledger schema for submit-annulment")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (submit-annulment audit appends)")?;

    let idem_str = annulment_idempotency_key.to_canonical_string();

    let attempt = audit_payloads::InvoiceAnnulmentSubmissionAttemptPayload::new(
        base_invoice_id,
        annulment_idempotency_key,
        endpoint_label,
        nav_outcome.request_xml.clone(),
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceAnnulmentSubmissionAttempt,
        attempt.to_bytes(),
        actor.clone(),
        Some(idem_str.clone()),
    )
    .context("audit_ledger::append_in_tx InvoiceAnnulmentSubmissionAttempt")?;

    let response = audit_payloads::InvoiceAnnulmentSubmissionResponsePayload::new(
        base_invoice_id,
        annulment_idempotency_key,
        &nav_outcome.transaction_id,
        nav_outcome.response_xml.clone(),
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoiceAnnulmentSubmissionResponse,
        response.to_bytes(),
        actor,
        Some(idem_str),
    )
    .context("audit_ledger::append_in_tx InvoiceAnnulmentSubmissionResponse")?;

    tx.commit()
        .context("commit DuckDB transaction (submit-annulment audit appends)")?;
    Ok(())
}

/// 8-digit base of a Hungarian tax number. Mirror of
/// `submit_invoice::parse_tax_number_8` per the operator-facing-
/// twin posture `retry_submission` / `poll_ack` use. If the three
/// copies drift they will produce confusingly different errors on
/// the same operator input; that is the failure mode the inline
/// note in `retry_submission.rs` already names.
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
// Tests — parse_tax_number_8 contract pin + precondition walker
// accept/reject discipline.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    /// Build an in-memory ledger seeded with the given entries.
    /// Mirror of the helper in `request_technical_annulment.rs`'s
    /// mod tests block — kept duplicate per the operator-facing-
    /// twin posture (CLAUDE.md rule 2 / rule 3 — neither extracted
    /// nor speculatively shared until a third caller appears).
    fn ledger_with_entries(
        entries: Vec<(EventKind, Vec<u8>, Option<String>)>,
    ) -> Ledger {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        for (kind, payload, idem) in entries {
            ledger.append(kind, payload, actor.clone(), idem).unwrap();
        }
        ledger
    }

    fn annulment_request_payload(
        invoice_id: &str,
        idem: IdempotencyKey,
        prior_txid: &str,
        code: &str,
    ) -> Vec<u8> {
        let payload = audit_payloads::InvoiceTechnicalAnnulmentRequestedPayload::new(
            invoice_id,
            idem,
            prior_txid,
            code,
            "test reason",
        );
        payload.to_bytes()
    }

    fn annulment_response_payload(
        invoice_id: &str,
        idem: IdempotencyKey,
        wire_txid: &str,
    ) -> Vec<u8> {
        let payload = audit_payloads::InvoiceAnnulmentSubmissionResponsePayload::new(
            invoice_id,
            idem,
            wire_txid,
            b"<response/>".to_vec(),
        );
        payload.to_bytes()
    }

    /// Happy path: a prior annulment-request exists; no prior
    /// successful wire response against the same key. Precondition
    /// passes, returns the request's idempotency key.
    #[test]
    fn check_annulment_is_submittable_accepts_requested_unwire_submitted() {
        let idem = IdempotencyKey::new();
        let entries = vec![(
            EventKind::InvoiceTechnicalAnnulmentRequested,
            annulment_request_payload("inv_A", idem, "PRIOR-TXID", "ERRATIC_DATA"),
            Some(idem.to_canonical_string()),
        )];
        let ledger = ledger_with_entries(entries);
        let pre = check_annulment_is_submittable(&ledger, "inv_A")
            .expect("an unsubmitted annulment-request must be submittable");
        assert_eq!(pre.annulment_idempotency_key, idem);
        assert_eq!(pre.prior_transaction_id, "PRIOR-TXID");
        assert_eq!(pre.annulment_code, "ERRATIC_DATA");
    }

    /// ADR-0026 §6: an invoice with no annulment-request loud-fails
    /// with a message that steers the operator to
    /// `request-technical-annulment` first. CLAUDE.md rule 9 — the
    /// named-error-message is part of the operator-visible
    /// artifact.
    #[test]
    fn check_annulment_is_submittable_rejects_no_annulment_request() {
        let entries = vec![]; // empty ledger
        let ledger = ledger_with_entries(entries);
        let err = check_annulment_is_submittable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no InvoiceTechnicalAnnulmentRequested"),
            "error must name the missing request entry: got {msg}"
        );
        assert!(
            msg.contains("request-technical-annulment"),
            "error must steer the operator to request-technical-annulment: got {msg}"
        );
    }

    /// ADR-0026 §"Surfaced conflict 3" default: double successful
    /// wire submission against the same annulment-request
    /// idempotency key loud-rejects. The error message must name
    /// "double-successful wire submission" — load-bearing review
    /// surface so a future contributor removing the branch removes
    /// the message too (CLAUDE.md rule 9).
    #[test]
    fn check_annulment_is_submittable_rejects_double_successful_wire() {
        let idem = IdempotencyKey::new();
        let entries = vec![
            (
                EventKind::InvoiceTechnicalAnnulmentRequested,
                annulment_request_payload("inv_A", idem, "PRIOR-TXID", "ERRATIC_DATA"),
                Some(idem.to_canonical_string()),
            ),
            (
                EventKind::InvoiceAnnulmentSubmissionResponse,
                annulment_response_payload("inv_A", idem, "WIRE-TXID-1"),
                Some(idem.to_canonical_string()),
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let err = check_annulment_is_submittable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("double-successful wire submission"),
            "error must name double-successful wire submission (load-bearing review surface per ADR-0026 §\"Surfaced conflict 3\"): got {msg}"
        );
    }

    /// ADR-0026 §"Surfaced conflict 3" permit branch: a prior wire
    /// ATTEMPT without a matching RESPONSE means the prior
    /// submission failed (NAV non-OK or connection dropped before
    /// response). The operator MAY retry — precondition accepts.
    /// This is the retry-after-failed-wire case.
    #[test]
    fn check_annulment_is_submittable_permits_retry_after_failed_wire() {
        let idem = IdempotencyKey::new();
        // Seed with the request entry only (no successful response
        // entry). The attempt entry on its own is not load-bearing
        // for the precondition decision — the precondition keys on
        // RESPONSE presence per ADR-0026 §"Surfaced conflict 3".
        let entries = vec![
            (
                EventKind::InvoiceTechnicalAnnulmentRequested,
                annulment_request_payload("inv_A", idem, "PRIOR-TXID", "ERRATIC_DATA"),
                Some(idem.to_canonical_string()),
            ),
            // An attempt without a response — represents the failed
            // wire case. We can synthesize the attempt payload via
            // the typed constructor for the test fixture.
            (
                EventKind::InvoiceAnnulmentSubmissionAttempt,
                audit_payloads::InvoiceAnnulmentSubmissionAttemptPayload::new(
                    "inv_A",
                    idem,
                    "test",
                    b"<ManageAnnulmentRequest/>".to_vec(),
                )
                .to_bytes(),
                Some(idem.to_canonical_string()),
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let pre = check_annulment_is_submittable(&ledger, "inv_A")
            .expect("retry after failed wire must be permitted (ADR-0026 §\"Surfaced conflict 3\")");
        assert_eq!(pre.annulment_idempotency_key, idem);
    }

    /// Cross-invoice contamination: a wire response against inv_B
    /// must NOT block a fresh submission for inv_A. Defence-in-
    /// depth pin mirroring `check_base_is_annullable_does_not_cross_invoice_ids`
    /// in `request_technical_annulment.rs`.
    #[test]
    fn check_annulment_is_submittable_does_not_cross_invoice_ids() {
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        let entries = vec![
            (
                EventKind::InvoiceTechnicalAnnulmentRequested,
                annulment_request_payload("inv_A", idem_a, "TXID-A", "ERRATIC_DATA"),
                Some(idem_a.to_canonical_string()),
            ),
            (
                EventKind::InvoiceTechnicalAnnulmentRequested,
                annulment_request_payload("inv_B", idem_b, "TXID-B", "ERRATIC_DATA"),
                Some(idem_b.to_canonical_string()),
            ),
            // Successful wire response for inv_B — should NOT block
            // inv_A's submission since the keys differ AND the
            // invoice_id field on the response payload is inv_B.
            (
                EventKind::InvoiceAnnulmentSubmissionResponse,
                annulment_response_payload("inv_B", idem_b, "WIRE-TXID-B"),
                Some(idem_b.to_canonical_string()),
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let pre = check_annulment_is_submittable(&ledger, "inv_A")
            .expect("inv_A's annulment must be submittable regardless of inv_B's wire state");
        assert_eq!(pre.annulment_idempotency_key, idem_a);
    }

    // ── parse_tax_number_8 contract pin ─────────────────────────────

    /// Same contract as `submit_invoice::parse_tax_number_8` /
    /// `retry_submission::parse_tax_number_8` /
    /// `poll_ack::parse_tax_number_8` per the operator-facing-twin
    /// posture. If the four copies drift they will produce
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
}
