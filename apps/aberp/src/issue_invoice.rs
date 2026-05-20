//! Orchestration for the `aberp issue-invoice` subcommand.
//!
//! Pipeline:
//!
//! 1. Parse the JSON input into a [`InvoiceInputJson`] struct.
//! 2. Resolve tenant id and series code (loud-fail on invalid input).
//! 3. Compute the binary hash and build [`aberp_audit_ledger::LedgerMeta`].
//! 4. Open a tenant DuckDB connection.
//! 5. Pre-tx setup (idempotent, no allocation occurs here):
//!    - Ensure the billing schema exists via `DuckDbBillingStore::ensure_schema`.
//!    - Ensure the requested series exists (auto-create on first run).
//!    - Take the Connection back via `into_connection`.
//!    - Ensure the audit-ledger schema exists.
//! 6. Build the [`aberp_billing::IssueInvoiceCommand`] and the
//!    [`aberp_billing::AllocateArgs`] from the parsed input.
//! 7. Open a single DuckDB transaction; under it:
//!    - Call [`aberp_billing::allocate_in_tx`] to burn the next number
//!      and write the reservation + invoice rows (ADR-0009 §3 steps 1–5).
//!    - On the `Fresh` branch, call [`aberp_audit_ledger::append_in_tx`]
//!      twice: `InvoiceSequenceReserved` then `InvoiceDraftCreated`
//!      (ADR-0009 §3 step 6).
//!    - Commit (ADR-0009 §3 step 7).
//! 8. Drop the Connection to release the DuckDB file lock, then re-open
//!    a fresh `Ledger` for `verify_chain` (the verify path stays
//!    Connection-owning per session-6's verify-path decision).
//! 9. Serialize the [`ReadyInvoice`] to NAV `InvoiceData` XML.
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

use aberp_audit_ledger::{
    self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_billing::{
    self as billing, AllocateArgs, AllocateOutcome, BillingStore, CustomerId, DraftInvoice,
    DuckDbBillingStore, Huf, IdempotencyKey, InvoiceId, InvoiceSeries, IssueInvoiceCommand,
    LineItem, ResetPolicy, SeriesCode, SeriesId,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::binary_hash;
use crate::cli::IssueInvoiceArgs;
use crate::nav_xml::{self, CustomerInfo, NavParties, SupplierInfo};

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

    // 3. Compute binary hash, then build the audit-ledger metadata once
    //    for the entire process. `LedgerMeta` anchors `time_mono` and is
    //    cheap to clone; threaded into every append_in_tx call.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 4–5. Pre-tx setup: schemas + series.
    let (conn, series) = pre_tx_setup(&args.db, &series_code)?;

    // 6. Build IssueInvoiceCommand + AllocateArgs for the tx body.
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

    // 7. One transaction across the billing writes and audit appends.
    //    `run_single_tx` owns the tx lifecycle: it commits on Ok and
    //    relies on `Transaction::drop` for rollback on Err or panic.
    let outcome = run_single_tx(conn, &ledger_meta, allocate_args, idempotency_key)?;

    let invoice = outcome.invoice;
    let is_fresh = outcome.was_fresh;
    tracing::info!(
        seq = invoice.sequence_number,
        fresh = is_fresh,
        idempotency_key = ?idempotency_key,
        "invoice issued"
    );

    // 8. Verify the audit chain — the success-criterion gate. Per the
    //    session-6 verify-path decision: re-open a fresh Ledger after
    //    the tx commits and the tx-Connection drops.
    let ledger =
        Ledger::open(&args.db, tenant.clone(), binary_hash_bytes).context("open audit ledger")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER issuance")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 9. Serialize the ReadyInvoice to NAV XML.
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
    let xml = nav_xml::render_invoice_data(&invoice, &series_code, &parties)
        .context("render NAV XML")?;
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
fn pre_tx_setup(
    db_path: &Path,
    series_code: &SeriesCode,
) -> Result<(Connection, InvoiceSeries)> {
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
        let actor = Actor::test_only(); // Real auth lands in a later PR.
        let idem_str = format!("{:?}", idempotency_key);

        let payload_seq = format!(
            "{{\"invoice_id\":\"{}\",\"seq\":{},\"reservation_id\":\"{}\"}}",
            invoice.id.to_prefixed_string(),
            invoice.sequence_number,
            reservation.id.to_prefixed_string(),
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceSequenceReserved,
            payload_seq.into_bytes(),
            actor.clone(),
            Some(idem_str.clone()),
        )
        .context("audit_ledger::append_in_tx InvoiceSequenceReserved")?;

        let payload_draft = format!(
            "{{\"invoice_id\":\"{}\",\"lines\":{}}}",
            invoice.id.to_prefixed_string(),
            invoice.lines.len(),
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceDraftCreated,
            payload_draft.into_bytes(),
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
