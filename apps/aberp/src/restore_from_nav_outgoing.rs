//! NAV-as-DR restore for OUTGOING invoices — S180 / PR-180.
//!
//! Operator-triggered recovery surface for the catastrophic
//! local-DB-loss case: if the tenant's local DuckDB is gone (disk
//! failure, deletion, corruption past the audit-ledger mirror's
//! reach), NAV becomes the disaster-recovery SoT for invoice data
//! the tenant has issued. The wizard at
//! `POST /api/restore-from-nav-outgoing { year }` walks NAV's
//! `queryInvoiceDigest OUTBOUND` view for that year and mirrors
//! each discovered invoice into the local [`restored_invoice`]
//! table as a recovered view.
//!
//! # v1 scope cap (per the session-180 brief)
//!
//! v1 is INVOICES ONLY. Partner / product / modify-storno-chain
//! reconstruction is deferred to v2 — those surfaces are harder
//! (dedup, fuzzy-match for PRIVATE_PERSON without tax_number,
//! base-invoice id resolution across digests) and `dev-db-disposable`
//! names partners + products as "re-inputtable; restoring them
//! automatically is nice-to-have, not load-bearing."
//!
//! v1 is also **digest-only** — the wizard does NOT fan out
//! per-digest `queryInvoiceData` calls. The conservative call is
//! the same one S178 took with the AP auto-sync daemon: ship
//! digest-only ingestion (fewer NAV calls, no full-XML parser
//! coupling) and add the XML-fetch path in a focused follow-on
//! when an audit-evidence consumer needs the bytes. The brief
//! invited the conservative + flag posture explicitly.
//!
//! # Why a dedicated `restored_invoice` table (NOT the canonical `invoice`)
//!
//! The brief's literal reading is "INSERT into `invoice` table with
//! NEW ULID." The conservative + flag departure here is deliberate:
//!
//!   1. The canonical `invoice` table requires `customer_id NOT NULL`
//!      pointing into `partners` (which v1 explicitly defers to v2 —
//!      satisfying the FK would require minting sentinel partners,
//!      which IS the partner-extraction problem the brief defers).
//!   2. The canonical surface tracks `(series_id, fiscal_year,
//!      sequence_number)` UNIQUE under the gap-free allocator
//!      invariant. Direct-INSERT bypassing the allocator corrupts
//!      `invoice_sequence_state.next_number`; the next operator-issued
//!      invoice would collide. Repairing the allocator state
//!      post-restore IS additional complexity.
//!   3. Restored rows are a RECOVERED VIEW of what NAV holds, not a
//!      re-issuance on this tenant. Mixing them into the canonical
//!      surface would pollute: the per-OUTGOING-invoice export bundle
//!      (`invoice.*` audit-kind glob), the audit-chain
//!      stuck-precondition walker, and the printed-PDF render path
//!      (lines + customer FK + payment_method + currency exchange
//!      rates all required).
//!
//! Mirrors S177's clean separation: incoming → `ap_invoice`,
//! restored → `restored_invoice`. v2 can promote restored rows
//! into the canonical surface AFTER partner/product reconciliation
//! lands.
//!
//! # Pagination
//!
//! NAV's `queryInvoiceDigest` caps `dateFrom..dateTo` at 35 days.
//! The wizard walks the year month-by-month (12 chunks × N pages
//! each), guarded by the same [`MAX_PAGES_PER_MONTH`] safety cap
//! S178's daemon uses. A capped month logs `warn!` and contributes
//! to `errored_count` for that month so the silent-omission risk
//! surfaces loud per CLAUDE.md rule 12.
//!
//! # Idempotency
//!
//!   - Primary: walk the audit ledger backward for prior
//!     `InvoiceRestoredFromNav` entries; a match on
//!     `source_nav_invoice_number` means "already restored — skip."
//!     This is the source of truth per the session-180 brief.
//!   - Defence-in-depth: the `restored_invoice` table carries a
//!     UNIQUE constraint on `(tenant_id, source_nav_invoice_number)`
//!     so a code-path-level idempotency-check gap surfaces as a
//!     DuckDB constraint violation rather than a silent duplicate.
//!
//! Re-running the wizard 10 times produces an identical
//! `restored_invoice` state.
//!
//! # Audit
//!
//!   - ONE `InvoiceRestoredFromNav` entry per row inserted (NOT per
//!     digest seen — skipped digests do not emit an audit entry, so
//!     re-running the wizard does not pollute the ledger with N×K
//!     duplicate entries).
//!   - No per-cycle summary audit kind. The wizard is
//!     operator-paced; the {restored, skipped, errored} counts ride
//!     the HTTP response body, not the audit chain.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{self as audit_ledger, Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::IdempotencyKey;
use aberp_nav_transport::operations::query_invoice_digest::{
    self, InvoiceDigest, QueryInvoiceDigestPage,
};
use aberp_nav_transport::soap::InvoiceDirection;
use aberp_nav_transport::{NavCredentials, NavEndpoint, NavTransport};
use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::{format_description::FormatItem, macros, OffsetDateTime};
use ulid::Ulid;

use crate::audit_payloads::InvoiceRestoredFromNavPayload;

/// Earliest year the wizard accepts. NAV's Online Számla / data-
/// submission system went live in 2018; pre-2018 invoices were not
/// submitted to NAV at all and thus cannot be restored from it. A
/// year below this floor surfaces as a 400 — better than a NAV-side
/// no-data response the operator has to interpret as "did it work?"
pub const MIN_RESTORE_YEAR: i32 = 2018;

/// Per-month pagination cap (mirrors S178's `MAX_PAGES_PER_CYCLE`).
/// 100 pages × ~100 digests/page = 10K invoices per month — a safety
/// ceiling. A capped month logs `warn!` and contributes to
/// `errored_count` so the silent-truncation risk surfaces loud per
/// CLAUDE.md rule 12.
pub const MAX_PAGES_PER_MONTH: u32 = 100;

const ISO_DATE: &[FormatItem<'_>] = macros::format_description!("[year]-[month]-[day]");

/// `rinv_<26-char-ULID>` newtype mirroring `IncomingInvoiceId`'s
/// shape per ADR-0005 (per-entity prefixed ULIDs surface type
/// confusion at compile time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RestoredInvoiceId(pub Ulid);

impl RestoredInvoiceId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
    pub fn to_prefixed_string(self) -> String {
        format!("rinv_{}", self.0)
    }
}

impl Default for RestoredInvoiceId {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Schema.
// ──────────────────────────────────────────────────────────────────────

const RESTORED_INVOICE_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS restored_invoice (
    id                          VARCHAR NOT NULL PRIMARY KEY,
    tenant_id                   VARCHAR NOT NULL,
    source_nav_invoice_number   VARCHAR NOT NULL,
    source_nav_transaction_id   VARCHAR,
    issue_date                  VARCHAR NOT NULL,
    total_net_minor             BIGINT  NOT NULL,
    total_vat_minor             BIGINT  NOT NULL,
    total_gross_minor           BIGINT  NOT NULL,
    currency                    VARCHAR NOT NULL CHECK (currency IN ('HUF','EUR')),
    restore_year                INTEGER NOT NULL,
    created_at                  VARCHAR NOT NULL,
    UNIQUE (tenant_id, source_nav_invoice_number)
);
CREATE INDEX IF NOT EXISTS restored_invoice_tenant_year_idx
    ON restored_invoice (tenant_id, restore_year);
CREATE INDEX IF NOT EXISTS restored_invoice_tenant_issue_idx
    ON restored_invoice (tenant_id, issue_date);
";

/// Idempotent `CREATE TABLE IF NOT EXISTS`. Same boot-time
/// posture as `incoming_invoices::ensure_schema` /
/// `partners::ensure_schema`.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(RESTORED_INVOICE_SCHEMA_SQL)
        .context("ensure restored_invoice schema")
}

// ──────────────────────────────────────────────────────────────────────
// Read model.
// ──────────────────────────────────────────────────────────────────────

/// One restored row as it appears on the wire (list response item).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestoredInvoice {
    pub id: String,
    pub source_nav_invoice_number: String,
    pub source_nav_transaction_id: Option<String>,
    pub issue_date: String,
    pub total_net_minor: i64,
    pub total_vat_minor: i64,
    pub total_gross_minor: i64,
    pub currency: String,
    pub restore_year: i32,
    pub created_at: String,
}

/// List every restored invoice for the tenant, newest issue_date
/// first. Used by the wizard's "what's already restored" panel.
pub fn list_restored(db_path: &Path, tenant: &str) -> Result<Vec<RestoredInvoice>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("open tenant DuckDB at {}", db_path.display()))?;
    ensure_schema(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, source_nav_invoice_number, source_nav_transaction_id, issue_date,
                total_net_minor, total_vat_minor, total_gross_minor, currency,
                restore_year, created_at
           FROM restored_invoice
          WHERE tenant_id = ?
          ORDER BY issue_date DESC, source_nav_invoice_number DESC;",
    )?;
    let rows = stmt.query_map(params![tenant], |row| {
        Ok(RestoredInvoice {
            id: row.get(0)?,
            source_nav_invoice_number: row.get(1)?,
            source_nav_transaction_id: row.get(2)?,
            issue_date: row.get(3)?,
            total_net_minor: row.get(4)?,
            total_vat_minor: row.get(5)?,
            total_gross_minor: row.get(6)?,
            currency: row.get(7)?,
            restore_year: row.get(8)?,
            created_at: row.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// ──────────────────────────────────────────────────────────────────────
// Wizard entry-point.
// ──────────────────────────────────────────────────────────────────────

/// Inputs the HTTP route assembles. Mirrors `ap_sync::CycleInputs`'s
/// posture (built once by the route handler from `AppState` +
/// keychain).
pub struct RestoreInputs {
    pub db_path: PathBuf,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub tax_number_8: String,
    pub endpoint: NavEndpoint,
    pub credentials: NavCredentials,
    pub year: i32,
}

/// Persistent context for ONE digest's processing. Decoupled from
/// `RestoreInputs` so the unit tests can build a minimal context
/// without a full `NavCredentials` instance (which requires the
/// `test-support` feature on `aberp-nav-transport`, kept out of this
/// crate's dev-dependencies to mirror S178's posture).
struct DigestContext<'a> {
    db_path: &'a Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator_login: &'a str,
    year: i32,
}

impl<'a> DigestContext<'a> {
    fn from_inputs(inputs: &'a RestoreInputs) -> Self {
        Self {
            db_path: &inputs.db_path,
            tenant: inputs.tenant.clone(),
            binary_hash: inputs.binary_hash,
            operator_login: &inputs.operator_login,
            year: inputs.year,
        }
    }
}

/// One wizard run's summary. Returned to the HTTP route which echoes
/// the body verbatim to the SPA.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RestoreSummary {
    pub year: i32,
    pub restored: u64,
    pub skipped: u64,
    pub errored: u64,
    pub pages_walked: u32,
    pub elapsed_ms: u64,
}

/// Validate the operator-supplied year. Same loud-fail posture as
/// `incoming_invoices::validate_ingestion_input` — closed bounds,
/// no silent clamp.
pub fn validate_year(year: i32, now_utc: OffsetDateTime) -> Result<(), String> {
    let current_year = now_utc.date().year();
    if year < MIN_RESTORE_YEAR {
        return Err(format!(
            "year must be >= {MIN_RESTORE_YEAR} (NAV Online Számla went live in 2018; \
             pre-2018 invoices were never submitted to NAV)"
        ));
    }
    if year > current_year {
        return Err(format!(
            "year must be <= the current calendar year ({current_year}); \
             NAV cannot hold invoices issued in the future"
        ));
    }
    Ok(())
}

/// Run one operator-triggered restore wizard cycle. Walks the year
/// month-by-month against NAV, mirrors each new digest into
/// `restored_invoice`, returns the {restored, skipped, errored}
/// counts. Idempotent: a re-run returns `restored=0` for already-
/// seen NAV invoice numbers.
pub async fn run(inputs: RestoreInputs) -> Result<RestoreSummary> {
    let started = std::time::Instant::now();
    validate_year(inputs.year, OffsetDateTime::now_utc())
        .map_err(|m| anyhow!("invalid year {}: {m}", inputs.year))?;

    let transport = NavTransport::new(inputs.endpoint)
        .context("build NAV transport for restore-from-nav wizard")?;

    let mut total_restored: u64 = 0;
    let mut total_skipped: u64 = 0;
    let mut total_errored: u64 = 0;
    let mut total_pages: u32 = 0;

    for month in 1u8..=12 {
        let (date_from, date_to) = month_window(inputs.year, month)?;
        let (restored, skipped, errored, pages) =
            walk_month(&inputs, &transport, &date_from, &date_to).await?;
        total_restored += restored;
        total_skipped += skipped;
        total_errored += errored;
        total_pages += pages;
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(RestoreSummary {
        year: inputs.year,
        restored: total_restored,
        skipped: total_skipped,
        errored: total_errored,
        pages_walked: total_pages,
        elapsed_ms,
    })
}

async fn walk_month(
    inputs: &RestoreInputs,
    transport: &NavTransport,
    date_from: &str,
    date_to: &str,
) -> Result<(u64, u64, u64, u32)> {
    let ctx = DigestContext::from_inputs(inputs);
    let mut restored: u64 = 0;
    let mut skipped: u64 = 0;
    let mut errored: u64 = 0;
    let mut page: u32 = 1;

    loop {
        if page > MAX_PAGES_PER_MONTH {
            tracing::warn!(
                cap = MAX_PAGES_PER_MONTH,
                date_from = date_from,
                date_to = date_to,
                "restore-from-nav: month-window page cap hit; truncating — \
                 operator should narrow the year or contact support"
            );
            errored = errored.saturating_add(1);
            return Ok((restored, skipped, errored, page - 1));
        }

        let page_result: QueryInvoiceDigestPage = match query_invoice_digest::call(
            transport,
            &inputs.credentials,
            &inputs.tax_number_8,
            page,
            InvoiceDirection::Outbound,
            date_from,
            date_to,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    date_from = date_from,
                    date_to = date_to,
                    page = page,
                    error = ?e,
                    "restore-from-nav: queryInvoiceDigest failed; \
                     continuing to next month"
                );
                errored = errored.saturating_add(1);
                return Ok((restored, skipped, errored, page.saturating_sub(1)));
            }
        };

        let available_page = page_result.available_page;
        for digest in &page_result.digests {
            match process_digest(&ctx, digest) {
                Ok(ProcessOutcome::Restored) => restored += 1,
                Ok(ProcessOutcome::Skipped) => skipped += 1,
                Err(e) => {
                    tracing::warn!(
                        invoice_number = %digest.invoice_number,
                        error = ?e,
                        "restore-from-nav: digest processing failed; continuing"
                    );
                    errored += 1;
                }
            }
        }

        if page >= available_page {
            return Ok((restored, skipped, errored, page));
        }
        page += 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessOutcome {
    Restored,
    Skipped,
}

/// Process one digest: idempotency check via audit-ledger walk, then
/// INSERT + audit-write under one tx, then chain-verify + mirror-sync.
/// Returns `Skipped` when the audit-ledger walk finds a prior
/// `InvoiceRestoredFromNav` entry for the same `source_nav_invoice_number`.
fn process_digest(ctx: &DigestContext<'_>, digest: &InvoiceDigest) -> Result<ProcessOutcome> {
    if already_restored(
        ctx.db_path,
        ctx.tenant.clone(),
        ctx.binary_hash,
        &digest.invoice_number,
    )? {
        return Ok(ProcessOutcome::Skipped);
    }

    let issue_date = digest.issue_date.clone().ok_or_else(|| {
        anyhow!(
            "digest for invoice_number={} missing <invoiceIssueDate>",
            digest.invoice_number
        )
    })?;
    let currency = match digest.currency.as_deref() {
        Some("HUF") => "HUF".to_string(),
        Some("EUR") => "EUR".to_string(),
        Some(other) => {
            return Err(anyhow!(
                "digest for invoice_number={} carries currency `{}` outside closed vocab (HUF | EUR)",
                digest.invoice_number,
                other,
            ));
        }
        None => {
            return Err(anyhow!(
                "digest for invoice_number={} missing <currency>",
                digest.invoice_number
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

    let id = RestoredInvoiceId::new().to_prefixed_string();
    let idempotency_key = IdempotencyKey::new();
    let now = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("format restored_invoice created_at as Rfc3339")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, ctx.operator_login);
    let ledger_meta = audit_ledger::LedgerMeta::new(ctx.tenant.clone(), ctx.binary_hash);

    let mut conn = Connection::open(ctx.db_path).with_context(|| {
        format!(
            "open tenant DuckDB at {} for restored_invoice insert",
            ctx.db_path.display()
        )
    })?;
    ensure_schema(&conn).context("ensure restored_invoice schema (insert)")?;
    audit_ledger::ensure_schema(&conn).context("ensure audit-ledger schema (restore insert)")?;

    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (restored_invoice insert)")?;
    tx.execute(
        "INSERT INTO restored_invoice (
            id, tenant_id, source_nav_invoice_number, source_nav_transaction_id,
            issue_date, total_net_minor, total_vat_minor, total_gross_minor,
            currency, restore_year, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &id,
            ctx.tenant.as_str(),
            &digest.invoice_number,
            digest.transaction_id.as_deref(),
            &issue_date,
            net_minor,
            vat_minor,
            gross_minor,
            &currency,
            ctx.year,
            &now,
        ],
    )
    .context("INSERT into restored_invoice")?;

    let payload = InvoiceRestoredFromNavPayload {
        restored_invoice_id: id.clone(),
        idempotency_key: idempotency_key.to_canonical_string(),
        source_nav_invoice_number: digest.invoice_number.clone(),
        source_nav_transaction_id: digest.transaction_id.clone(),
        issue_date: issue_date.clone(),
        total_net_minor: net_minor,
        total_vat_minor: vat_minor,
        total_gross_minor: gross_minor,
        currency: currency.clone(),
        restore_year: ctx.year,
    };
    audit_ledger::append_in_tx(
        &tx,
        &ledger_meta,
        EventKind::InvoiceRestoredFromNav,
        payload.to_bytes(),
        actor,
        Some(idempotency_key.to_canonical_string()),
    )
    .map_err(|e| anyhow!("audit_ledger::append_in_tx InvoiceRestoredFromNav: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction (restored_invoice insert)")?;
    drop(conn);

    let ledger = Ledger::open(ctx.db_path, ctx.tenant.clone(), ctx.binary_hash)
        .context("open audit ledger to verify chain after restore insert")?;
    ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER restore insert")?;
    let mirror_path = audit_ledger::mirror_path_for(ctx.db_path);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after restore insert")?;

    Ok(ProcessOutcome::Restored)
}

/// Walk the audit ledger backward for the most-recent
/// `InvoiceRestoredFromNav` entry whose payload's
/// `source_nav_invoice_number` matches AND whose entry tenant_id
/// matches `tenant`.
///
/// `Ledger::entries()` returns every row in the underlying DuckDB
/// audit-ledger table regardless of tenant (the storage is a shared
/// per-DB table, multi-tenant by row column not by table). Tenant
/// scoping happens HERE at the consumer — same posture the rest of
/// `audit_query.rs`'s helpers take. Without this filter, a tenant-A
/// restore would mark tenant B's same NAV invoice number as
/// already-restored — the cross-tenant contamination failure mode
/// CLAUDE.md rule 12 names.
pub fn already_restored(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    source_nav_invoice_number: &str,
) -> Result<bool> {
    let ledger = Ledger::open(db_path, tenant.clone(), binary_hash)
        .context("open audit ledger for already_restored lookup")?;
    let entries = ledger
        .entries()
        .context("read audit ledger entries for already_restored lookup")?;
    for entry in entries.iter().rev() {
        if entry.kind != EventKind::InvoiceRestoredFromNav {
            continue;
        }
        if entry.tenant_id.as_str() != tenant.as_str() {
            continue;
        }
        let payload: InvoiceRestoredFromNavPayload = serde_json::from_slice(&entry.payload)
            .map_err(|e| {
                anyhow!(
                    "InvoiceRestoredFromNav payload (seq {:?}) failed typed decode: {e}",
                    entry.seq
                )
            })?;
        if payload.source_nav_invoice_number == source_nav_invoice_number {
            return Ok(true);
        }
    }
    Ok(false)
}

// ──────────────────────────────────────────────────────────────────────
// Helpers — date math + amount parsing.
// ──────────────────────────────────────────────────────────────────────

/// Build `(YYYY-MM-01, YYYY-MM-DD)` for month inside the year.
/// `DD` is the month's last day. NAV's `dateFrom`/`dateTo` are
/// inclusive per the v3.0 XSD.
pub fn month_window(year: i32, month: u8) -> Result<(String, String)> {
    let m: time::Month = match month {
        1 => time::Month::January,
        2 => time::Month::February,
        3 => time::Month::March,
        4 => time::Month::April,
        5 => time::Month::May,
        6 => time::Month::June,
        7 => time::Month::July,
        8 => time::Month::August,
        9 => time::Month::September,
        10 => time::Month::October,
        11 => time::Month::November,
        12 => time::Month::December,
        _ => return Err(anyhow!("month {month} outside 1..=12")),
    };
    let first = time::Date::from_calendar_date(year, m, 1)
        .map_err(|e| anyhow!("month_window first day for {year}-{month}: {e}"))?;
    let last_day = days_in_month(year, m);
    let last = time::Date::from_calendar_date(year, m, last_day)
        .map_err(|e| anyhow!("month_window last day for {year}-{month}: {e}"))?;
    Ok((first.format(&ISO_DATE)?, last.format(&ISO_DATE)?))
}

fn days_in_month(year: i32, month: time::Month) -> u8 {
    match month {
        time::Month::January
        | time::Month::March
        | time::Month::May
        | time::Month::July
        | time::Month::August
        | time::Month::October
        | time::Month::December => 31,
        time::Month::April | time::Month::June | time::Month::September | time::Month::November => {
            30
        }
        time::Month::February => {
            let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            if leap {
                29
            } else {
                28
            }
        }
    }
}

/// Convert NAV's decimal-as-string amount into minor units for the
/// closed-vocab currency. HUF has 0 decimals (forint is the minor
/// unit); EUR has 2 (cents). Identical shape to
/// `ap_sync::decimal_to_minor` (copied — extracting to a shared util
/// for two callers would widen the public surface for no real win).
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

// ──────────────────────────────────────────────────────────────────────
// Tests.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// Per-test tempdir under the system temp root. Mirrors the
    /// pattern in `incoming_invoices::tests::ScopedTempDir` —
    /// avoids the `tempfile` dev-dep so the surface stays tight per
    /// CLAUDE.md rule 2.
    struct ScopedTempDir(std::path::PathBuf);

    impl ScopedTempDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir()
                .join(format!("aberp-s180-restore-{label}-{pid}-{nanos}-{seq}"));
            std::fs::create_dir_all(&path).expect("create scoped tempdir");
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScopedTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_context<'a>(
        db_path: &'a Path,
        tenant_str: &str,
        operator: &'a str,
        year: i32,
    ) -> DigestContext<'a> {
        DigestContext {
            db_path,
            tenant: TenantId::new(tenant_str.to_string()).unwrap(),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            operator_login: operator,
            year,
        }
    }

    fn fixture_digest(invoice_number: &str, issue_date: &str) -> InvoiceDigest {
        InvoiceDigest {
            invoice_number: invoice_number.to_string(),
            supplier_tax_number: "12345678".to_string(),
            supplier_name: Some("Our Co Kft.".to_string()),
            issue_date: Some(issue_date.to_string()),
            transaction_id: Some(format!("TXN-{invoice_number}")),
            currency: Some("HUF".to_string()),
            invoice_net_amount: Some("100000".to_string()),
            invoice_vat_amount: Some("27000".to_string()),
        }
    }

    #[test]
    fn validate_year_rejects_below_floor() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let err = validate_year(2017, now).expect_err("pre-NAV year must reject");
        assert!(err.contains("2018"), "{err}");
    }

    #[test]
    fn validate_year_rejects_future() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let err = validate_year(2027, now).expect_err("future year must reject");
        assert!(err.contains("current calendar year"), "{err}");
    }

    #[test]
    fn validate_year_accepts_current_and_past() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        validate_year(2026, now).expect("current year ok");
        validate_year(2018, now).expect("floor year ok");
        validate_year(2025, now).expect("recent past ok");
    }

    #[test]
    fn month_window_january_2026() {
        let (from, to) = month_window(2026, 1).unwrap();
        assert_eq!(from, "2026-01-01");
        assert_eq!(to, "2026-01-31");
    }

    #[test]
    fn month_window_february_leap() {
        let (from, to) = month_window(2024, 2).unwrap();
        assert_eq!(from, "2024-02-01");
        assert_eq!(to, "2024-02-29");
    }

    #[test]
    fn month_window_february_non_leap() {
        let (from, to) = month_window(2026, 2).unwrap();
        assert_eq!(from, "2026-02-01");
        assert_eq!(to, "2026-02-28");
    }

    #[test]
    fn month_window_century_non_leap() {
        // Year 2100 is divisible by 100 but not by 400 → non-leap.
        let (from, to) = month_window(2100, 2).unwrap();
        assert_eq!(from, "2100-02-01");
        assert_eq!(to, "2100-02-28");
    }

    #[test]
    fn month_window_december() {
        let (from, to) = month_window(2026, 12).unwrap();
        assert_eq!(from, "2026-12-01");
        assert_eq!(to, "2026-12-31");
    }

    #[test]
    fn month_window_invalid_month_loud_fails() {
        let err = month_window(2026, 13).expect_err("month 13 invalid");
        assert!(format!("{err:#}").contains("outside 1..=12"));
    }

    #[test]
    fn restored_invoice_id_prefixes_with_rinv() {
        let id = RestoredInvoiceId::new();
        let s = id.to_prefixed_string();
        assert!(s.starts_with("rinv_"), "{s}");
        assert_eq!(s.len(), "rinv_".len() + 26);
    }

    #[test]
    fn decimal_to_minor_handles_huf_zero_scale() {
        assert_eq!(decimal_to_minor("100", "HUF").unwrap(), 100);
        assert_eq!(decimal_to_minor("100.49", "HUF").unwrap(), 100);
    }

    #[test]
    fn decimal_to_minor_handles_eur_two_scale() {
        assert_eq!(decimal_to_minor("12.34", "EUR").unwrap(), 1234);
        assert_eq!(decimal_to_minor("", "EUR").unwrap(), 0);
    }

    /// End-to-end happy path: process one digest, then process the
    /// SAME digest again. First call inserts + emits an audit entry;
    /// second call short-circuits via the audit-ledger idempotency
    /// walk and returns `Skipped`.
    #[test]
    fn process_digest_is_idempotent_via_audit_ledger() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);

        let d = fixture_digest("INV-default/00042", "2026-04-15");
        let outcome1 = process_digest(&ctx, &d).expect("first call inserts");
        assert!(matches!(outcome1, ProcessOutcome::Restored));

        let outcome2 = process_digest(&ctx, &d).expect("second call short-circuits");
        assert!(matches!(outcome2, ProcessOutcome::Skipped));

        let list = list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 1, "exactly one row after two calls");
        assert_eq!(list[0].source_nav_invoice_number, "INV-default/00042");
        assert_eq!(list[0].issue_date, "2026-04-15");
        assert_eq!(list[0].total_net_minor, 100_000);
        assert_eq!(list[0].total_vat_minor, 27_000);
        assert_eq!(list[0].total_gross_minor, 127_000);
        assert_eq!(list[0].currency, "HUF");
        assert_eq!(list[0].restore_year, 2026);

        // Verify the audit chain has exactly ONE InvoiceRestoredFromNav
        // entry (not two — the skipped re-run must not write a
        // duplicate).
        let ledger =
            Ledger::open(&db_path, ctx.tenant.clone(), ctx.binary_hash).expect("open ledger");
        let entries = ledger.entries().expect("read entries");
        let restored_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == EventKind::InvoiceRestoredFromNav)
            .collect();
        assert_eq!(restored_entries.len(), 1, "exactly one audit entry");
    }

    /// A digest carrying a currency outside the closed vocab loud-fails;
    /// the daemon-style continue-on-error happens at the WALK layer, not
    /// here. CLAUDE.md rule 12.
    #[test]
    fn process_digest_loud_fails_on_unknown_currency() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);

        let mut d = fixture_digest("INV-default/00099", "2026-05-01");
        d.currency = Some("USD".to_string());
        let err = process_digest(&ctx, &d).expect_err("USD outside closed vocab");
        assert!(format!("{err:#}").contains("USD"), "{err:#}");
    }

    /// A digest missing `<invoiceIssueDate>` surfaces loud-fail.
    #[test]
    fn process_digest_loud_fails_on_missing_issue_date() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);

        let mut d = fixture_digest("INV-default/00100", "2026-05-01");
        d.issue_date = None;
        let err = process_digest(&ctx, &d).expect_err("missing issue_date");
        assert!(format!("{err:#}").contains("invoiceIssueDate"));
    }

    /// `already_restored` MUST be tenant-scoped. A future refactor
    /// that loosens the scoping surfaces immediately via this pin.
    #[test]
    fn already_restored_is_tenant_scoped_by_ledger_open() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx_a = fixture_context(&db_path, "t1", "test-user", 2026);

        let d = fixture_digest("INV-default/00050", "2026-03-10");
        process_digest(&ctx_a, &d).expect("tenant A restores");

        let seen_b = already_restored(
            &db_path,
            TenantId::new("t2".to_string()).unwrap(),
            ctx_a.binary_hash,
            "INV-default/00050",
        )
        .expect("ledger lookup t2");
        assert!(
            !seen_b,
            "tenant B must not see tenant A's restored entry as already-restored"
        );

        let seen_a = already_restored(
            &db_path,
            ctx_a.tenant.clone(),
            ctx_a.binary_hash,
            "INV-default/00050",
        )
        .expect("ledger lookup t1");
        assert!(seen_a, "tenant A must see its own restored entry");
    }

    /// Two distinct digests both process cleanly; the listing
    /// reflects both with newest-issue-date-first ordering.
    #[test]
    fn process_digest_two_distinct_invoices() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);

        process_digest(&ctx, &fixture_digest("INV-default/00010", "2026-01-15"))
            .expect("first row");
        process_digest(&ctx, &fixture_digest("INV-default/00011", "2026-02-15"))
            .expect("second row");

        let list = list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 2);
        // Newest issue_date first.
        assert_eq!(list[0].source_nav_invoice_number, "INV-default/00011");
        assert_eq!(list[1].source_nav_invoice_number, "INV-default/00010");
    }
}
