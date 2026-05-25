//! Orchestration for the `aberp issue-modification` subcommand
//! (PR-11, ADR-0009 §6, ADR-0024).
//!
//! Structural parallel to `apps/aberp/src/issue_storno.rs` — same
//! `run` / `run_single_tx` split, same idempotency-replay branch,
//! same drop-then-reopen pattern for the post-commit chain
//! verification. Diverges from STORNO in three places (ADR-0024 §1,
//! §3, §4, §6):
//!
//!   - Precondition walker accepts BOTH a `Finalized` base (last ack
//!     `SAVED`) and an already-`Amended` base (prior
//!     `InvoiceModificationIssued` entry against the base). Rejects
//!     a `Storno`-cancelled base loudly per ADR-0024 §6 — modifying
//!     a base that has been legally cancelled is malformed.
//!
//!   - The XML emitter is [`nav_xml::render_modification_data`] —
//!     full-replace body (NOT negated like STORNO), with an
//!     `<invoiceReference>` block carrying `<modificationIssueDate>`
//!     (ADR-0024 §3 — the discriminator
//!     `submit_invoice::detect_operation_from_xml` keys on).
//!
//!   - The chain-index allocator walks BOTH `InvoiceStornoIssued` AND
//!     `InvoiceModificationIssued` payloads against the same base
//!     (ADR-0024 §7); the symmetric walker in `issue_storno.rs` was
//!     widened in the same commit. Both must stay in sync.
//!
//! Pipeline:
//!
//! 1. Parse the JSON input via [`crate::issue_invoice::InvoiceInputJson`]
//!    (same shape as `issue-invoice --in` / `issue-storno --in`).
//! 2. Validate the operator-supplied `--modification-date` is a
//!    well-formed `YYYY-MM-DD` per ADR-0024 §1 (no silent
//!    today-default — CLAUDE.md rule 4 + 12).
//! 3. Resolve tenant id and series code.
//! 4. Load NAV credentials from the OS keychain (Actor identity
//!    discipline; closes F15 the same way `issue_storno.rs` does).
//! 5. Compute the binary hash + ledger meta.
//! 6. Pre-flight precondition: walk the audit ledger (read-only
//!    `Ledger::open`) and confirm the `--references` target carries
//!    either a `SAVED` last-ack OR a prior MODIFY chain entry, AND
//!    has NOT been stornoed.
//! 7. Pre-tx setup: schemas + series (same helper shape as STORNO).
//! 8. Open one DuckDB transaction; under it: load the base row,
//!    walk the chain (both kinds), allocate, write three audit
//!    entries (`InvoiceSequenceReserved`, `InvoiceDraftCreated`,
//!    `InvoiceModificationIssued`), commit.
//! 9. Drop the Connection, re-open `Ledger`, verify the chain.
//! 10. Render the modification's `<InvoiceData>` XML via
//!     [`nav_xml::render_modification_data`], run the ADR-0022
//!     runtime XSD invariant check, write to disk.
//!
//! # `modification_index` allocator — local-base path only (PR-11)
//!
//! Same posture as `issue_storno.rs`'s local-base path. The
//! migrated-from-Billingo `queryInvoiceChainDigest` path is the same
//! deferral (ADR-0023 §4 + ADR-0024 §7 / F23) — the local invoice
//! schema has no `origin` column today, so the migrated-base
//! conditional never fires.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{
    self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_billing::{
    self as billing, AllocateArgs, AllocateOutcome, BillingStore, Currency, CustomerId,
    DraftInvoice, DuckDbBillingStore, Huf, IdempotencyKey, InvoiceId, InvoiceSeries,
    IssueInvoiceCommand, LineItem, RateMetadata, ReadyInvoice, ResetPolicy, SeriesCode, SeriesId,
};
use aberp_nav_transport::NavCredentials;
use anyhow::{anyhow, bail, Context, Result};
use duckdb::Connection;
use time::macros::format_description;
use time::Date;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::IssueModificationArgs;
use crate::invoice_currency_metadata::{
    inherit_rate_metadata_for_chain, load_invoice_currency_metadata_in_tx,
    require_chain_currency_match,
};
use crate::issue_invoice::InvoiceInputJson;
use crate::nav_xml::{
    self, CustomerInfo, ModificationReference, NavParties, SupplierInfo,
};

// ──────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────

pub fn run(args: &IssueModificationArgs) -> Result<()> {
    let _span = tracing::info_span!("issue_modification").entered();

    // 1. Read + parse the JSON input.
    let input_bytes = std::fs::read(&args.r#in)
        .with_context(|| format!("read input JSON from {}", args.r#in.display()))?;
    let input: InvoiceInputJson =
        serde_json::from_slice(&input_bytes).context("parse input JSON")?;
    tracing::info!(lines = input.lines.len(), "JSON input parsed");

    // 2. Load NAV credentials BEFORE any DB write — Actor identity
    //    discipline (closes F15 the same way issue_storno does).
    //    PR-47β / session-65: the actor derivation moves here (the CLI
    //    wrapper) so the library helper `modification_from_inputs` can
    //    be called from the SPA route with a pre-loaded actor (the
    //    route mints its own Actor from AppState's startup-cached
    //    operator_login; modification — like storno — does not call
    //    NAV at issuance time per ADR-0024 §3, so the route does not
    //    even need fresh credentials).
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        tenant = %args.tenant,
        session_id = %actor.session_id,
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for this CLI invocation"
    );

    let summary = modification_from_inputs(
        input,
        &args.db,
        &args.tenant,
        &args.series,
        &args.references,
        &args.modification_date,
        args.out.clone(),
        actor,
    )?;

    println!(
        "issued modification {} -> {} (references {} as modificationIndex {} \
         on {}, audit chain verified across {} entries)",
        summary.invoice_number,
        args.out.display(),
        args.references,
        summary.modification_index,
        args.modification_date,
        summary.entries_verified,
    );
    Ok(())
}

/// PR-47β / session-65 — operator-visible summary of a single
/// modification issuance, returned by [`modification_from_inputs`].
/// Mirrors [`crate::issue_storno::StornoIssuedSummary`] field-for-field
/// so the SPA route surfaces a uniform wire shape across the two
/// chain-child actions; the only domain-relevant difference is the
/// audit kind on the chain link (`InvoiceModificationIssued` vs
/// `InvoiceStornoIssued`).
#[derive(Debug, Clone)]
pub struct ModificationIssuedSummary {
    /// Prefixed-ULID id of the modification invoice itself
    /// (`inv_<ULID>`). Distinct from the BASE invoice's id.
    pub invoice_id: String,
    /// NAV-facing number of the modification
    /// (`<series>/<5-digit-seq>`).
    pub invoice_number: String,
    /// 1-based chain index allocated to this modification per
    /// ADR-0024 §7 (`modificationIndex` on the wire). Shared name
    /// space with prior storno entries against the same base.
    pub modification_index: u32,
    /// Ledger entry count `verify_chain` walked post-commit.
    pub entries_verified: u64,
}

/// PR-47β / session-65 — library helper that wires the modification
/// pipeline over an already-parsed `InvoiceInputJson` + an already-
/// derived `Actor`. The CLI's `run` calls into this after parsing
/// `--in` and loading NAV credentials; the SPA route calls into this
/// with the operator-edited body and an Actor minted from
/// `AppState::operator_login`.
///
/// Mirrors `issue_storno::storno_from_inputs` per the same library-
/// helper posture (A159 / PR-47α). `pub` so the integration test
/// (`tests/serve_modification_route.rs`) can drive it without
/// spinning the HTTPS listener.
///
/// Steps 1 + 3-10 from the pre-PR-47β `run` body, moved here verbatim.
/// Step 2 (NAV-creds + actor derivation) stays on the caller because
/// the SPA route mints its Actor differently (per-request, from
/// `operator_login`).
#[allow(clippy::too_many_arguments)]
pub fn modification_from_inputs(
    input: InvoiceInputJson,
    db: &Path,
    tenant_str: &str,
    series_str: &str,
    references: &str,
    modification_date: &str,
    nav_xml_out: PathBuf,
    actor: Actor,
) -> Result<ModificationIssuedSummary> {
    if input.lines.is_empty() {
        return Err(anyhow!("input has no lines"));
    }

    // Validate modification_date is canonical YYYY-MM-DD per ADR-0024
    // §1. No silent today-default. The parsed Date is discarded (the
    // audit payload stores the original String per ADR-0024 §5).
    let date_format = format_description!("[year]-[month]-[day]");
    let _parsed_date = Date::parse(modification_date, &date_format).map_err(|e| {
        anyhow!(
            "modification_date '{}' is not a well-formed YYYY-MM-DD: {e} \
             (ADR-0024 §1 requires explicit operator-supplied date — no silent default)",
            modification_date
        )
    })?;

    // Resolve tenant id + series code.
    let tenant = TenantId::new(tenant_str.to_string()).ok_or_else(|| {
        anyhow!("tenant value '{}' is empty or has a null byte", tenant_str)
    })?;
    let series_code = SeriesCode::new(series_str.to_string()).ok_or_else(|| {
        anyhow!("series value '{}' fails SeriesCode validation", series_str)
    })?;
    if !references.starts_with("inv_") {
        bail!(
            "references value '{}' is not a prefixed invoice id (expected inv_<ULID>)",
            references
        );
    }

    // Compute binary hash + ledger meta.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // ADR-0031 §5 pre-allocation hard-cap check.
    let pending_count = crate::submission_queue::count_pending(
        db,
        tenant.clone(),
        binary_hash_bytes,
    )
    .context("count pending submissions (ADR-0031 §5 cap check) for modification")?;
    if pending_count >= crate::submission_queue::HARD_CAP_PENDING {
        return Err(anyhow!(
            "submission queue is full ({}/{} pending invoices per ADR-0009 §7 / ADR-0031 §5); \
             run `aberp drain-submission-queue --endpoint <test|production> --tax-number ...` \
             to submit the backlog before issuing a modification",
            pending_count,
            crate::submission_queue::HARD_CAP_PENDING,
        ));
    }

    // Pre-flight precondition: base is Finalized OR already Amended,
    // and not Storno-cancelled. Read-only ledger; drop before opening
    // the write transaction.
    {
        let ledger = Ledger::open(db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger for modification precondition check")?;
        check_base_is_modifiable(&ledger, references)?;
    }

    // Pre-tx setup: schemas + series.
    let (conn, series) = pre_tx_setup(db, &series_code)?;

    // Build the modification command.
    let command = build_modification_command(&input, &series_code)?;
    let idempotency_key = command.idempotency_key;
    let issue_date = OffsetDateTime::now_utc();
    let draft = DraftInvoice {
        id: InvoiceId::new(),
        series_id: series.id,
        customer_id: command.customer_id,
        lines: command.lines,
        issue_date,
    };
    // PR-44γ.1 — placeholder defaults; `run_single_tx` reads the
    // base's stored currency metadata and overrides per ADR-0037 §4
    // invariant C6 (chain-currency-match by inheritance).
    let allocate_args = AllocateArgs {
        series_id: series.id,
        draft,
        idempotency_key,
        currency: Currency::Huf,
        rate_metadata: None,
    };

    let outcome = run_single_tx(
        conn,
        &ledger_meta,
        allocate_args,
        idempotency_key,
        actor,
        references,
        modification_date,
        nav_xml_out.clone(),
    )?;

    let modification = outcome.modification;
    let modification_index = outcome.modification_index;
    let base_sequence_number = outcome.base_sequence_number;
    let was_fresh = outcome.was_fresh;
    let chain_currency = outcome.chain_currency;
    let chain_rate_metadata = outcome.chain_rate_metadata;
    tracing::info!(
        seq = modification.sequence_number,
        modification_index,
        base_sequence_number,
        fresh = was_fresh,
        idempotency_key = ?idempotency_key,
        "modification issued"
    );

    // Verify the audit chain — success-criterion gate.
    let ledger = Ledger::open(db, tenant.clone(), binary_hash_bytes)
        .context("re-open audit ledger after modification commit")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER modification issuance")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    // post-commit.
    let mirror_path = audit_ledger::mirror_path_for(db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after modification commit")?;

    // Render the modification's <InvoiceData> XML + ADR-0022 runtime
    // XSD invariant check before writing to disk.
    let parties = NavParties {
        supplier: SupplierInfo {
            tax_number: input.supplier.tax_number,
            name: input.supplier.name,
            address_country_code: input.supplier.address.country_code,
            address_postal_code: input.supplier.address.postal_code,
            address_city: input.supplier.address.city,
            address_street: input.supplier.address.street,
        },
        customer: CustomerInfo {
            tax_number: input.customer.tax_number,
            name: input.customer.name,
        },
    };
    let base_invoice_number = format!(
        "{}/{:05}",
        series_code.as_str(),
        base_sequence_number
    );
    let modification_reference = ModificationReference {
        base_invoice_number,
        modification_index,
        modification_issue_date: modification_date.to_string(),
    };
    let xml = nav_xml::render_modification_data(
        &modification,
        &series_code,
        &parties,
        &modification_reference,
        chain_currency,
        chain_rate_metadata.as_ref(),
    )
    .context("render NAV modification XML")?;
    aberp_nav_xsd_validator::validate_invoice_data(&xml).context(
        "NAV InvoiceData v3.0 invariant check (ADR-0022) failed for rendered modification XML",
    )?;
    tracing::info!(
        bytes = xml.len(),
        nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
        "NAV modification InvoiceData XML passed v3.0 invariant check"
    );
    nav_xml::write_to_path(&nav_xml_out, &xml)?;
    tracing::info!(
        path = %nav_xml_out.display(),
        bytes = xml.len(),
        "NAV modification XML written"
    );

    let invoice_number = format!(
        "{}/{:05}",
        series_code.as_str(),
        modification.sequence_number
    );
    Ok(ModificationIssuedSummary {
        invoice_id: modification.id.to_prefixed_string(),
        invoice_number,
        modification_index,
        entries_verified: verified,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Pre-flight precondition: base must be Finalized OR Amended, and
// not Storno-cancelled. ADR-0024 §6.
// ──────────────────────────────────────────────────────────────────────

/// Walk the audit ledger and confirm that `base_invoice_id` is in a
/// modifiable state per ADR-0024 §6:
///
///   - Has an `InvoiceSubmissionResponse` (was submitted).
///   - Not abandoned (no `InvoiceMarkedAbandoned`).
///   - Not stornoed (no `InvoiceStornoIssued` against it as base).
///   - Last ack is `SAVED` (Finalized) — OR — at least one prior
///     `InvoiceModificationIssued` against the base exists (Amended).
///   - Last ack is NOT `ABORTED` (Rejected); NOT a non-terminal value
///     (Stuck).
///
/// Loud-fail with a specific named-reason message per CLAUDE.md
/// rule 12.
fn check_base_is_modifiable(ledger: &Ledger, base_invoice_id: &str) -> Result<()> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries for modification precondition check")?;

    let mut has_marked_abandoned = false;
    let mut has_submission_response = false;
    let mut has_storno = false;
    let mut has_prior_modification = false;
    let mut latest_ack_status: Option<String> = None;

    for entry in &entries {
        match entry.kind {
            EventKind::InvoiceMarkedAbandoned => {
                let payload: audit_payloads::InvoiceMarkedAbandonedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceMarkedAbandoned audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    has_marked_abandoned = true;
                }
            }
            EventKind::InvoiceSubmissionResponse => {
                let payload: audit_payloads::InvoiceSubmissionResponsePayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceSubmissionResponse audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    has_submission_response = true;
                }
            }
            EventKind::InvoiceAckStatus => {
                let payload: audit_payloads::InvoiceAckStatusPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceAckStatus audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    latest_ack_status = Some(payload.ack_status);
                }
            }
            EventKind::InvoiceStornoIssued => {
                let payload: audit_payloads::InvoiceStornoIssuedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceStornoIssued audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.base_invoice_id == base_invoice_id {
                    has_storno = true;
                }
            }
            EventKind::InvoiceModificationIssued => {
                let payload: audit_payloads::InvoiceModificationIssuedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceModificationIssued audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.base_invoice_id == base_invoice_id {
                    has_prior_modification = true;
                }
            }
            _ => {}
        }
    }

    if has_marked_abandoned {
        bail!(
            "base invoice {} is ABANDONED (operator previously ran \
             `aberp mark-abandoned`); no modification can be issued against it",
            base_invoice_id
        );
    }
    if has_storno {
        bail!(
            "base invoice {} has been legally cancelled by a storno — \
             a modification against a stornoed base is malformed; issue \
             a fresh corrective new invoice instead (ADR-0024 §6)",
            base_invoice_id
        );
    }
    if !has_submission_response {
        bail!(
            "base invoice {} has no NAV submission response on record — \
             run `aberp submit-invoice` and `aberp poll-ack` first \
             to finalize it before issuing a modification",
            base_invoice_id
        );
    }
    match latest_ack_status.as_deref() {
        Some("SAVED") => Ok(()),
        Some("ABORTED") => bail!(
            "base invoice {} was REJECTED by NAV (last ack: ABORTED) — \
             a modification is only valid against a SAVED (finalized) invoice; \
             issue a corrective new invoice instead",
            base_invoice_id
        ),
        Some(other) => bail!(
            "base invoice {} is STUCK (last ack: {}) — finalize it via \
             `aberp poll-ack` (or unblock via `aberp retry-submission`) \
             before issuing a modification; modification against a not-yet- \
             finalized invoice is rejected per ADR-0024 §6",
            base_invoice_id,
            other
        ),
        None => {
            // No ack on record. Permit IFF there is at least one
            // prior MODIFY against this base — that means the base is
            // in the derived `Amended` state per ADR-0024 §2, which
            // is modifiable (modify-after-modify is permitted by
            // default — ADR-0024 §6 + §8 default-permit posture).
            if has_prior_modification {
                Ok(())
            } else {
                bail!(
                    "base invoice {} has a submission response but no ack status \
                     and no prior modification — run `aberp poll-ack` first; \
                     modification requires the base to be Finalized (SAVED) or \
                     already Amended (ADR-0024 §6)",
                    base_invoice_id
                )
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pre-tx setup — same shape as issue_storno.rs / issue_invoice.rs
// ──────────────────────────────────────────────────────────────────────

fn pre_tx_setup(db_path: &Path, series_code: &SeriesCode) -> Result<(Connection, InvoiceSeries)> {
    let mut billing = DuckDbBillingStore::open(db_path)
        .with_context(|| format!("open billing DuckDB at {}", db_path.display()))?;
    billing.ensure_schema().context("ensure billing schema")?;
    let series = ensure_series(&mut billing, series_code)?;
    let conn = billing.into_connection();
    audit_ledger::ensure_schema(&conn).context("ensure audit-ledger schema")?;
    Ok((conn, series))
}

fn ensure_series<S: BillingStore + ?Sized>(
    store: &mut S,
    code: &SeriesCode,
) -> Result<InvoiceSeries> {
    if let Some(series) = store.find_series_by_code(code)? {
        return Ok(series);
    }
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: code.clone(),
        reset_policy: ResetPolicy::Never,
        fiscal_year: None,
        created_at: OffsetDateTime::now_utc(),
    };
    store.create_series(&series).context("create series")?;
    tracing::info!(series = code.as_str(), "auto-created series");
    Ok(series)
}

// ──────────────────────────────────────────────────────────────────────
// The single transaction — base load, chain-index allocation,
// modification allocator, three audit appends.
// ──────────────────────────────────────────────────────────────────────

struct TxOutcome {
    modification: ReadyInvoice,
    modification_index: u32,
    base_sequence_number: u64,
    was_fresh: bool,
    /// PR-44γ.1 — currency inherited from base per ADR-0037 §4 C6.
    chain_currency: Currency,
    /// PR-44γ.1 — rate metadata inherited from base; `huf_equivalent_total`
    /// recomputed from the modification's own full-replace gross.
    /// `None` for HUF.
    chain_rate_metadata: Option<RateMetadata>,
}

/// One DuckDB transaction across: load base row, walk chain (both
/// kinds), allocate modification, write three audit entries, commit.
/// Rollback contract matches `issue_storno::run_single_tx` (drop-on-
/// error rolls back both halves).
fn run_single_tx(
    mut conn: Connection,
    ledger_meta: &LedgerMeta,
    mut allocate_args: AllocateArgs,
    idempotency_key: IdempotencyKey,
    actor: Actor,
    base_invoice_id: &str,
    modification_issue_date: &str,
    nav_xml_path: std::path::PathBuf,
) -> Result<TxOutcome> {
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (modification: billing + audit-ledger)")?;

    // (a) Load the base row to capture its NAV-facing sequence
    //     number for the chain-link payload's `base_sequence_number`
    //     (denormalized per ADR-0023 §3, carried forward to MODIFY
    //     per ADR-0024 §5). Loud-fail if the row is absent.
    let (base_invoice, _base_idem) = billing::load_ready_invoice_by_id(&tx, base_invoice_id)
        .context("billing::load_ready_invoice_by_id (modification base)")?
        .ok_or_else(|| {
            anyhow!(
                "base invoice {} exists in audit ledger but not in billing table — \
                 tenant DB appears tampered between precondition check and modification tx",
                base_invoice_id
            )
        })?;
    let base_sequence_number = base_invoice.sequence_number;

    // (a') PR-44γ.1 — read the base invoice's stored currency + rate
    //      metadata and inherit onto the modification. The modification
    //      is full-replace per ADR-0024 §4, so `huf_equivalent_total`
    //      is computed against the modification's OWN positive gross.
    let base_currency_metadata =
        load_invoice_currency_metadata_in_tx(&tx, base_invoice_id)
            .context("load base invoice currency metadata for modification (ADR-0037 §4 C6)")?;
    let modification_gross_cents: i64 = allocate_args
        .draft
        .lines
        .iter()
        .try_fold(0i64, |acc, line| {
            let line_gross = line
                .gross_total()
                .ok_or_else(|| anyhow!("modification line gross_total overflow"))?;
            acc.checked_add(line_gross.as_i64())
                .ok_or_else(|| anyhow!("modification gross accumulator overflow"))
        })?;
    let (inherited_currency, inherited_rate_metadata) =
        inherit_rate_metadata_for_chain(&base_currency_metadata, modification_gross_cents)
            .context("inherit rate metadata for modification chain child")?;
    allocate_args.currency = inherited_currency;
    allocate_args.rate_metadata = inherited_rate_metadata.clone();
    require_chain_currency_match(
        base_currency_metadata.currency,
        allocate_args.currency,
        base_invoice_id,
    )?;

    // (b) Walk the audit ledger inside the SAME tx for prior chain
    //     entries (BOTH kinds per ADR-0024 §7) against this base.
    //     Allocate modification_index = max + 1 (or 1 if empty).
    let modification_index = next_modification_index_in_tx(&tx, base_invoice_id)?;

    // (c) Standard allocator path: burn the modification's own
    //     sequence number + write its reservation + invoice rows.
    let now = OffsetDateTime::now_utc();
    let outcome = billing::allocate_in_tx(&tx, allocate_args, now)
        .context("billing::allocate_in_tx (modification)")?;

    let (modification_invoice, reservation, was_fresh) = match outcome {
        AllocateOutcome::Fresh {
            invoice,
            reservation,
        } => (invoice, reservation, true),
        AllocateOutcome::Replay {
            invoice,
            reservation,
        } => (invoice, reservation, false),
    };

    if was_fresh {
        let idem_str = idempotency_key.to_canonical_string();

        // 1) InvoiceSequenceReserved for the MODIFICATION's own sequence.
        let seq_payload = audit_payloads::InvoiceSequenceReservedPayload::from_outcome(
            &modification_invoice,
            &reservation,
            idempotency_key,
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceSequenceReserved,
            seq_payload.to_bytes(),
            actor.clone(),
            Some(idem_str.clone()),
        )
        .context("audit_ledger::append_in_tx InvoiceSequenceReserved (modification)")?;

        // 2) InvoiceDraftCreated for the MODIFICATION. PR-18 /
        //    ADR-0031 §2 — record the operator's --out path on
        //    the audit payload.
        //
        //    PR-44γ.1 / ADR-0037 — non-HUF MODIFY stamps the inherited
        //    currency + rate metadata via `from_invoice_with_rate`; HUF
        //    keeps the existing PR-18 path. Same shape as issue_storno's
        //    branch.
        let draft_payload = if let Some(rate) = inherited_rate_metadata.as_ref() {
            audit_payloads::InvoiceDraftCreatedPayload::from_invoice_with_rate(
                &modification_invoice,
                idempotency_key,
                Some(nav_xml_path),
                inherited_currency,
                rate,
            )
        } else {
            audit_payloads::InvoiceDraftCreatedPayload::from_invoice_with_xml_path(
                &modification_invoice,
                idempotency_key,
                nav_xml_path,
            )
        };
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceDraftCreated,
            draft_payload.to_bytes(),
            actor.clone(),
            Some(idem_str.clone()),
        )
        .context("audit_ledger::append_in_tx InvoiceDraftCreated (modification)")?;

        // 3) InvoiceModificationIssued — the chain-link payload.
        let modification_payload = audit_payloads::InvoiceModificationIssuedPayload::new(
            &modification_invoice.id.to_prefixed_string(),
            modification_invoice.sequence_number,
            &reservation.id.to_prefixed_string(),
            idempotency_key,
            base_invoice_id,
            base_sequence_number,
            modification_index,
            modification_issue_date,
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceModificationIssued,
            modification_payload.to_bytes(),
            actor,
            Some(idem_str),
        )
        .context("audit_ledger::append_in_tx InvoiceModificationIssued")?;
    } else {
        tracing::info!("replay path: no new audit entries written (modification idempotency hit)");
    }

    tx.commit()
        .context("commit DuckDB transaction (modification: billing + audit-ledger)")?;
    Ok(TxOutcome {
        modification: modification_invoice,
        modification_index,
        base_sequence_number,
        was_fresh,
        chain_currency: inherited_currency,
        chain_rate_metadata: inherited_rate_metadata,
    })
}

/// Walk `audit_ledger` inside the borrowed transaction for every
/// chain entry (BOTH `InvoiceStornoIssued` AND
/// `InvoiceModificationIssued`), decode each payload, filter by
/// `base_invoice_id`, return `max(modification_index) + 1` — or `1`
/// if no prior chain entry exists.
///
/// **Symmetric** to `issue_storno::next_modification_index_in_tx`
/// per ADR-0024 §7 — both walkers must stay in sync. NAV's
/// `modificationIndex` uniqueness is per `invoiceReference`
/// regardless of operation kind; walking only one kind would
/// re-issue an index the other kind already burned, and NAV would
/// reject at the wire (far from the allocator). The two functions
/// are NOT extracted to a shared `chain_allocator` module — that
/// would be the speculative-abstraction trap (CLAUDE.md rule 2); if
/// a third chain kind ever appears, both functions extend together
/// and the extraction trigger is named in ADR-0024 §7.
fn next_modification_index_in_tx(
    tx: &duckdb::Transaction<'_>,
    base_invoice_id: &str,
) -> Result<u32> {
    let mut max_index: u32 = 0;

    // STORNO entries.
    {
        let mut stmt = tx
            .prepare("SELECT seq, payload FROM audit_ledger WHERE kind = ?;")
            .context("prepare audit_ledger scan for storno chain index (modification walker)")?;
        let rows = stmt
            .query_map([EventKind::InvoiceStornoIssued.as_str()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .context("query audit_ledger for storno chain index (modification walker)")?;
        for row in rows {
            let (seq, payload_bytes) = row
                .context("read audit_ledger row during storno chain-index walk (modification)")?;
            let payload: audit_payloads::InvoiceStornoIssuedPayload =
                serde_json::from_slice(&payload_bytes).map_err(|e| {
                    anyhow!(
                        "InvoiceStornoIssued audit payload (seq {seq}) failed typed decode: {e} \
                         — audit ledger appears tampered or schema-drifted"
                    )
                })?;
            if payload.base_invoice_id == base_invoice_id
                && payload.modification_index > max_index
            {
                max_index = payload.modification_index;
            }
        }
    }

    // MODIFICATION entries.
    {
        let mut stmt = tx
            .prepare("SELECT seq, payload FROM audit_ledger WHERE kind = ?;")
            .context("prepare audit_ledger scan for modification chain index")?;
        let rows = stmt
            .query_map([EventKind::InvoiceModificationIssued.as_str()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .context("query audit_ledger for modification chain index")?;
        for row in rows {
            let (seq, payload_bytes) =
                row.context("read audit_ledger row during modification chain-index walk")?;
            let payload: audit_payloads::InvoiceModificationIssuedPayload =
                serde_json::from_slice(&payload_bytes).map_err(|e| {
                    anyhow!(
                        "InvoiceModificationIssued audit payload (seq {seq}) failed typed decode: {e} \
                         — audit ledger appears tampered or schema-drifted"
                    )
                })?;
            if payload.base_invoice_id == base_invoice_id
                && payload.modification_index > max_index
            {
                max_index = payload.modification_index;
            }
        }
    }

    Ok(max_index.saturating_add(1))
}

// ──────────────────────────────────────────────────────────────────────
// Modification command construction — same shape as issue_storno's
// build_storno_command. The MODIFY semantics are full-replace
// (ADR-0024 §4) — the input.lines ARE the new effective lines, no
// negation.
// ──────────────────────────────────────────────────────────────────────

fn build_modification_command(
    input: &InvoiceInputJson,
    code: &SeriesCode,
) -> Result<IssueInvoiceCommand> {
    let lines = input
        .lines
        .iter()
        .map(|l| LineItem {
            description: l.description.clone(),
            quantity: l.quantity,
            unit_price: Huf(l.unit_price),
            vat_rate_basis_points: percent_to_basis_points(l.vat_rate_percent),
        })
        .collect();
    Ok(IssueInvoiceCommand {
        idempotency_key: IdempotencyKey::new(),
        series_code: code.clone(),
        customer_id: CustomerId::new(),
        lines,
    })
}

fn percent_to_basis_points(percent: u16) -> u16 {
    percent.saturating_mul(100)
}

// ──────────────────────────────────────────────────────────────────────
// Tests — chain-index allocator (union walk over both kinds) +
// precondition walker (modify-after-modify, storno rejection, etc.).
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    /// Build a Connection-owning fixture: ensure the audit-ledger
    /// schema, then append chain entries of mixed kinds inside one
    /// tx. The `entries` slice is `(kind_label, base_invoice_id,
    /// modification_index)` — `kind_label` is `"S"` for STORNO and
    /// `"M"` for MODIFY. Returns the Connection; the caller opens
    /// its own tx to invoke `next_modification_index_in_tx`.
    fn fixture_ledger_with_mixed_chain(entries: &[(&str, &str, u32)]) -> Connection {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let meta = LedgerMeta::new(tenant, bh);

        let mut conn = Connection::open_in_memory().unwrap();
        audit_ledger::ensure_schema(&conn).unwrap();
        {
            let tx = conn.transaction().unwrap();
            for (i, (kind_label, base, idx)) in entries.iter().enumerate() {
                let idem = IdempotencyKey::new();
                match *kind_label {
                    "S" => {
                        let payload = audit_payloads::InvoiceStornoIssuedPayload::new(
                            &format!("inv_storno_{i}"),
                            100 + i as u64,
                            &format!("rsv_storno_{i}"),
                            idem,
                            base,
                            42,
                            *idx,
                        );
                        audit_ledger::append_in_tx(
                            &tx,
                            &meta,
                            EventKind::InvoiceStornoIssued,
                            payload.to_bytes(),
                            actor.clone(),
                            Some(idem.to_canonical_string()),
                        )
                        .unwrap();
                    }
                    "M" => {
                        let payload = audit_payloads::InvoiceModificationIssuedPayload::new(
                            &format!("inv_modif_{i}"),
                            100 + i as u64,
                            &format!("rsv_modif_{i}"),
                            idem,
                            base,
                            42,
                            *idx,
                            "2026-05-21",
                        );
                        audit_ledger::append_in_tx(
                            &tx,
                            &meta,
                            EventKind::InvoiceModificationIssued,
                            payload.to_bytes(),
                            actor.clone(),
                            Some(idem.to_canonical_string()),
                        )
                        .unwrap();
                    }
                    other => panic!("unknown kind_label {other}"),
                }
            }
            tx.commit().unwrap();
        }
        conn
    }

    #[test]
    fn next_modification_index_starts_at_1_when_chain_is_empty() {
        let mut conn = fixture_ledger_with_mixed_chain(&[]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 1);
    }

    /// MODIFY-only chain: prior MODIFY at 1, next MODIFY allocates 2.
    /// Pins the MODIFY half of the union walk in isolation.
    #[test]
    fn next_modification_index_increments_past_modify_only_chain() {
        let mut conn = fixture_ledger_with_mixed_chain(&[("M", "inv_BASE", 1)]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 2);
    }

    /// **ADR-0024 §7 union-walk pin.** Mixed chain: prior STORNO at
    /// 1, prior MODIFY at 2. Next allocation must be 3 — the walker
    /// must consider both kinds. Without this, the MODIFY-only walker
    /// would see 2 and return 3 (correct by coincidence here), but
    /// the STORNO-only walker would see 1 and return 2, which would
    /// duplicate the existing MODIFY's index — exactly the
    /// `INVOICE_NUMBER_NOT_UNIQUE` failure mode the union walk is
    /// designed to prevent. CLAUDE.md rule 9: this test asserts the
    /// intent (union over both kinds), not just any allocator
    /// behaviour.
    #[test]
    fn next_modification_index_unions_storno_and_modify_against_same_base() {
        let mut conn = fixture_ledger_with_mixed_chain(&[
            ("S", "inv_BASE", 1),
            ("M", "inv_BASE", 2),
        ]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(
            idx, 3,
            "modification walker must union STORNO + MODIFY chains \
             (ADR-0024 §7) — got {idx}"
        );
    }

    /// Defence-in-depth: chain isolation by base. A STORNO + MODIFY
    /// against `inv_OTHER` must not contaminate `inv_BASE`'s chain.
    #[test]
    fn next_modification_index_ignores_unrelated_base() {
        let mut conn = fixture_ledger_with_mixed_chain(&[
            ("S", "inv_OTHER", 1),
            ("M", "inv_OTHER", 2),
            ("M", "inv_OTHER", 3),
        ]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 1);
    }

    /// Modification of a Finalized base — accepted.
    #[test]
    fn check_base_is_modifiable_accepts_saved_ack() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_A",
            "TXID-A",
            "SAVED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(EventKind::InvoiceAckStatus, ack.to_bytes(), actor, None)
            .unwrap();

        check_base_is_modifiable(&ledger, "inv_A").expect("SAVED base must be modifiable");
    }

    /// Modification of an Amended base (prior MODIFY exists) —
    /// accepted per ADR-0024 §6 default-permit posture for
    /// modify-after-modify.
    #[test]
    fn check_base_is_modifiable_accepts_already_amended_base() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem_submit = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem_submit,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem_submit.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_A",
            "TXID-A",
            "SAVED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceAckStatus,
                ack.to_bytes(),
                actor.clone(),
                None,
            )
            .unwrap();
        // Prior MODIFY at index 1 against inv_A.
        let idem_modify = IdempotencyKey::new();
        let modif = audit_payloads::InvoiceModificationIssuedPayload::new(
            "inv_M1",
            7,
            "rsv_M1",
            idem_modify,
            "inv_A",
            1,
            1,
            "2026-05-20",
        );
        ledger
            .append(
                EventKind::InvoiceModificationIssued,
                modif.to_bytes(),
                actor,
                Some(idem_modify.to_canonical_string()),
            )
            .unwrap();

        check_base_is_modifiable(&ledger, "inv_A")
            .expect("Amended base (prior MODIFY) must remain modifiable");
    }

    /// **ADR-0024 §6 hard rejection: modify-of-a-Storno-base.** The
    /// inline citation in `check_base_is_modifiable`'s storno-branch
    /// + its error message is what the §6 named "load-bearing review
    /// surface". This test fires the branch with a real fixture.
    #[test]
    fn check_base_is_modifiable_rejects_stornoed_base() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem_submit = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem_submit,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem_submit.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_A",
            "TXID-A",
            "SAVED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceAckStatus,
                ack.to_bytes(),
                actor.clone(),
                None,
            )
            .unwrap();
        // STORNO against inv_A.
        let idem_storno = IdempotencyKey::new();
        let storno = audit_payloads::InvoiceStornoIssuedPayload::new(
            "inv_S1",
            7,
            "rsv_S1",
            idem_storno,
            "inv_A",
            1,
            1,
        );
        ledger
            .append(
                EventKind::InvoiceStornoIssued,
                storno.to_bytes(),
                actor,
                Some(idem_storno.to_canonical_string()),
            )
            .unwrap();

        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("legally cancelled by a storno"),
            "error message must name the storno cancellation; got: {msg}"
        );
    }

    #[test]
    fn check_base_is_modifiable_rejects_never_submitted() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no NAV submission response"),
            "error must name the missing submission: got {msg}"
        );
    }

    #[test]
    fn check_base_is_modifiable_rejects_aborted() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_A",
            "TXID-A",
            "ABORTED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(EventKind::InvoiceAckStatus, ack.to_bytes(), actor, None)
            .unwrap();
        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("REJECTED"), "got {msg}");
    }

    #[test]
    fn check_base_is_modifiable_rejects_abandoned() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let aban = audit_payloads::InvoiceMarkedAbandonedPayload::new(
            "inv_A",
            idem,
            Some("TXID-A".to_string()),
            Some("PROCESSING".to_string()),
            "operator decision",
        );
        ledger
            .append(
                EventKind::InvoiceMarkedAbandoned,
                aban.to_bytes(),
                actor,
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ABANDONED"), "got {msg}");
    }

    /// Cross-invoice contamination — SAVED ack against `inv_B` must
    /// NOT mark `inv_A` as modifiable.
    #[test]
    fn check_base_is_modifiable_does_not_cross_invoice_ids() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_B",
            idem,
            "TXID-B",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_B",
            "TXID-B",
            "SAVED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(EventKind::InvoiceAckStatus, ack.to_bytes(), actor, None)
            .unwrap();
        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no NAV submission response"),
            "inv_A should be NeverSubmitted regardless of inv_B's state: got {msg}"
        );
    }
}
