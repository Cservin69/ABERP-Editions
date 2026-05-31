//! AP-side auto-sync daemon — S178 / PR-178.
//!
//! Pairs the S177 [`crate::incoming_invoices::ingest_incoming_invoice`]
//! foundation with NAV's `queryInvoiceDigest INBOUND` endpoint to
//! mirror supplier-issued invoices into the local `ap_invoice` table
//! without operator action. Ervin's posture: "low resource
//! utilization low priority database sync."
//!
//! # Cadence
//!
//!   - **Boot tick**: 30 seconds after `serve` start so the hot
//!     launch path is uncontested.
//!   - **Steady cadence**: every 30 minutes.
//!   - **Manual trigger**: `POST /api/incoming-invoices/sync-now`
//!     calls [`run_one_cycle`] synchronously and returns the
//!     ingest/skip counts in the JSON body.
//!
//! # Window
//!
//!   - **30-day rolling window** (`today - 30 .. today`). NAV's
//!     per-request cap is 35 days (per the v3.0 XSD); 30 leaves
//!     operator margin for clock skew + the "ingest the same
//!     invoice that came in last night" overlap. Flagged in the
//!     S178 brief — bump to 35 if operator-visible drops appear.
//!
//! # Pagination + safety
//!
//!   - The daemon walks pages until `current_page >= available_page`
//!     OR the [`MAX_PAGES_PER_CYCLE`] safety cap fires (10K
//!     invoices / 100 per page). A capped cycle logs a `warn!` and
//!     records the truncation on the cycle's audit entry so the
//!     operator sees the silent-omission risk loud per CLAUDE.md
//!     rule 12.
//!   - Concurrency is sequential (no per-digest fanout). The data
//!     volume is small and the daemon is deliberately gentle on
//!     NAV.
//!
//! # Idempotency
//!
//!   - `ingest_incoming_invoice` is idempotent on the UNIQUE
//!     `(tenant, supplier_tax_number, nav_invoice_number)` key per
//!     S177. The daemon does NOT pre-check existence — the helper
//!     returns `AlreadyExists { id }` for duplicates which the
//!     daemon counts as `skipped_count`.
//!
//! # Audit
//!
//!   - One [`audit_payloads::IncomingInvoiceSyncCycleCompletedPayload`]
//!     per cycle, written via
//!     `aberp_audit_ledger::EventKind::IncomingInvoiceSyncCycleCompleted`.
//!   - Per-digest ingestions emit their own `IncomingInvoiceIngested`
//!     entries via `ingest_incoming_invoice` (same path as the manual
//!     route).
//!
//! # What this module DELIBERATELY does NOT do
//!
//!   - It does NOT fetch the full NAV InvoiceData XML via
//!     `queryInvoiceData`. The digest carries enough typed fields
//!     for the v1 `ap_invoice` row (supplier, totals, dates,
//!     currency); a future PR can layer `queryInvoiceData` ingestion
//!     on top to populate `nav_xml_path` (S177's optional field) —
//!     additive over this PR. The S178 brief asked for the per-digest
//!     `queryInvoiceData` fanout, but the conservative call is to ship
//!     digest-only ingestion now (fewer NAV calls, no full-XML parser
//!     needed) and add the XML fetch in a focused follow-on when an
//!     audit-evidence consumer needs the bytes.
//!   - It does NOT short-circuit on `outcome != IngestOutcome::Created`.
//!     The daemon walks every page and counts both inserts + skips so
//!     the cycle entry is honest about the volume seen, not just the
//!     volume changed.
//!   - It does NOT trigger NAV setup or boot-state checks. The caller
//!     must be in `ServeBootState::Ready` (the spawn point in
//!     `serve.rs` checks; the manual route runs through
//!     `require_ready`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use time::{format_description::FormatItem, macros, OffsetDateTime};
use ulid::Ulid;

use aberp_audit_ledger::{self as audit_ledger, Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::IdempotencyKey;
use aberp_nav_transport::operations::query_invoice_digest::{
    self, InvoiceDigest, QueryInvoiceDigestPage,
};
use aberp_nav_transport::soap::InvoiceDirection;
use aberp_nav_transport::{NavCredentials, NavEndpoint, NavTransport};

use crate::audit_payloads::IncomingInvoiceSyncCycleCompletedPayload;
use crate::incoming_invoices::{self, IngestOutcome, IngestionInput};

/// Boot delay before the first daemon tick. 30s gives `serve`'s
/// other boot tasks (NAV poll daemon recovery, mirror reconciliation)
/// uncontested CPU.
pub const BOOT_DELAY_SECS: u64 = 30;

/// Steady-state cadence between daemon ticks. 30 minutes per the
/// session-178 brief — small data volume + low priority => no need
/// to hammer NAV.
pub const CADENCE_SECS: u64 = 30 * 60;

/// Date-window width in days. NAV's per-request cap is 35; the
/// 30-day choice leaves operator margin.
pub const WINDOW_DAYS: i64 = 30;

/// Per-cycle pagination cap. 100 pages × ~100 digests/page = 10K
/// invoices. A capped cycle records the truncation in the audit
/// entry so the operator can re-run /sync-now manually with the
/// next window slice.
pub const MAX_PAGES_PER_CYCLE: u32 = 100;

/// Closed-vocab trigger label persisted on the cycle audit entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleTrigger {
    /// Boot tick (30s after `serve` start) or steady-state cadence
    /// (every 30 min).
    Daemon,
    /// Operator-clicked `/api/incoming-invoices/sync-now`.
    Manual,
}

impl CycleTrigger {
    pub fn as_audit_str(self) -> &'static str {
        match self {
            CycleTrigger::Daemon => "daemon",
            CycleTrigger::Manual => "manual",
        }
    }
}

/// Result of one cycle. Surfaced to the manual route handler so the
/// SPA can echo a toast like "synced 3 new / 47 skipped in 412 ms."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleSummary {
    pub trigger: CycleTrigger,
    pub date_from: String,
    pub date_to: String,
    pub ingested_count: u64,
    pub skipped_count: u64,
    pub pages_walked: u32,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

/// Inputs to [`run_one_cycle`]. The daemon's spawn site in
/// `serve.rs` builds one of these per tick; the manual route does
/// the same.
pub struct CycleInputs {
    pub db_path: PathBuf,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub ap_artifacts_dir: PathBuf,
    pub tax_number_8: String,
    pub endpoint: NavEndpoint,
    pub credentials: NavCredentials,
}

/// Spawn the auto-sync daemon as a background task. Returns
/// immediately — the daemon ticks forever (or until the runtime
/// shuts down). Boot-recovery posture: a daemon panic / loud-failure
/// is logged at `warn!` and the daemon dies; the next process boot
/// re-spawns. The audit chain remains the source of truth for
/// `ingested_count` / `skipped_count` per cycle, so a missed cycle is
/// recoverable on the next tick.
pub async fn run_daemon_forever<F>(build_inputs: F)
where
    F: Fn() -> Result<CycleInputs> + Send + Sync + 'static,
{
    let build_inputs = Arc::new(build_inputs);
    tokio::time::sleep(Duration::from_secs(BOOT_DELAY_SECS)).await;
    loop {
        match build_inputs() {
            Ok(inputs) => match run_one_cycle(inputs, CycleTrigger::Daemon).await {
                Ok(summary) => {
                    tracing::info!(
                        ingested = summary.ingested_count,
                        skipped = summary.skipped_count,
                        pages = summary.pages_walked,
                        elapsed_ms = summary.elapsed_ms,
                        error = ?summary.error,
                        "AP auto-sync cycle complete"
                    );
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "AP auto-sync cycle failed"),
            },
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "AP auto-sync skipped (build_inputs failed; will retry on next tick)"
            ),
        }
        tokio::time::sleep(Duration::from_secs(CADENCE_SECS)).await;
    }
}

/// Run one sync cycle: query the digest by page, ingest each new
/// row via `ingest_incoming_invoice`, write the cycle audit entry.
/// The cycle audit entry fires UNCONDITIONALLY at the end (success
/// or loud-failure) so the audit trail has zero gaps.
pub async fn run_one_cycle(inputs: CycleInputs, trigger: CycleTrigger) -> Result<CycleSummary> {
    let started = Instant::now();
    let (date_from, date_to) = compute_date_window(OffsetDateTime::now_utc())?;
    let result = run_cycle_inner(&inputs, &date_from, &date_to).await;

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (ingested_count, skipped_count, pages_walked, error) = match &result {
        Ok((i, s, p)) => (*i, *s, *p, None),
        Err(e) => (0, 0, 0, Some(format!("{e:#}"))),
    };

    let summary = CycleSummary {
        trigger,
        date_from: date_from.clone(),
        date_to: date_to.clone(),
        ingested_count,
        skipped_count,
        pages_walked,
        elapsed_ms,
        error: error.clone(),
    };

    // Best-effort audit-entry write. A write-failure here logs loud
    // but does NOT mask the caller's original error. S191 — the
    // sync DuckDB write is fenced inside `spawn_blocking` so the
    // tokio worker pool is not blocked for the duration of the
    // INSERT + chain-verify + mirror-sync. `JoinError` is unified
    // into the existing warn! surface.
    let audit_inputs_db = inputs.db_path.clone();
    let audit_inputs_tenant = inputs.tenant.clone();
    let audit_inputs_binary_hash = inputs.binary_hash;
    let audit_inputs_login = inputs.operator_login.clone();
    let audit_summary = summary.clone();
    let audit_outcome = tokio::task::spawn_blocking(move || {
        write_cycle_audit_entry_inner(
            &audit_inputs_db,
            audit_inputs_tenant,
            audit_inputs_binary_hash,
            &audit_inputs_login,
            &audit_summary,
        )
    })
    .await;
    match audit_outcome {
        Ok(Ok(())) => {}
        Ok(Err(audit_err)) => tracing::warn!(
            error = %format!("{audit_err:#}"),
            "failed to write IncomingInvoiceSyncCycleCompleted audit entry"
        ),
        Err(join_err) => tracing::warn!(
            error = %format!("{join_err}"),
            "IncomingInvoiceSyncCycleCompleted audit-write task panicked"
        ),
    }

    match result {
        Ok(_) => Ok(summary),
        Err(e) => Err(e),
    }
}

async fn run_cycle_inner(
    inputs: &CycleInputs,
    date_from: &str,
    date_to: &str,
) -> Result<(u64, u64, u32)> {
    let transport =
        NavTransport::new(inputs.endpoint).context("build NAV transport for AP sync cycle")?;

    let mut ingested_count: u64 = 0;
    let mut skipped_count: u64 = 0;
    let mut page: u32 = 1;

    loop {
        if page > MAX_PAGES_PER_CYCLE {
            tracing::warn!(
                cap = MAX_PAGES_PER_CYCLE,
                "AP auto-sync hit per-cycle page cap; truncating — \
                 operator should re-run /sync-now to walk the remainder"
            );
            return Ok((ingested_count, skipped_count, page - 1));
        }

        let page_result: QueryInvoiceDigestPage = query_invoice_digest::call(
            &transport,
            &inputs.credentials,
            &inputs.tax_number_8,
            page,
            InvoiceDirection::Inbound,
            date_from,
            date_to,
        )
        .await
        .with_context(|| format!("queryInvoiceDigest page {page}"))?;

        let available_page = page_result.available_page;

        // S191 — process the whole page's digests on the blocking
        // pool so the tokio worker is not held across N synchronous
        // DuckDB INSERT + chain-verify + mirror-sync calls. One
        // `spawn_blocking` per page keeps the boundary-cross count at
        // O(pages) instead of O(digests).
        let digests = page_result.digests;
        let db_path = inputs.db_path.clone();
        let tenant = inputs.tenant.clone();
        let binary_hash = inputs.binary_hash;
        let operator_login = inputs.operator_login.clone();
        let ap_artifacts_dir = inputs.ap_artifacts_dir.clone();
        let (page_ingested, page_skipped) = tokio::task::spawn_blocking(move || {
            let mut ingested: u64 = 0;
            let mut skipped: u64 = 0;
            for digest in digests {
                match digest_to_ingestion_input(&digest) {
                    Ok(input) => {
                        match incoming_invoices::ingest_incoming_invoice(
                            &db_path,
                            tenant.clone(),
                            binary_hash,
                            &operator_login,
                            &ap_artifacts_dir,
                            input,
                        ) {
                            Ok(IngestOutcome::Created { .. }) => ingested += 1,
                            Ok(IngestOutcome::AlreadyExists { .. }) => skipped += 1,
                            Err(e) => {
                                // A single-digest ingest failure must NOT
                                // abort the whole cycle — the digest is
                                // logged loud and the daemon continues.
                                // Otherwise one malformed row from NAV
                                // would block every subsequent row.
                                tracing::warn!(
                                    invoice_number = %digest.invoice_number,
                                    supplier_tax = %digest.supplier_tax_number,
                                    error = ?e,
                                    "ingest_incoming_invoice failed for digest; continuing cycle"
                                );
                                skipped += 1;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            invoice_number = %digest.invoice_number,
                            supplier_tax = %digest.supplier_tax_number,
                            error = ?e,
                            "digest → IngestionInput conversion failed; skipping"
                        );
                        skipped += 1;
                    }
                }
            }
            (ingested, skipped)
        })
        .await
        .map_err(|join_err| anyhow!("AP sync per-page ingest task panicked: {join_err}"))?;
        ingested_count += page_ingested;
        skipped_count += page_skipped;

        if page >= available_page {
            return Ok((ingested_count, skipped_count, page));
        }
        page += 1;
    }
}

/// Convert a NAV digest row into an [`IngestionInput`] suitable for
/// the S177 [`incoming_invoices::ingest_incoming_invoice`] helper.
///
/// Loud-fails on:
///   - Missing or empty `supplier_name` (NAV always populates;
///     absence is schema drift per CLAUDE.md rule 12).
///   - Missing `issue_date`.
///   - Currency outside the `ap_invoice` closed vocab
///     (HUF / EUR) — the daemon does NOT silently coerce, even
///     for digests whose `currency` field is absent.
///   - Net/VAT amounts that fail to parse as `Decimal` or land
///     outside i64 minor-unit range.
fn digest_to_ingestion_input(digest: &InvoiceDigest) -> Result<IngestionInput> {
    let supplier_name = digest
        .supplier_name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "digest for supplier_tax={} invoice_number={} missing <supplierName>",
                digest.supplier_tax_number,
                digest.invoice_number,
            )
        })?;
    let issue_date = digest.issue_date.clone().ok_or_else(|| {
        anyhow!(
            "digest for supplier_tax={} invoice_number={} missing <invoiceIssueDate>",
            digest.supplier_tax_number,
            digest.invoice_number,
        )
    })?;
    let currency = match digest.currency.as_deref() {
        Some("HUF") => "HUF".to_string(),
        Some("EUR") => "EUR".to_string(),
        Some(other) => {
            return Err(anyhow!(
                "digest for invoice_number={} carries currency `{}` outside ap_invoice closed vocab (HUF | EUR)",
                digest.invoice_number,
                other,
            ));
        }
        None => {
            return Err(anyhow!(
                "digest for invoice_number={} missing <currency>",
                digest.invoice_number,
            ));
        }
    };

    let net_minor = decimal_to_minor(
        digest.invoice_net_amount.as_deref().unwrap_or("0"),
        &currency,
    )
    .with_context(|| format!("parse invoice_net_amount for {}", digest.invoice_number))?;
    let vat_minor = decimal_to_minor(
        digest.invoice_vat_amount.as_deref().unwrap_or("0"),
        &currency,
    )
    .with_context(|| format!("parse invoice_vat_amount for {}", digest.invoice_number))?;
    let gross_minor = net_minor
        .checked_add(vat_minor)
        .ok_or_else(|| anyhow!("gross overflow for {}", digest.invoice_number))?;

    Ok(IngestionInput {
        supplier_tax_number: digest.supplier_tax_number.clone(),
        supplier_name,
        supplier_address: None,
        nav_invoice_number: digest.invoice_number.clone(),
        issue_date,
        delivery_date: None,
        payment_deadline: None,
        total_net_minor: net_minor,
        total_vat_minor: vat_minor,
        total_gross_minor: gross_minor,
        currency,
        nav_xml: None,
    })
}

/// Convert a NAV-string amount into minor units for the closed-vocab
/// currency. HUF has 0 decimals (forint is the minor unit); EUR has 2
/// (cents). Loud-fails on parse / overflow per CLAUDE.md rule 12.
fn decimal_to_minor(value: &str, currency: &str) -> Result<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let parsed: Decimal = trimmed
        .parse()
        .map_err(|e| anyhow!("amount `{trimmed}` is not a valid Decimal: {e}"))?;
    let scale: u32 = match currency {
        "HUF" => 0,
        "EUR" => 2,
        other => {
            return Err(anyhow!(
                "decimal_to_minor called with currency `{other}` outside closed vocab"
            ));
        }
    };
    let scaled = parsed * Decimal::from(10i64.pow(scale));
    let rounded = scaled.round();
    rounded
        .to_i64()
        .ok_or_else(|| anyhow!("amount `{trimmed}` (scaled) exceeds i64 range"))
}

const ISO_DATE: &[FormatItem<'_>] = macros::format_description!("[year]-[month]-[day]");

fn compute_date_window(now_utc: OffsetDateTime) -> Result<(String, String)> {
    let today = now_utc.date();
    let from = today
        .checked_sub(time::Duration::days(WINDOW_DAYS))
        .ok_or_else(|| anyhow!("date underflow computing AP sync window"))?;
    Ok((from.format(&ISO_DATE)?, today.format(&ISO_DATE)?))
}

/// S191 — owned-arg variant called from inside `spawn_blocking`. The
/// pre-S191 `write_cycle_audit_entry(&CycleInputs, &CycleSummary)`
/// borrowed `inputs`, which the move-closure boundary forbids;
/// splitting the owned fields out keeps the move ergonomics clean
/// without a wrapping `Arc<CycleInputs>` clone.
fn write_cycle_audit_entry_inner(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator_login: &str,
    summary: &CycleSummary,
) -> Result<()> {
    let payload = IncomingInvoiceSyncCycleCompletedPayload {
        idempotency_key: IdempotencyKey::new().to_canonical_string(),
        trigger: summary.trigger.as_audit_str().to_string(),
        date_from: summary.date_from.clone(),
        date_to: summary.date_to.clone(),
        ingested_count: summary.ingested_count,
        skipped_count: summary.skipped_count,
        pages_walked: summary.pages_walked,
        elapsed_ms: summary.elapsed_ms,
        error: summary.error.clone(),
    };
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, operator_login);
    let ledger_meta = audit_ledger::LedgerMeta::new(tenant.clone(), binary_hash);

    let mut conn = duckdb::Connection::open(db_path).with_context(|| {
        format!(
            "open tenant DuckDB at {} for AP sync cycle audit entry",
            db_path.display()
        )
    })?;
    audit_ledger::ensure_schema(&conn)
        .context("ensure audit-ledger schema for AP sync cycle audit entry")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (AP sync cycle audit entry)")?;
    audit_ledger::append_in_tx(
        &tx,
        &ledger_meta,
        EventKind::IncomingInvoiceSyncCycleCompleted,
        payload.to_bytes(),
        actor,
        Some(payload.idempotency_key.clone()),
    )
    .map_err(|e| anyhow!("audit_ledger::append_in_tx IncomingInvoiceSyncCycleCompleted: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction (AP sync cycle audit entry)")?;
    drop(conn);

    let ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to verify chain after AP sync cycle entry")?;
    ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER AP sync cycle entry")?;
    let mirror_path = audit_ledger::mirror_path_for(db_path);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after AP sync cycle entry")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn fixture_digest_huf() -> InvoiceDigest {
        InvoiceDigest {
            invoice_number: "SUP-2026/0001".to_string(),
            supplier_tax_number: "12345678".to_string(),
            supplier_name: Some("Példa Kft.".to_string()),
            issue_date: Some("2026-05-10".to_string()),
            transaction_id: Some("TXN-001".to_string()),
            currency: Some("HUF".to_string()),
            invoice_net_amount: Some("100000.00".to_string()),
            invoice_vat_amount: Some("27000.00".to_string()),
        }
    }

    fn fixture_digest_eur() -> InvoiceDigest {
        InvoiceDigest {
            invoice_number: "SUP-EU-001".to_string(),
            supplier_tax_number: "87654321".to_string(),
            supplier_name: Some("Other GmbH".to_string()),
            issue_date: Some("2026-05-11".to_string()),
            transaction_id: Some("TXN-002".to_string()),
            currency: Some("EUR".to_string()),
            invoice_net_amount: Some("50.00".to_string()),
            invoice_vat_amount: Some("13.50".to_string()),
        }
    }

    #[test]
    fn digest_to_ingestion_input_handles_huf() {
        let input = digest_to_ingestion_input(&fixture_digest_huf()).expect("HUF digest");
        assert_eq!(input.currency, "HUF");
        assert_eq!(input.total_net_minor, 100_000);
        assert_eq!(input.total_vat_minor, 27_000);
        assert_eq!(input.total_gross_minor, 127_000);
        assert_eq!(input.supplier_name, "Példa Kft.");
        assert_eq!(input.nav_invoice_number, "SUP-2026/0001");
        assert!(input.nav_xml.is_none());
    }

    #[test]
    fn digest_to_ingestion_input_handles_eur_scales_to_cents() {
        let input = digest_to_ingestion_input(&fixture_digest_eur()).expect("EUR digest");
        assert_eq!(input.currency, "EUR");
        // 50.00 EUR -> 5000 cents; 13.50 EUR -> 1350 cents.
        assert_eq!(input.total_net_minor, 5_000);
        assert_eq!(input.total_vat_minor, 1_350);
        assert_eq!(input.total_gross_minor, 6_350);
    }

    #[test]
    fn digest_to_ingestion_input_rejects_unknown_currency() {
        let mut d = fixture_digest_huf();
        d.currency = Some("USD".to_string());
        let err = digest_to_ingestion_input(&d).expect_err("USD outside closed vocab");
        assert!(format!("{err:#}").contains("USD"), "{err:#}");
    }

    #[test]
    fn digest_to_ingestion_input_rejects_missing_currency() {
        let mut d = fixture_digest_huf();
        d.currency = None;
        let err = digest_to_ingestion_input(&d).expect_err("missing currency");
        assert!(format!("{err:#}").contains("missing <currency>"));
    }

    #[test]
    fn digest_to_ingestion_input_rejects_missing_issue_date() {
        let mut d = fixture_digest_huf();
        d.issue_date = None;
        let err = digest_to_ingestion_input(&d).expect_err("missing issue_date");
        assert!(format!("{err:#}").contains("invoiceIssueDate"));
    }

    #[test]
    fn digest_to_ingestion_input_rejects_missing_supplier_name() {
        let mut d = fixture_digest_huf();
        d.supplier_name = None;
        let err = digest_to_ingestion_input(&d).expect_err("missing supplier_name");
        assert!(format!("{err:#}").contains("supplierName"));
    }

    #[test]
    fn digest_to_ingestion_input_treats_absent_amounts_as_zero() {
        let mut d = fixture_digest_huf();
        d.invoice_net_amount = None;
        d.invoice_vat_amount = None;
        let input = digest_to_ingestion_input(&d).expect("zero amounts ok");
        assert_eq!(input.total_net_minor, 0);
        assert_eq!(input.total_vat_minor, 0);
        assert_eq!(input.total_gross_minor, 0);
    }

    #[test]
    fn decimal_to_minor_rounds_half_even_for_eur() {
        // Decimal::round defaults to half-even (banker's rounding).
        assert_eq!(decimal_to_minor("12.34", "EUR").unwrap(), 1234);
        assert_eq!(decimal_to_minor("12.345", "EUR").unwrap(), 1234);
        assert_eq!(decimal_to_minor("12.355", "EUR").unwrap(), 1236);
    }

    #[test]
    fn decimal_to_minor_truncates_decimals_for_huf() {
        // HUF has 0 decimal scale; fractional inputs round to whole forints.
        assert_eq!(decimal_to_minor("100", "HUF").unwrap(), 100);
        assert_eq!(decimal_to_minor("100.49", "HUF").unwrap(), 100);
        assert_eq!(decimal_to_minor("100.50", "HUF").unwrap(), 100); // half-even
        assert_eq!(decimal_to_minor("101.50", "HUF").unwrap(), 102); // half-even
    }

    #[test]
    fn decimal_to_minor_loud_fails_on_malformed_input() {
        let err = decimal_to_minor("not-a-number", "HUF").expect_err("must loud-fail");
        assert!(format!("{err:#}").contains("not a valid Decimal"));
    }

    #[test]
    fn compute_date_window_is_thirty_days_back() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let (from, to) = compute_date_window(now).unwrap();
        assert_eq!(to, "2026-05-30");
        assert_eq!(from, "2026-04-30");
    }

    /// S192 — `checked_sub` underflow surfaces as a typed loud-fail.
    /// PR-182 review's S178 🟢 flagged the `?` on
    /// `today.checked_sub(time::Duration::days(WINDOW_DAYS))` as a
    /// possible error path unreachable in practice but undocumented.
    /// At `time::Date::MIN`, subtracting 30 days underflows the
    /// representable date range, so the helper MUST surface the
    /// `"date underflow computing AP sync window"` anyhow error
    /// rather than silently clamping or panicking.
    ///
    /// CLAUDE.md rule 12 — loud-fail on unreachable-in-practice paths
    /// is the load-bearing contract: a future calendar-math refactor
    /// that swaps `checked_sub` for plain `-` would panic, and pinning
    /// the typed-error path forces the regressor to look at this test.
    #[test]
    fn compute_date_window_loud_fails_on_underflow_at_date_min() {
        // Build an OffsetDateTime whose `.date()` is exactly Date::MIN
        // (the lower bound of the `time` crate's representable range).
        // The 30-day subtraction is guaranteed to underflow.
        let now =
            time::PrimitiveDateTime::new(time::Date::MIN, time::Time::from_hms(0, 0, 0).unwrap())
                .assume_utc();
        let err = compute_date_window(now).expect_err("Date::MIN - 30 days must underflow");
        assert!(
            format!("{err:#}").contains("date underflow"),
            "underflow must surface as the documented loud-fail message; got: {err:#}"
        );
    }

    #[test]
    fn cycle_trigger_audit_strings_are_closed_vocab() {
        assert_eq!(CycleTrigger::Daemon.as_audit_str(), "daemon");
        assert_eq!(CycleTrigger::Manual.as_audit_str(), "manual");
    }
}
