//! Orchestration for the `aberp issue-invoice` subcommand.
//!
//! Pipeline:
//!
//! 1. Parse the JSON input into a [`InvoiceInputJson`] struct.
//! 2. Resolve tenant id and series code (loud-fail on invalid input).
//! 3. **Load NAV credentials from the OS keychain** (PR-7-A — closes
//!    F15 and F6). Required for the operator's session identity, even
//!    though PR-7-A does not yet submit to NAV. Missing keychain
//!    items fail loud per CLAUDE.md rule 12 before any DB write.
//! 4. Compute the binary hash and build [`aberp_audit_ledger::LedgerMeta`].
//! 5. Open a tenant DuckDB connection.
//! 6. Pre-tx setup (idempotent, no allocation occurs here):
//!    - Ensure the billing schema exists via `DuckDbBillingStore::ensure_schema`.
//!    - Ensure the requested series exists (auto-create on first run).
//!    - Take the Connection back via `into_connection`.
//!    - Ensure the audit-ledger schema exists.
//! 7. Build the [`aberp_billing::IssueInvoiceCommand`] and the
//!    [`aberp_billing::AllocateArgs`] from the parsed input.
//! 8. Open a single DuckDB transaction; under it:
//!    - Call [`aberp_billing::allocate_in_tx`] to burn the next number
//!      and write the reservation + invoice rows (ADR-0009 §3 steps 1–5).
//!    - On the `Fresh` branch, call [`aberp_audit_ledger::append_in_tx`]
//!      twice: `InvoiceSequenceReserved` then `InvoiceDraftCreated`
//!      (ADR-0009 §3 step 6) using the keychain-derived
//!      [`Actor::from_local_cli`] — NOT `Actor::test_only` (F15).
//!    - Commit (ADR-0009 §3 step 7).
//! 9. Drop the Connection to release the DuckDB file lock, then re-open
//!    a fresh `Ledger` for `verify_chain` (the verify path stays
//!    Connection-owning per session-6's verify-path decision).
//! 10. Serialize the [`ReadyInvoice`] to NAV `InvoiceData` XML.
//!
//! # ADR-0008 §Storage conformance (PR-6 close-out)
//!
//! Steps 7's billing writes and audit appends are in the **same DuckDB
//! transaction**. A crash or returned error between [`allocate_in_tx`]
//! and `tx.commit()` rolls back *both* halves cleanly — the tenant DB is
//! left exactly as before the issuance attempt. The rollback contract is
//! pinned by the conformance tests in
//! `apps/aberp/tests/rollback_conformance.rs` (panic-injection + drop
//! variants).
//!
//! The replay branch (`AllocateOutcome::Replay`) intentionally skips the
//! audit appends: the prior issuance already wrote its entries, and
//! ADR-0008's append-only contract forbids writing duplicates for the
//! same business event.

use std::path::Path;

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{
    self as billing, AllocateArgs, AllocateOutcome, BillingStore, CustomerId, DraftInvoice,
    DuckDbBillingStore, Huf, IdempotencyKey, InvoiceId, InvoiceSeries, IssueInvoiceCommand,
    LineItem, ResetPolicy, SeriesCode, SeriesId,
};
use aberp_nav_transport::NavCredentials;
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use serde::Deserialize;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::IssueInvoiceArgs;
use crate::nav_xml::{self, CustomerInfo, NavParties, SupplierInfo};
use crate::submission_queue;

// ──────────────────────────────────────────────────────────────────────
// Input JSON shape (NAV-aligned per Ervin's preference, session 5)
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct InvoiceInputJson {
    pub supplier: SupplierJson,
    pub customer: CustomerJson,
    pub lines: Vec<LineJson>,
}

#[derive(Debug, Deserialize)]
pub struct SupplierJson {
    #[serde(rename = "taxNumber")]
    pub tax_number: String,
    pub name: String,
    pub address: AddressJson,
}

#[derive(Debug, Deserialize)]
pub struct AddressJson {
    #[serde(rename = "countryCode")]
    pub country_code: String,
    #[serde(rename = "postalCode")]
    pub postal_code: String,
    pub city: String,
    pub street: String,
}

#[derive(Debug, Deserialize)]
pub struct CustomerJson {
    #[serde(rename = "taxNumber")]
    pub tax_number: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct LineJson {
    pub description: String,
    pub quantity: u32,
    #[serde(rename = "unitPrice")]
    pub unit_price: i64,
    #[serde(rename = "vatRatePercent")]
    pub vat_rate_percent: u16,
}

// ──────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────

pub fn run(args: &IssueInvoiceArgs) -> Result<()> {
    let _span = tracing::info_span!("issue_invoice").entered();

    // 1. Read + parse the JSON input.
    let input_bytes = std::fs::read(&args.r#in)
        .with_context(|| format!("read input JSON from {}", args.r#in.display()))?;
    let input: InvoiceInputJson =
        serde_json::from_slice(&input_bytes).context("parse input JSON")?;
    tracing::info!(lines = input.lines.len(), "JSON input parsed");

    if input.lines.is_empty() {
        return Err(anyhow!("input JSON has no lines"));
    }

    // 2. Resolve tenant id + series code (loud-fail on invalid input).
    let tenant = TenantId::new(args.tenant.clone()).ok_or_else(|| {
        anyhow!(
            "--tenant value '{}' is empty or has a null byte",
            args.tenant
        )
    })?;
    let series_code = SeriesCode::new(args.series.clone()).ok_or_else(|| {
        anyhow!(
            "--series value '{}' fails SeriesCode validation",
            args.series
        )
    })?;

    // 3. Load NAV credentials from the OS keychain BEFORE any DB write.
    //    Per ADR-0009 §4 + ADR-0020 §3 + CLAUDE.md rule 12: a missing
    //    keychain item is a hard error, not a silent fallback. Failing
    //    here keeps the tenant DB pristine if credentials aren't set up.
    //
    //    The login is then the user_id baked into every audit-ledger
    //    entry written by this CLI invocation (Actor::from_local_cli),
    //    closing fortnightly review F15 — Actor::test_only is no longer
    //    reachable on a production code path.
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

    // 4. Compute binary hash, then build the audit-ledger metadata once
    //    for the entire process. `LedgerMeta` anchors `time_mono` and is
    //    cheap to clone; threaded into every append_in_tx call.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 4a. PR-18 / ADR-0031 §5 — pre-allocation hard-cap check
    //     against the offline submission queue. Refuses fresh
    //     allocation when the ledger already shows the
    //     `HARD_CAP_PENDING` threshold of unsubmitted invoices.
    //     Loud-fail per CLAUDE.md rule 12 BEFORE the allocator
    //     tx opens so the sequence-slot invariant (ADR-0009 §3)
    //     is preserved. The check opens + drops its own Ledger
    //     handle; pre_tx_setup below opens a fresh Connection.
    let pending_count = submission_queue::count_pending(
        &args.db,
        tenant.clone(),
        binary_hash_bytes,
    )
    .context("count pending submissions (ADR-0031 §5 cap check)")?;
    if pending_count >= submission_queue::HARD_CAP_PENDING {
        return Err(anyhow!(
            "submission queue is full ({}/{} pending invoices per ADR-0009 §7 / ADR-0031 §5); \
             run `aberp drain-submission-queue --endpoint <test|production> --tax-number ...` \
             to submit the backlog, or `aberp mark-abandoned --invoice-id <id> --reason ...` \
             on invoices the operator has decided not to submit",
            pending_count,
            submission_queue::HARD_CAP_PENDING,
        ));
    }
    tracing::info!(
        pending_count = pending_count,
        cap = submission_queue::HARD_CAP_PENDING,
        "ADR-0031 §5 cap check passed"
    );

    // 5–6. Pre-tx setup: schemas + series.
    let (conn, series) = pre_tx_setup(&args.db, &series_code)?;

    // 7. Build IssueInvoiceCommand + AllocateArgs for the tx body.
    let command = build_command(&input, &series_code)?;
    let idempotency_key = command.idempotency_key;
    let issue_date = OffsetDateTime::now_utc();
    let draft = DraftInvoice {
        id: InvoiceId::new(),
        series_id: series.id,
        customer_id: command.customer_id,
        lines: command.lines,
        issue_date,
    };
    let allocate_args = AllocateArgs {
        series_id: series.id,
        draft,
        idempotency_key,
    };

    // 8. One transaction across the billing writes and audit appends.
    //    `run_single_tx` owns the tx lifecycle: it commits on Ok and
    //    relies on `Transaction::drop` for rollback on Err or panic.
    //
    //    PR-18 / ADR-0031 §2: the operator-chosen `--out` path is
    //    threaded into `run_single_tx` so the InvoiceDraftCreated
    //    payload's new `nav_xml_path` field records where the XML
    //    will be written. The drain worker consumes this at submit
    //    time without requiring an operator-supplied path argument.
    let outcome = run_single_tx(
        conn,
        &ledger_meta,
        allocate_args,
        idempotency_key,
        actor,
        args.out.clone(),
    )?;

    let invoice = outcome.invoice;
    let is_fresh = outcome.was_fresh;
    tracing::info!(
        seq = invoice.sequence_number,
        fresh = is_fresh,
        idempotency_key = ?idempotency_key,
        "invoice issued"
    );

    // 9. Verify the audit chain — the success-criterion gate. Per the
    //    session-6 verify-path decision: re-open a fresh Ledger after
    //    the tx commits and the tx-Connection drops.
    let ledger =
        Ledger::open(&args.db, tenant.clone(), binary_hash_bytes).context("open audit ledger")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER issuance")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 9a. PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit. On a fresh DB (or first post-PR-17 invocation
    //     on a pre-existing DB) `sync_mirror` runs the implicit
    //     one-time backfill per ADR-0030 §7 and logs
    //     `audit_mirror_initialized` at INFO.
    let mirror_path = audit_ledger::mirror_path_for(&args.db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after commit")?;

    // 10. Serialize the ReadyInvoice to NAV XML.
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
    let xml =
        nav_xml::render_invoice_data(&invoice, &series_code, &parties).context("render NAV XML")?;
    // PR-9-0 / ADR-0022: runtime <InvoiceData> v3.0 invariant check
    // between render and disk write. On failure the typed
    // `NavXsdValidationError` flows up as `anyhow::Error` — loud-fail
    // per CLAUDE.md rule 12 keeps malformed XML off both disk and the
    // wire. Audit entries from the prior commit DO remain in the
    // ledger (they describe what happened — the sequence was
    // allocated); recovery is to fix the emitter/validator and re-run
    // with the same input JSON, hitting the Replay branch which
    // returns the same invoice and re-renders cleanly.
    aberp_nav_xsd_validator::validate_invoice_data(&xml)
        .context("NAV InvoiceData v3.0 invariant check (ADR-0022) failed for rendered XML")?;
    tracing::info!(
        bytes = xml.len(),
        nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
        "NAV InvoiceData XML passed v3.0 invariant check"
    );
    nav_xml::write_to_path(&args.out, &xml)?;
    tracing::info!(path = %args.out.display(), bytes = xml.len(), "NAV XML written");

    // Match the XML's invoice-number format exactly (5-digit padding) so
    // operator logs, audit entries, and the XML body all agree.
    println!(
        "issued invoice {}/{:05} -> {} (audit chain verified across {} entries)",
        series_code.as_str(),
        invoice.sequence_number,
        args.out.display(),
        verified,
    );
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Pre-tx setup
// ──────────────────────────────────────────────────────────────────────

/// Open the tenant DB, run idempotent schema creation for both crates,
/// and ensure the requested series exists. Returns the Connection
/// (handed back from the billing store via `into_connection`) and the
/// resolved `InvoiceSeries`. No allocation occurs here; ADR-0008
/// §Storage's transactional contract is engaged in `run_single_tx`.
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
// The single transaction (PR-6 close-out)
// ──────────────────────────────────────────────────────────────────────

/// Carrier for the single-tx outcome that the caller actually needs
/// after commit: the ready invoice and the fresh-vs-replay bit. Keeps
/// `run_single_tx`'s return type narrow.
struct TxOutcome {
    invoice: aberp_billing::ReadyInvoice,
    was_fresh: bool,
}

/// Open one DuckDB transaction, run the ADR-0009 §3 allocator and the
/// ADR-0008 §Storage audit appends inside it, and commit. Returns the
/// outcome the caller needs after commit.
///
/// Rollback contract: if any step returns `Err`, the function returns
/// without committing; `Transaction::drop` rolls back. If a panic
/// unwinds across this function, the same `drop` runs. Both paths leave
/// the tenant DB in its pre-call state. Exercised by
/// `apps/aberp/tests/rollback_conformance.rs`.
fn run_single_tx(
    mut conn: Connection,
    ledger_meta: &LedgerMeta,
    allocate_args: AllocateArgs,
    idempotency_key: IdempotencyKey,
    actor: Actor,
    nav_xml_path: std::path::PathBuf,
) -> Result<TxOutcome> {
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (billing + audit-ledger)")?;

    let now = OffsetDateTime::now_utc();
    let outcome =
        billing::allocate_in_tx(&tx, allocate_args, now).context("billing::allocate_in_tx")?;

    let (invoice, reservation, was_fresh) = match outcome {
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
        // Actor is the keychain-derived `from_local_cli` value built in
        // `run()` (PR-7-A closes F15). Canonical on-disk string per
        // ADR-0005 (PR-6.1 F8). Stable across crate versions; the
        // `Debug` derive that PR-5 used was not.
        let idem_str = idempotency_key.to_canonical_string();

        // Typed payloads serialized via `serde_json::to_vec` per
        // PR-6.1 F9. `format!`-built JSON would have to be hand-
        // escaped against quotes / backslashes / control chars /
        // non-ASCII — for the values PR-5 used it happened to be
        // safe, but PR-7's NAV verbatim-XML payloads would not be.
        let seq_payload = audit_payloads::InvoiceSequenceReservedPayload::from_outcome(
            &invoice,
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
        .context("audit_ledger::append_in_tx InvoiceSequenceReserved")?;

        // PR-18 / ADR-0031 §2 — record the operator's --out path on
        // the audit payload so the drain worker can submit without
        // a per-invocation path argument.
        let draft_payload = audit_payloads::InvoiceDraftCreatedPayload::from_invoice_with_xml_path(
            &invoice,
            idempotency_key,
            nav_xml_path,
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceDraftCreated,
            draft_payload.to_bytes(),
            actor,
            Some(idem_str),
        )
        .context("audit_ledger::append_in_tx InvoiceDraftCreated")?;
    } else {
        tracing::info!("replay path: no new audit entries written");
    }

    tx.commit()
        .context("commit DuckDB transaction (billing + audit-ledger)")?;
    Ok(TxOutcome { invoice, was_fresh })
}

// ──────────────────────────────────────────────────────────────────────
// Command construction
// ──────────────────────────────────────────────────────────────────────

fn build_command(input: &InvoiceInputJson, code: &SeriesCode) -> Result<IssueInvoiceCommand> {
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
