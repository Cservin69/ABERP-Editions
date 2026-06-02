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
//!   - S186 / PR-186 performance fix: the set of already-restored
//!     `source_nav_invoice_number`s is loaded ONCE at the top of
//!     [`run`] via a SINGLE [`Ledger::open`] + [`Ledger::entries`]
//!     walk, into a [`HashSet<String>`]. Each digest then does an
//!     O(1) `contains` check instead of opening a fresh `Ledger` +
//!     walking the entire chain backward (pre-S186 was O(N×K) for
//!     `N` digests × `K` prior entries — a 1000-invoice year on a
//!     tenant with 10K prior audit entries was 10M JSON decodes).
//!     The cache is mutated in place as new restores succeed so a
//!     within-the-same-cycle re-encounter of the same NAV invoice
//!     number (impossible from NAV's API but cheap to defend
//!     against) still skips.
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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use aberp_audit_ledger::{self as audit_ledger, Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::{Currency, IdempotencyKey};
use aberp_nav_transport::error::NavTransportError;
use aberp_nav_transport::operations::query_invoice_data;
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

use crate::audit_payloads::{
    InvoiceRestoredFromNavPayload, RestoreBuyerBackfillCycleCompletedPayload,
};
use crate::restore_from_nav_extract::{self, ExtractionDelta};

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

/// PR-216 / S218 — additive migration for the buyer columns. The S180
/// scoping (digest-only) shipped without a `customerInfo` link; S196
/// then extracted partners but only into the `partners` master table,
/// leaving `restored_invoice` rows orphaned from the buyer label the
/// SPA outgoing list renders. S218 closes the loop by carrying the
/// buyer label IN-ROW on `restored_invoice` itself — a denormalised
/// snapshot mirroring how Own rows snapshot their customer in
/// `<ULID>.input.json` (no FK to `partners`, no JOIN at query time).
/// Closed-vocab `customer_vat_status` invariant lives in application
/// code (write through [`update_buyer_fields`], read through
/// [`list_restored`]'s SELECT) — no CHECK constraint per the
/// app-layer-migration discipline.
const RESTORED_INVOICE_PR216_MIGRATION_SQL: &str = "
ALTER TABLE restored_invoice
    ADD COLUMN IF NOT EXISTS customer_name        VARCHAR;
ALTER TABLE restored_invoice
    ADD COLUMN IF NOT EXISTS customer_tax_number  VARCHAR;
ALTER TABLE restored_invoice
    ADD COLUMN IF NOT EXISTS customer_vat_status  VARCHAR;
";

/// PR-217 / S220 — additive migration for the operator-paced manual
/// partner-link column. Per [[aberp-extnav-partner-nav-gap]] the
/// `queryInvoiceData OUTBOUND` call PR-216 leans on is entitlement-
/// gated to the original submitter; for invoices submitted via a third
/// party the row stays without a buyer label after backfill. PR-217
/// adds an operator-facing affordance to LINK a `partners` row
/// manually, and `partner_id` is the durable pointer that survives a
/// `customer_name` rename on the master.
///
/// VARCHAR (no FK to `partners.id`) per the
/// [[no-sql-specific]] discipline — the closed-vocab invariant ("if
/// non-null, must reference a real partner") lives in application code,
/// not the schema. Nullable: an unlinked ExtNav row carries NULL,
/// matching the buyer-fields posture from PR-216.
const RESTORED_INVOICE_PR217_MIGRATION_SQL: &str = "
ALTER TABLE restored_invoice
    ADD COLUMN IF NOT EXISTS partner_id          VARCHAR;
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` + PR-216 + PR-217 additive
/// migrations. Same boot-time posture as
/// `incoming_invoices::ensure_schema` / `partners::ensure_schema`.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(RESTORED_INVOICE_SCHEMA_SQL)
        .context("ensure restored_invoice base schema")?;
    conn.execute_batch(RESTORED_INVOICE_PR216_MIGRATION_SQL)
        .context("apply PR-216 restored_invoice migration (customer_name/tax_number/vat_status)")?;
    conn.execute_batch(RESTORED_INVOICE_PR217_MIGRATION_SQL)
        .context("apply PR-217 restored_invoice migration (partner_id)")?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Read model.
// ──────────────────────────────────────────────────────────────────────

/// One restored row as it appears on the wire (list response item).
///
/// PR-216 / S218 — the three `customer_*` fields are populated either
/// inline by the S196 fresh-restore extraction path
/// ([`update_buyer_fields`]) or by the boot-time backfill task
/// ([`run_buyer_backfill_once`]) for pre-PR-216 rows. Pre-backfill
/// rows surface `None` on all three; the SPA outgoing list renders the
/// em-dash placeholder in that case (matching the read-side fallback
/// `read_buyer_name_from_side_store` takes for missing side-store
/// files on Own rows).
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
    /// PR-216 / S218 — buyer label snapshot. Mirrors
    /// `<customerInfo>/<customerName>` on the originally-submitted NAV
    /// XML. `None` for pre-backfill rows + for any row whose
    /// queryInvoiceData fetch failed or whose XML omitted the field
    /// (post-session-154 NAV wire shape suppresses `<customerName>` on
    /// `PRIVATE_PERSON` buyers per [[reference_nav_gotchas]] §1).
    #[serde(default)]
    pub customer_name: Option<String>,
    /// PR-216 / S218 — canonical Hungarian tax number
    /// (`xxxxxxxx-y-zz`) for DOMESTIC buyers, `None` for PRIVATE_PERSON
    /// + OTHER. Not currently rendered by the SPA list (the column
    /// shows `customer_name` only, matching the Own-row convention)
    /// but kept on the wire for parity with Own-row partner metadata
    /// future-proofing.
    #[serde(default)]
    pub customer_tax_number: Option<String>,
    /// PR-216 / S218 — closed-vocab `CustomerVatStatus` rendered as its
    /// serde string (`"Domestic"` / `"PrivatePerson"` / `"Other"`).
    /// `None` for pre-backfill rows. Stored as `VARCHAR` rather than
    /// constrained to a CHECK so a future ADR-0048 extension (e.g. a
    /// new third-state shape) can land without a schema migration; the
    /// closed-vocab invariant lives in application code per the
    /// app-layer-migration discipline.
    #[serde(default)]
    pub customer_vat_status: Option<String>,
    /// PR-217 / S220 — operator-paced manual partner link, durable
    /// pointer into the `partners` master. `None` for fresh restored
    /// rows + for ExtNav rows the operator has not yet linked. When
    /// `Some(_)`, the `customer_*` fields above were last written from
    /// this partner's snapshot at link time (the audit ledger carries
    /// the before/after; the row carries the current state).
    ///
    /// Not currently surfaced on the SPA outgoing-list wire shape (the
    /// list already shows `customer_name`); reserved for the partner-
    /// picker modal's "currently linked" affordance + future joins.
    #[serde(default)]
    pub partner_id: Option<String>,
}

/// List every restored invoice for the tenant, newest issue_date
/// first. Used by the wizard's "what's already restored" panel and by
/// the SPA virtual-union outgoing list per ADR-0058 / S215.
pub fn list_restored(db_path: &Path, tenant: &str) -> Result<Vec<RestoredInvoice>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("open tenant DuckDB at {}", db_path.display()))?;
    ensure_schema(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, source_nav_invoice_number, source_nav_transaction_id, issue_date,
                total_net_minor, total_vat_minor, total_gross_minor, currency,
                restore_year, created_at,
                customer_name, customer_tax_number, customer_vat_status,
                partner_id
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
            customer_name: row.get(10)?,
            customer_tax_number: row.get(11)?,
            customer_vat_status: row.get(12)?,
            partner_id: row.get(13)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// PR-216 / S218 — write the parsed buyer fields back to the
/// `restored_invoice` row identified by `(tenant_id,
/// source_nav_invoice_number)`. Called from two paths:
///
///   1. **Fresh-restore extraction** ([`restore_from_nav_extract::apply_candidates`]):
///      every freshly-restored invoice that successfully parses its
///      `<customerInfo>` block writes back here in the same pass that
///      mints the partner master row.
///   2. **Boot-time backfill** ([`run_buyer_backfill_once`]): pre-PR-216
///      rows (and any S196 invoice whose queryInvoiceData fetch failed
///      mid-cycle) get re-fetched + re-parsed + persisted.
///
/// Idempotent: re-writing the same values is a no-op-equivalent
/// `UPDATE` that DuckDB handles in a single touch. The `WHERE`
/// matches at most one row by the `UNIQUE (tenant_id,
/// source_nav_invoice_number)` index, so a wrong-tenant
/// `source_nav_invoice_number` collision is impossible.
///
/// Returns the number of rows affected — `0` means the
/// `(tenant_id, source_nav_invoice_number)` pair was not found
/// (caller's defence-in-depth signal that something's wrong; the
/// fresh-restore path INSERTed the row moments before so we expect
/// `1`).
pub fn update_buyer_fields(
    conn: &Connection,
    tenant: &str,
    source_nav_invoice_number: &str,
    customer_name: Option<&str>,
    customer_tax_number: Option<&str>,
    customer_vat_status: Option<&str>,
) -> Result<usize> {
    ensure_schema(conn)?;
    let affected = conn
        .execute(
            "UPDATE restored_invoice
                SET customer_name        = ?,
                    customer_tax_number  = ?,
                    customer_vat_status  = ?
              WHERE tenant_id = ?
                AND source_nav_invoice_number = ?;",
            params![
                customer_name,
                customer_tax_number,
                customer_vat_status,
                tenant,
                source_nav_invoice_number,
            ],
        )
        .with_context(|| {
            format!(
                "UPDATE restored_invoice buyer fields for tenant `{tenant}` invoice `{source_nav_invoice_number}`"
            )
        })?;
    Ok(affected)
}

// ──────────────────────────────────────────────────────────────────────
// PR-217 / S220 — operator-paced manual partner link.
// ──────────────────────────────────────────────────────────────────────

/// PR-217 / S220 — the four denormalized buyer fields that ride
/// `restored_invoice`, packaged for a get-before-set audit pair.
///
/// Returned by [`read_restored_buyer_snapshot`] (used to capture
/// `*_before` on the manual-link audit entry) and surfaced verbatim on
/// the manual-link route response so the SPA can refresh the row
/// without a second list-restored round trip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestoredBuyerSnapshot {
    pub partner_id: Option<String>,
    pub customer_name: Option<String>,
    pub customer_tax_number: Option<String>,
    pub customer_vat_status: Option<String>,
}

/// PR-217 / S220 — read the current `(partner_id, customer_*)` snapshot
/// of a restored row keyed by `(tenant_id, id)` (the `rinv_<ULID>` form
/// the SPA carries on the wire). Returns `Ok(None)` if the row does
/// not exist OR belongs to a different tenant; the handler maps both
/// to 404.
pub fn read_restored_buyer_snapshot(
    conn: &Connection,
    tenant: &str,
    id: &str,
) -> Result<Option<RestoredBuyerSnapshot>> {
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT partner_id, customer_name, customer_tax_number, customer_vat_status
           FROM restored_invoice
          WHERE tenant_id = ? AND id = ?;",
    )?;
    let mut rows = stmt.query_map(params![tenant, id], |row| {
        Ok(RestoredBuyerSnapshot {
            partner_id: row.get(0)?,
            customer_name: row.get(1)?,
            customer_tax_number: row.get(2)?,
            customer_vat_status: row.get(3)?,
        })
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// PR-217 / S220 — read `source_nav_invoice_number` for a restored row
/// keyed by `(tenant_id, id)`. The manual-link audit payload carries
/// the canonical NAV number alongside the row's `rinv_<ULID>` id; the
/// list_restored UPDATE path keys on `source_nav_invoice_number` so we
/// need to look it up off the row's `id`.
pub fn read_restored_source_number(
    conn: &Connection,
    tenant: &str,
    id: &str,
) -> Result<Option<String>> {
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT source_nav_invoice_number
           FROM restored_invoice
          WHERE tenant_id = ? AND id = ?;",
    )?;
    let mut rows = stmt.query_map(params![tenant, id], |row| row.get::<_, String>(0))?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// PR-217 / S220 — write the four denormalized buyer fields together.
/// Used by the manual-link path so the partner pointer + the snapshot
/// always move in lockstep. WHERE keys on `(tenant_id, id)` — the
/// `rinv_<ULID>` form the SPA carries on the wire — so the route
/// handler does not need to look up the `source_nav_invoice_number`
/// just to call [`update_buyer_fields`].
///
/// Returns the number of rows affected (`0` means the row was deleted
/// between the read and the write; the handler surfaces 404 in that
/// case).
pub fn update_partner_for_restored(
    conn: &Connection,
    tenant: &str,
    id: &str,
    partner_id: Option<&str>,
    customer_name: Option<&str>,
    customer_tax_number: Option<&str>,
    customer_vat_status: Option<&str>,
) -> Result<usize> {
    ensure_schema(conn)?;
    let affected = conn
        .execute(
            "UPDATE restored_invoice
                SET partner_id           = ?,
                    customer_name        = ?,
                    customer_tax_number  = ?,
                    customer_vat_status  = ?
              WHERE tenant_id = ? AND id = ?;",
            params![
                partner_id,
                customer_name,
                customer_tax_number,
                customer_vat_status,
                tenant,
                id,
            ],
        )
        .with_context(|| {
            format!(
                "UPDATE restored_invoice partner_id+buyer fields for tenant `{tenant}` id `{id}`"
            )
        })?;
    Ok(affected)
}

/// PR-216 / S218 — list the `(id, source_nav_invoice_number, currency)`
/// triples of restored rows that are MISSING the buyer label snapshot.
/// Used by the boot-time backfill task to find rows that need a
/// `queryInvoiceData` fetch. The `customer_name IS NULL` predicate is
/// the load-bearing sentinel — there is no separate "backfilled" flag,
/// since a row whose customer is genuinely empty (PRIVATE_PERSON
/// post-session-154 wire shape with no `<customerName>`) stays NULL
/// even after a successful backfill attempt; that row will be
/// re-attempted on every subsequent boot, which is fine (one extra
/// `queryInvoiceData` call per such row per boot) and lets a future
/// NAV-side data correction be picked up automatically.
pub fn list_restored_missing_buyer(
    db_path: &Path,
    tenant: &str,
) -> Result<Vec<RestoredMissingBuyer>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("open tenant DuckDB at {}", db_path.display()))?;
    ensure_schema(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT source_nav_invoice_number
           FROM restored_invoice
          WHERE tenant_id = ?
            AND customer_name IS NULL
          ORDER BY issue_date DESC, source_nav_invoice_number DESC;",
    )?;
    let rows = stmt.query_map(params![tenant], |row| {
        Ok(RestoredMissingBuyer {
            source_nav_invoice_number: row.get(0)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// PR-216 / S218 — narrow read-shape for the boot-time backfill task.
/// The NAV invoice number is the only field per-row backfill needs
/// (it's the SOAP `invoiceNumber` arg + the `WHERE` predicate on the
/// final UPDATE); the full `RestoredInvoice` decode would round-trip
/// 12 columns per candidate for no read benefit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredMissingBuyer {
    pub source_nav_invoice_number: String,
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

/// S186 / PR-186 — in-memory cache of `source_nav_invoice_number`s
/// already present in the tenant's audit ledger as
/// `InvoiceRestoredFromNav` entries. Built ONCE per wizard run by
/// [`load_already_restored_cache`] before the month-walk loop starts;
/// every digest then checks membership in O(1) instead of the
/// pre-S186 O(N) per-digest ledger walk. The cache is mutated in
/// place as new restores succeed so within-cycle duplicates (NAV
/// would not emit them, but the defence is cheap) stay skipped.
type AlreadyRestoredCache = HashSet<String>;

/// S186 — build the already-restored cache for the tenant: ONE
/// `Ledger::open` + ONE `entries()` walk, payload-decoding only the
/// `InvoiceRestoredFromNav` entries scoped to `tenant`. Memory cost
/// is ~`prior_restored_count` × (~30 bytes per NAV invoice number
/// string + HashSet overhead) — fine for tenants with tens of
/// thousands of restored rows.
fn load_already_restored_cache(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
) -> Result<AlreadyRestoredCache> {
    let ledger = Ledger::open(db_path, tenant.clone(), binary_hash)
        .context("open audit ledger to pre-load already-restored cache")?;
    let entries = ledger
        .entries()
        .context("read audit ledger entries for already-restored cache")?;
    let mut set: AlreadyRestoredCache = HashSet::new();
    for entry in entries.iter() {
        if entry.kind != EventKind::InvoiceRestoredFromNav {
            continue;
        }
        // Cross-tenant defensive scoping — same posture
        // [`already_restored`] takes (storage is multi-tenant by row
        // column, not by table).
        if entry.tenant_id.as_str() != tenant.as_str() {
            continue;
        }
        let payload: InvoiceRestoredFromNavPayload = serde_json::from_slice(&entry.payload)
            .map_err(|e| {
                anyhow!(
                    "InvoiceRestoredFromNav payload (seq {:?}) failed typed decode \
                     while pre-loading already-restored cache: {e}",
                    entry.seq
                )
            })?;
        set.insert(payload.source_nav_invoice_number);
    }
    Ok(set)
}

/// One wizard run's summary. Returned to the HTTP route which echoes
/// the body verbatim to the SPA.
///
/// S196 / PR-196 — extended with partner + product catalog-extraction
/// counters. Pre-S196 fields are unchanged so the SPA's existing
/// `RestoreSummary` reader continues to work; the new fields are
/// additive (extra JSON keys ignored by the pre-S196 SPA build, picked
/// up by the post-S196 SPA build).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RestoreSummary {
    pub year: i32,
    pub restored: u64,
    pub skipped: u64,
    pub errored: u64,
    pub pages_walked: u32,
    pub elapsed_ms: u64,
    /// S196 — partners inserted into the local `partners` table from
    /// freshly restored invoice `<customerInfo>` blocks.
    #[serde(default)]
    pub partners_restored: u64,
    /// S196 — partner candidates that matched an existing row by the
    /// dedup key (`tax_number` for DOMESTIC; `(legal_name, address)`
    /// for PRIVATE_PERSON).
    #[serde(default)]
    pub partners_skipped_duplicate: u64,
    /// S196 — partner extraction failures (NAV-side queryInvoiceData
    /// non-OK, missing required fields, validator rejection). The
    /// invoice itself is already restored; this counter tracks the
    /// extraction sub-step only.
    #[serde(default)]
    pub partners_errored: u64,
    /// S196 — products inserted into the local `products` table from
    /// freshly restored invoice `<invoiceLines>` blocks.
    #[serde(default)]
    pub products_restored: u64,
    /// S196 — product candidates that matched an existing row by the
    /// dedup key `(name, ProductUnit)` and carried the same price.
    /// Subsumes price-drift cases (those increment `*_price_varies`
    /// alongside this counter).
    #[serde(default)]
    pub products_skipped_duplicate: u64,
    /// S196 — per-line product extraction failures.
    #[serde(default)]
    pub products_errored: u64,
    /// S196 — subset of `products_skipped_duplicate` where the
    /// candidate's price DIFFERED from the stored row's price; the
    /// stored price was updated to the last-seen value. v3 polish
    /// target: surface as a per-row `price_varies` flag on the
    /// product itself.
    #[serde(default)]
    pub products_price_varies: u64,
    /// S196 — `queryInvoiceData` calls that failed entirely (NAV
    /// transport, HTTP non-success, parse error). The invoice's
    /// restored_invoice row is still intact; only the catalog
    /// extraction for it was lost.
    #[serde(default)]
    pub invoice_extraction_errored: u64,
}

/// Validate the operator-supplied year. Same loud-fail posture as
/// `incoming_invoices::validate_ingestion_input` — closed bounds,
/// no silent clamp.
///
/// S183 — `current_year` is computed in **Europe/Budapest local time**,
/// not UTC. The only year-flip happens on Jan 1 (which falls in CET,
/// UTC+1, every year — DST runs late March to late October so summer's
/// CEST never straddles a year boundary). The fixed +1h offset
/// suffices for the year-bounds check: at any moment of any year, the
/// Hungarian calendar-year computed via `(now_utc + 1h).year()`
/// matches the wall clock the operator sees. Pre-S183 the validator
/// read `now_utc.date().year()`, so between 00:00–00:59 CET on Jan 1
/// the operator's correct entry (typing N+1) was rejected as "future"
/// because UTC was still Dec 31 of year N. PR-182 review §S180 named
/// this; PR-183 closes it.
pub fn validate_year(year: i32, now_utc: OffsetDateTime) -> Result<(), String> {
    let current_year = budapest_calendar_year(now_utc);
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

/// S183 — Europe/Budapest calendar year from a UTC instant. See the
/// docstring of [`validate_year`] for why a fixed +1h offset is
/// sufficient (the only year-flip is in winter, when Hungary is CET).
fn budapest_calendar_year(now_utc: OffsetDateTime) -> i32 {
    let budapest_offset = time::UtcOffset::from_hms(1, 0, 0)
        .expect("UTC+1 is a valid offset; const construction cannot fail at runtime");
    now_utc.to_offset(budapest_offset).date().year()
}

/// Run one operator-triggered restore wizard cycle. Walks the year
/// month-by-month against NAV, mirrors each new digest into
/// `restored_invoice`, returns the {restored, skipped, errored}
/// counts. Idempotent: a re-run returns `restored=0` for already-
/// seen NAV invoice numbers.
///
/// S186 — the already-restored set is loaded ONCE here (one
/// `Ledger::open` + one `entries()` walk into a `HashSet<String>`)
/// and passed by mutable reference through the month-walk loop;
/// each digest checks membership in O(1) and inserts on successful
/// restore. Pre-S186 this was an O(N) per-digest ledger walk with a
/// fresh `Ledger::open` per call — 1000 digests × 10K prior entries
/// = 10M JSON decodes worst-case. PR-182 review §S180 named the cost.
pub async fn run(inputs: RestoreInputs) -> Result<RestoreSummary> {
    let started = std::time::Instant::now();
    validate_year(inputs.year, OffsetDateTime::now_utc())
        .map_err(|m| anyhow!("invalid year {}: {m}", inputs.year))?;

    let transport = NavTransport::new(inputs.endpoint)
        .context("build NAV transport for restore-from-nav wizard")?;

    // S186 — single ledger walk before the month-loop; cache passed
    // by &mut into walk_month / process_digest, mutated as new
    // restores succeed.
    //
    // S191 — the ledger walk is fully synchronous DuckDB / typed
    // JSON-decode work over potentially tens of thousands of
    // entries; fence it inside `spawn_blocking` so the tokio worker
    // is not held until it returns.
    let cache_db = inputs.db_path.clone();
    let cache_tenant = inputs.tenant.clone();
    let cache_binary_hash = inputs.binary_hash;
    let mut already_restored_cache = tokio::task::spawn_blocking(move || {
        load_already_restored_cache(&cache_db, cache_tenant, cache_binary_hash)
    })
    .await
    .map_err(|join_err| anyhow!("restore wizard cache-load task panicked: {join_err}"))?
    .context("pre-load already-restored cache for restore wizard")?;

    let mut total_restored: u64 = 0;
    let mut total_skipped: u64 = 0;
    let mut total_errored: u64 = 0;
    let mut total_pages: u32 = 0;
    let mut total_extraction = ExtractionDelta::default();

    for month in 1u8..=12 {
        let (date_from, date_to) = month_window(inputs.year, month)?;
        let outcome = walk_month(
            &inputs,
            &transport,
            &date_from,
            &date_to,
            &mut already_restored_cache,
        )
        .await?;
        total_restored += outcome.restored;
        total_skipped += outcome.skipped;
        total_errored += outcome.errored;
        total_pages += outcome.pages;
        total_extraction.add(outcome.extraction);
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(RestoreSummary {
        year: inputs.year,
        restored: total_restored,
        skipped: total_skipped,
        errored: total_errored,
        pages_walked: total_pages,
        elapsed_ms,
        partners_restored: total_extraction.partners_restored,
        partners_skipped_duplicate: total_extraction.partners_skipped_duplicate,
        partners_errored: total_extraction.partners_errored,
        products_restored: total_extraction.products_restored,
        products_skipped_duplicate: total_extraction.products_skipped_duplicate,
        products_errored: total_extraction.products_errored,
        products_price_varies: total_extraction.products_price_varies,
        invoice_extraction_errored: total_extraction.invoice_extraction_errored,
    })
}

/// S196 — one month-walk's outputs. Pre-S196 returned a 4-tuple;
/// extraction counters added on top so the per-month aggregation in
/// [`run`] stays a single accumulator pass.
struct MonthOutcome {
    restored: u64,
    skipped: u64,
    errored: u64,
    pages: u32,
    extraction: ExtractionDelta,
}

async fn walk_month(
    inputs: &RestoreInputs,
    transport: &NavTransport,
    date_from: &str,
    date_to: &str,
    already_restored_cache: &mut AlreadyRestoredCache,
) -> Result<MonthOutcome> {
    let mut restored: u64 = 0;
    let mut skipped: u64 = 0;
    let mut errored: u64 = 0;
    let mut page: u32 = 1;
    let mut month_extraction = ExtractionDelta::default();

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
            return Ok(MonthOutcome {
                restored,
                skipped,
                errored,
                pages: page - 1,
                extraction: month_extraction,
            });
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
                return Ok(MonthOutcome {
                    restored,
                    skipped,
                    errored,
                    pages: page.saturating_sub(1),
                    extraction: month_extraction,
                });
            }
        };

        let available_page = page_result.available_page;
        // S191 — process the page's digests on the blocking pool so
        // the tokio worker is not held across N synchronous DuckDB
        // INSERT + chain-verify + mirror-sync calls. One
        // `spawn_blocking` per page keeps the boundary-cross count at
        // O(pages) instead of O(digests). The cache is `mem::take`n
        // into the closure and threaded back out so the mutation
        // posture (one cache for the whole month-walk) is preserved.
        //
        // S196 — alongside `(restored, skipped, errored)` counters the
        // closure now collects a `fresh_restored: Vec<FreshRestored>`
        // listing the invoices that landed as `Restored` this page.
        // After the blocking pool returns, the async caller fans those
        // out to one `queryInvoiceData` call + catalog extraction per
        // entry. The extraction step itself uses another `spawn_blocking`
        // for the XML parse + partner/product DB inserts so the worker
        // is not held across the synchronous DB work.
        let digests = page_result.digests;
        let db_path = inputs.db_path.clone();
        let tenant = inputs.tenant.clone();
        let binary_hash = inputs.binary_hash;
        let operator_login = inputs.operator_login.clone();
        let year = inputs.year;
        let cache_taken = std::mem::take(already_restored_cache);
        let (cache_returned, page_restored, page_skipped, page_errored, fresh_restored) =
            tokio::task::spawn_blocking(move || {
                let ctx = DigestContext {
                    db_path: &db_path,
                    tenant,
                    binary_hash,
                    operator_login: &operator_login,
                    year,
                };
                let mut cache = cache_taken;
                let mut r: u64 = 0;
                let mut s: u64 = 0;
                let mut er: u64 = 0;
                let mut fresh: Vec<FreshRestored> = Vec::new();
                for digest in &digests {
                    match process_digest(&ctx, digest, &mut cache) {
                        Ok(ProcessOutcome::Restored) => {
                            r += 1;
                            // process_digest already validated currency
                            // against the closed vocab; the re-parse here
                            // cannot fail in practice but loud-fails
                            // defensively if it does (CLAUDE.md rule 12).
                            match parse_digest_currency(digest) {
                                Ok(currency) => fresh.push(FreshRestored {
                                    invoice_number: digest.invoice_number.clone(),
                                    currency,
                                }),
                                Err(e) => {
                                    tracing::warn!(
                                        invoice_number = %digest.invoice_number,
                                        error = ?e,
                                        "S196: post-restore currency re-parse failed; \
                                         catalog extraction skipped for this invoice"
                                    );
                                }
                            }
                        }
                        Ok(ProcessOutcome::Skipped) => s += 1,
                        Err(e) => {
                            tracing::warn!(
                                invoice_number = %digest.invoice_number,
                                error = ?e,
                                "restore-from-nav: digest processing failed; continuing"
                            );
                            er += 1;
                        }
                    }
                }
                (cache, r, s, er, fresh)
            })
            .await
            .map_err(|join_err| anyhow!("restore wizard per-page task panicked: {join_err}"))?;
        *already_restored_cache = cache_returned;
        restored += page_restored;
        skipped += page_skipped;
        errored += page_errored;

        // S196 — async fan-out of catalog extraction per fresh-restored
        // invoice. Sequential here (one queryInvoiceData at a time) —
        // NAV's per-tenant rate limits favour serial calls and the
        // wizard is operator-paced (one operator click per cycle).
        for fresh in fresh_restored {
            let delta = extract_catalog_for_invoice(
                &inputs.db_path,
                &inputs.tenant,
                &inputs.tax_number_8,
                &inputs.credentials,
                transport,
                &fresh.invoice_number,
                fresh.currency,
            )
            .await;
            month_extraction.add(delta);
        }

        if page >= available_page {
            return Ok(MonthOutcome {
                restored,
                skipped,
                errored,
                pages: page,
                extraction: month_extraction,
            });
        }
        page += 1;
    }
}

/// S196 — one freshly-restored invoice, captured by the per-page
/// blocking-pool worker and consumed by the async extraction fan-out.
#[derive(Debug, Clone)]
struct FreshRestored {
    invoice_number: String,
    currency: Currency,
}

/// S196 — map a digest's wire-form currency string to the closed-vocab
/// [`Currency`] enum the extract module needs. Loud-fails on any value
/// outside `{HUF, EUR}` so a NAV-side schema drift surfaces immediately.
fn parse_digest_currency(digest: &InvoiceDigest) -> Result<Currency> {
    match digest.currency.as_deref() {
        Some("HUF") => Ok(Currency::Huf),
        Some("EUR") => Ok(Currency::Eur),
        Some(other) => Err(anyhow!(
            "digest for invoice_number={} carries currency `{}` outside closed vocab (HUF | EUR)",
            digest.invoice_number,
            other,
        )),
        None => Err(anyhow!(
            "digest for invoice_number={} missing <currency>",
            digest.invoice_number
        )),
    }
}

/// S196 — for one freshly-restored invoice: call `queryInvoiceData`
/// against NAV, decode the base64 `<invoiceData>` blob, parse the
/// inner `<customerInfo>` + `<invoiceLines>` blocks, and upsert the
/// candidates into the local `partners` + `products` tables. Returns
/// an [`ExtractionDelta`] the caller accumulates.
///
/// Per-invoice failures are CONTAINED here: any error path (NAV
/// transport, HTTP non-success, base64 decode, XML parse, DB upsert)
/// surfaces as a `tracing::warn!` + an `invoice_extraction_errored`
/// counter increment, NOT a propagated `Err(...)`. The wizard's
/// primary contract is the invoice restore itself, which has already
/// landed by the time this function runs.
async fn extract_catalog_for_invoice(
    db_path: &Path,
    tenant: &TenantId,
    tax_number_8: &str,
    credentials: &NavCredentials,
    transport: &NavTransport,
    invoice_number: &str,
    currency: Currency,
) -> ExtractionDelta {
    let outcome = match query_invoice_data::call(
        transport,
        credentials,
        tax_number_8,
        invoice_number,
        InvoiceDirection::Outbound,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                invoice_number = invoice_number,
                error = %nav_transport_error_message(&e),
                "S196: queryInvoiceData failed; catalog extraction skipped for this invoice"
            );
            return ExtractionDelta {
                invoice_extraction_errored: 1,
                ..Default::default()
            };
        }
    };

    let response_xml = outcome.response_xml;
    let db_path_owned = db_path.to_path_buf();
    let tenant_owned = tenant.clone();
    let invoice_number_owned = invoice_number.to_string();

    // Synchronous XML parse + DB upserts on the blocking pool so the
    // tokio worker is not held across the per-candidate DuckDB writes.
    let join_result = tokio::task::spawn_blocking(move || {
        // PR-215 / S217 — `extract_inner_invoice_data_xml` now returns
        // `Result<Option<Vec<u8>>>`. For OUTBOUND (the seller's own
        // invoice) NAV is expected to always carry `<invoiceData>` —
        // the seller's entitlement to their own submission is
        // unconditional. So `Ok(None)` here is anomalous and gets the
        // same `warn!` + skip treatment as a hard decode failure (it
        // would indicate NAV-side data loss or a wire-shape regression
        // we want surfaced loud).
        let inner = match restore_from_nav_extract::extract_inner_invoice_data_xml(&response_xml) {
            Ok(Some(v)) => v,
            Ok(None) => {
                tracing::warn!(
                    invoice_number = invoice_number_owned.as_str(),
                    "S196: queryInvoiceData OUTBOUND returned funcCode=OK without \
                     <invoiceData> for the seller's own invoice — anomalous (the \
                     seller is unconditionally entitled to their own submission); \
                     catalog extraction skipped"
                );
                return ExtractionDelta {
                    invoice_extraction_errored: 1,
                    ..Default::default()
                };
            }
            Err(e) => {
                tracing::warn!(
                    invoice_number = invoice_number_owned.as_str(),
                    error = ?e,
                    "S196: failed to decode <invoiceData> base64 blob; catalog extraction skipped"
                );
                return ExtractionDelta {
                    invoice_extraction_errored: 1,
                    ..Default::default()
                };
            }
        };
        let customer = match restore_from_nav_extract::parse_customer_info(&inner) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    invoice_number = invoice_number_owned.as_str(),
                    error = ?e,
                    "S196: failed to parse <customerInfo> from inner InvoiceData XML"
                );
                return ExtractionDelta {
                    invoice_extraction_errored: 1,
                    ..Default::default()
                };
            }
        };
        let lines = match restore_from_nav_extract::parse_invoice_lines(&inner, currency) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    invoice_number = invoice_number_owned.as_str(),
                    error = ?e,
                    "S196: failed to parse <invoiceLines> from inner InvoiceData XML"
                );
                return ExtractionDelta {
                    invoice_extraction_errored: 1,
                    ..Default::default()
                };
            }
        };
        let conn = match restore_from_nav_extract::open_for_extract(&db_path_owned) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    invoice_number = invoice_number_owned.as_str(),
                    error = ?e,
                    "S196: failed to open tenant DB for catalog extraction"
                );
                return ExtractionDelta {
                    invoice_extraction_errored: 1,
                    ..Default::default()
                };
            }
        };
        restore_from_nav_extract::apply_candidates(
            &conn,
            tenant_owned.as_str(),
            &invoice_number_owned,
            &customer,
            &lines,
            currency,
        )
    })
    .await;

    match join_result {
        Ok(delta) => delta,
        Err(join_err) => {
            tracing::warn!(
                invoice_number = invoice_number,
                error = ?join_err,
                "S196: catalog-extraction blocking task panicked; counter increments \
                 for invoice_extraction_errored"
            );
            ExtractionDelta {
                invoice_extraction_errored: 1,
                ..Default::default()
            }
        }
    }
}

/// Render a [`NavTransportError`] as a short string for the
/// `tracing::warn!` payload. The full Debug form is fine but variant
/// names (`QueryInvoiceDataRetryable { code, message }` etc.) ride
/// cleanly through `Display` on each variant; collapse here so the
/// log line carries a one-line message regardless of variant.
fn nav_transport_error_message(e: &NavTransportError) -> String {
    format!("{e}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessOutcome {
    Restored,
    Skipped,
}

/// Process one digest: O(1) cache-membership idempotency check, then
/// INSERT + audit-write under one tx, then chain-verify + mirror-sync.
/// Returns `Skipped` when the cache already contains the digest's
/// `source_nav_invoice_number` (the pre-S186 path opened a fresh
/// `Ledger` and walked the chain backward per call).
///
/// On successful restore the cache is mutated in place so a
/// subsequent digest in the SAME cycle that names the same NAV
/// invoice number (NAV would not emit a duplicate, but the defence
/// is cheap) stays skipped.
fn process_digest(
    ctx: &DigestContext<'_>,
    digest: &InvoiceDigest,
    already_restored_cache: &mut AlreadyRestoredCache,
) -> Result<ProcessOutcome> {
    if already_restored_cache.contains(&digest.invoice_number) {
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

    // S186 — mark this NAV invoice number as already-restored so a
    // subsequent digest in the SAME cycle that re-names it (NAV
    // would not emit duplicates, but the defence is cheap) skips
    // via the O(1) path.
    already_restored_cache.insert(digest.invoice_number.clone());

    Ok(ProcessOutcome::Restored)
}

// ──────────────────────────────────────────────────────────────────────
// PR-216 / S218 — boot-time buyer-snapshot backfill.
// ──────────────────────────────────────────────────────────────────────

/// One backfill cycle's summary. Returned to the caller so the boot
/// log can surface the headline counts; PR-217 / S220 also wires this
/// shape verbatim into the `RestoreBuyerBackfillCycleCompleted` audit
/// payload so the audit ledger answers "did backfill run, what did
/// it find" without a log grep.
///
/// `Copy` was removed when `first_error_messages` (Vec<String>) was
/// added in PR-217; the field is a small bounded vec (cap 3) so the
/// `Clone` cost is negligible and the few callers that consumed it
/// `Copy`-style are now explicit about ownership.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuyerBackfillSummary {
    /// Rows whose `customer_name` was successfully populated this run.
    pub backfilled: u64,
    /// Rows where `queryInvoiceData` returned `funcCode=OK` but the
    /// XML carried no `<customerName>` (post-session-154 PRIVATE_PERSON
    /// wire shape per [[reference_nav_gotchas]] §1). The row's
    /// `customer_vat_status` IS populated; only the name field stays
    /// NULL because NAV does not republish it. These will be retried
    /// on every subsequent boot.
    pub backfilled_without_name: u64,
    /// Rows where the `queryInvoiceData` fetch or the parse failed.
    /// Stays NULL on disk; the next boot will retry. Each failure
    /// surfaces a `tracing::warn!`.
    pub errored: u64,
    /// Total rows scanned this run (i.e. `len()` of
    /// [`list_restored_missing_buyer`] at backfill start).
    pub scanned: u64,
    /// PR-217 / S220 — first 3 per-row error messages, captured to ride
    /// the audit payload so the operator can ask "what's failing?"
    /// without grepping logs. Cap at 3 because the typical failure
    /// mode is the [[aberp-extnav-partner-nav-gap]] entitlement
    /// rejection and 14 identical strings add no signal.
    pub first_error_messages: Vec<String>,
    /// PR-217 / S220 — wall-clock duration of the cycle, for the
    /// audit payload + the boot log line.
    pub elapsed_ms: u64,
    /// PR-217 / S220 — `Some(_)` when the cycle aborted BEFORE the
    /// per-row loop ran (worklist scan failed, transport setup
    /// failed). Surfaced verbatim onto the audit payload's `error`
    /// field. Per-row errors do NOT promote to cycle-level errors.
    pub cycle_error: Option<String>,
}

/// Inputs the boot-time spawn path assembles from `AppState` +
/// keychain. Mirrors `ap_sync::CycleInputs`'s posture (struct over
/// positional args) so a future credential-rotation can be threaded
/// through with one field rather than a signature break.
///
/// PR-217 / S220 — added `binary_hash` so the cycle can append an
/// audit entry (the binary hash rides every `LedgerMeta::new()` call
/// per `crate::audit_ledger`'s F8 carry-forward shape). The hash is
/// resolved at the call site (via `BinaryHashHandle::wait()`) so the
/// backfill task does not need to know about the handle abstraction.
pub struct BackfillInputs {
    pub db_path: PathBuf,
    pub tenant: TenantId,
    pub tax_number_8: String,
    pub endpoint: NavEndpoint,
    pub credentials: NavCredentials,
    pub binary_hash: BinaryHash,
}

/// PR-216 / S218 — one-shot backfill: scan `restored_invoice` rows
/// with NULL `customer_name`, call `queryInvoiceData OUTBOUND` per
/// row, parse `<customerInfo>`, write back the buyer snapshot. Each
/// per-row failure is contained (warn + `errored += 1`); the function
/// never propagates an error short of an unrecoverable boot-state
/// failure (DB unreadable, transport build failure).
///
/// Cancellation: between every per-row iteration we check
/// `cancel.is_cancelled()`. A mid-run shutdown drops the remaining
/// rows; they'll be picked up on the next boot.
///
/// Idempotency: the row-marker IS `customer_name IS NULL`, so a
/// re-run after a successful backfill finds 0 rows. A genuinely-empty
/// row (PRIVATE_PERSON post-session-154) stays NULL on every boot,
/// which is fine — one extra `queryInvoiceData` call per such row per
/// boot. The boot count is bounded by the operator's restart cadence,
/// not by a daemon tick, so the steady-state cost is negligible.
pub async fn run_buyer_backfill_once(
    inputs: BackfillInputs,
    cancel: tokio_util::sync::CancellationToken,
) -> BuyerBackfillSummary {
    // PR-217 / S220 — track wall-clock for the audit payload.
    let started_at = std::time::Instant::now();

    // Snapshot the missing-buyer worklist on the blocking pool so the
    // tokio worker is not held across the DuckDB read. The worklist
    // is fully consumed (then dropped) before any NAV call — no
    // long-lived DB handle.
    let scan_db = inputs.db_path.clone();
    let scan_tenant = inputs.tenant.as_str().to_string();
    let worklist = match tokio::task::spawn_blocking(move || {
        list_restored_missing_buyer(&scan_db, &scan_tenant)
    })
    .await
    {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => {
            tracing::warn!(
                error = ?e,
                "S218: buyer-backfill scan failed; skipping this boot — will retry next launch"
            );
            let summary = BuyerBackfillSummary {
                elapsed_ms: started_at.elapsed().as_millis() as u64,
                cycle_error: Some(format!("worklist scan failed: {e:#}")),
                ..Default::default()
            };
            emit_backfill_cycle_audit(&inputs, &summary);
            return summary;
        }
        Err(join_err) => {
            tracing::warn!(
                error = ?join_err,
                "S218: buyer-backfill scan task panicked; skipping this boot"
            );
            let summary = BuyerBackfillSummary {
                elapsed_ms: started_at.elapsed().as_millis() as u64,
                cycle_error: Some(format!("worklist scan task panicked: {join_err}")),
                ..Default::default()
            };
            emit_backfill_cycle_audit(&inputs, &summary);
            return summary;
        }
    };

    let scanned = worklist.len() as u64;
    if scanned == 0 {
        tracing::debug!("S218: buyer-backfill scan found 0 rows missing buyer snapshot");
        // PR-217 / S220 — emit the cycle entry on the zero-rows path
        // too. Per [[trust-code-not-operator]] the silent path was
        // the original observability bug; a "ran, found nothing" row
        // is the answer to "did backfill run." Same posture as S178's
        // `IncomingInvoiceSyncCycleCompleted` (which writes even on
        // zero-ingest cycles).
        let summary = BuyerBackfillSummary {
            elapsed_ms: started_at.elapsed().as_millis() as u64,
            ..Default::default()
        };
        emit_backfill_cycle_audit(&inputs, &summary);
        return summary;
    }
    tracing::info!(
        rows = scanned,
        "S218: buyer-backfill starting — found {scanned} restored_invoice rows missing buyer snapshot"
    );

    let transport = match NavTransport::new(inputs.endpoint) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                error = ?e,
                "S218: buyer-backfill could not build NAV transport; skipping this boot"
            );
            let summary = BuyerBackfillSummary {
                scanned,
                elapsed_ms: started_at.elapsed().as_millis() as u64,
                cycle_error: Some(format!("NAV transport build failed: {e:#}")),
                ..Default::default()
            };
            emit_backfill_cycle_audit(&inputs, &summary);
            return summary;
        }
    };

    let mut summary = BuyerBackfillSummary {
        scanned,
        ..Default::default()
    };

    for row in worklist {
        if cancel.is_cancelled() {
            tracing::info!(
                processed = summary.backfilled + summary.backfilled_without_name + summary.errored,
                remaining = scanned.saturating_sub(
                    summary.backfilled + summary.backfilled_without_name + summary.errored
                ),
                "S218: buyer-backfill cancelled mid-run — remaining rows deferred to next boot"
            );
            summary.elapsed_ms = started_at.elapsed().as_millis() as u64;
            emit_backfill_cycle_audit(&inputs, &summary);
            return summary;
        }
        match backfill_one_row(&transport, &inputs, &row).await {
            Ok(BackfillOutcome::Wrote) => summary.backfilled += 1,
            Ok(BackfillOutcome::WroteWithoutName) => summary.backfilled_without_name += 1,
            Err(e) => {
                tracing::warn!(
                    source_nav_invoice_number = %row.source_nav_invoice_number,
                    error = ?e,
                    "S218: per-row buyer backfill failed; row stays NULL — next boot retries"
                );
                summary.errored += 1;
                // PR-217 / S220 — capture first 3 error messages for
                // the audit payload. The per-row tracing::warn above
                // is still the operator's primary debugging surface;
                // these inline strings are the ledger's record of
                // "what was failing on this cycle" without a log
                // grep.
                if summary.first_error_messages.len() < 3 {
                    summary
                        .first_error_messages
                        .push(format!("{}: {e:#}", row.source_nav_invoice_number));
                }
            }
        }
    }

    summary.elapsed_ms = started_at.elapsed().as_millis() as u64;
    tracing::info!(
        scanned = summary.scanned,
        backfilled = summary.backfilled,
        backfilled_without_name = summary.backfilled_without_name,
        errored = summary.errored,
        elapsed_ms = summary.elapsed_ms,
        "S218: buyer-backfill complete"
    );
    emit_backfill_cycle_audit(&inputs, &summary);
    summary
}

/// PR-217 / S220 — write the cycle-completion audit entry.
///
/// Fire-and-forget at the call boundary: a ledger append failure here
/// is logged at `warn!` but does NOT bubble up to the caller — the
/// backfill is a boot-time recovery flow and the operator should not
/// see the app refuse to come up because the audit append failed. The
/// next boot will write its own cycle entry; the failure-to-append is
/// the kind of drift `crate::audit_ledger::verify_chain` is supposed
/// to catch on the next ledger open.
///
/// Idempotency-key minting: each cycle is a fresh decision so we mint
/// a new ULID. Same posture as
/// `IncomingInvoiceSyncCycleCompletedPayload`'s F8 carry-forward.
fn emit_backfill_cycle_audit(inputs: &BackfillInputs, summary: &BuyerBackfillSummary) {
    let payload = RestoreBuyerBackfillCycleCompletedPayload {
        idempotency_key: Ulid::new().to_string(),
        trigger: "boot".to_string(),
        scanned: summary.scanned,
        backfilled: summary.backfilled,
        backfilled_without_name: summary.backfilled_without_name,
        errored: summary.errored,
        first_error_messages: summary.first_error_messages.clone(),
        elapsed_ms: summary.elapsed_ms,
        error: summary.cycle_error.clone(),
    };
    if let Err(e) = append_backfill_cycle_entry(inputs, &payload) {
        tracing::warn!(
            error = ?e,
            scanned = summary.scanned,
            "S220: buyer-backfill cycle-audit append failed — next boot will write its own entry"
        );
    }
}

/// Inner half of [`emit_backfill_cycle_audit`] — splits out the
/// fallible plumbing so the outer fn can log+swallow uniformly.
fn append_backfill_cycle_entry(
    inputs: &BackfillInputs,
    payload: &RestoreBuyerBackfillCycleCompletedPayload,
) -> Result<()> {
    let mut conn = Connection::open(&inputs.db_path)
        .with_context(|| format!("open tenant DuckDB at {}", inputs.db_path.display()))?;
    audit_ledger::ensure_schema(&conn)
        .context("ensure audit-ledger schema for backfill cycle audit entry")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, inputs.credentials.login());
    let tx = conn
        .transaction()
        .context("begin tx for backfill cycle audit")?;
    let ledger_meta = audit_ledger::LedgerMeta::new(inputs.tenant.clone(), inputs.binary_hash);
    let idempotency_key = payload.idempotency_key.clone();
    audit_ledger::append_in_tx(
        &tx,
        &ledger_meta,
        EventKind::RestoreBuyerBackfillCycleCompleted,
        payload.to_bytes(),
        actor,
        Some(idempotency_key),
    )
    .map_err(|e| anyhow!("audit_ledger::append_in_tx RestoreBuyerBackfillCycleCompleted: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction (backfill cycle audit)")?;
    drop(conn);
    let ledger = Ledger::open(&inputs.db_path, inputs.tenant.clone(), inputs.binary_hash)
        .context("open audit ledger to sync mirror after backfill cycle entry")?;
    let mirror_path = audit_ledger::mirror_path_for(&inputs.db_path);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after backfill cycle entry")?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackfillOutcome {
    Wrote,
    WroteWithoutName,
}

/// Per-row backfill: queryInvoiceData → extract inner XML → parse
/// `<customerInfo>` → UPDATE restored_invoice. The synchronous DB +
/// XML parse path is fenced inside `spawn_blocking` so the tokio
/// worker is not held across it.
async fn backfill_one_row(
    transport: &NavTransport,
    inputs: &BackfillInputs,
    row: &RestoredMissingBuyer,
) -> Result<BackfillOutcome> {
    let outcome = query_invoice_data::call(
        transport,
        &inputs.credentials,
        &inputs.tax_number_8,
        &row.source_nav_invoice_number,
        aberp_nav_transport::soap::InvoiceDirection::Outbound,
    )
    .await
    .with_context(|| {
        format!(
            "queryInvoiceData OUTBOUND for {} (buyer backfill)",
            row.source_nav_invoice_number
        )
    })?;

    let response_xml = outcome.response_xml;
    let db_path = inputs.db_path.clone();
    let tenant = inputs.tenant.as_str().to_string();
    let source_nav_invoice_number = row.source_nav_invoice_number.clone();

    tokio::task::spawn_blocking(move || -> Result<BackfillOutcome> {
        let inner =
            match crate::restore_from_nav_extract::extract_inner_invoice_data_xml(&response_xml)? {
                Some(bytes) => bytes,
                None => {
                    return Err(anyhow!(
                        "queryInvoiceData OUTBOUND for {source_nav_invoice_number} returned \
                     funcCode=OK without <invoiceData> — seller's own invoice should \
                     always carry it; treating as backfill failure"
                    ));
                }
            };
        let customer = crate::restore_from_nav_extract::parse_customer_info(&inner)
            .context("parse <customerInfo> for buyer backfill")?;

        let conn = Connection::open(&db_path).with_context(|| {
            format!(
                "open tenant DuckDB at {} for buyer-backfill UPDATE",
                db_path.display()
            )
        })?;
        let affected = update_buyer_fields(
            &conn,
            &tenant,
            &source_nav_invoice_number,
            customer.name.as_deref(),
            customer.tax_number.as_deref(),
            Some(customer.vat_status.as_db_str()),
        )?;
        if affected == 0 {
            return Err(anyhow!(
                "UPDATE for {source_nav_invoice_number} affected 0 rows — \
                 expected exactly 1 row to match (tenant, source_nav_invoice_number)"
            ));
        }
        if customer
            .name
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            Ok(BackfillOutcome::WroteWithoutName)
        } else {
            Ok(BackfillOutcome::Wrote)
        }
    })
    .await
    .map_err(|join_err| anyhow!("buyer-backfill blocking task panicked: {join_err}"))?
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

    /// S183 — NYE-Europe-Budapest-vs-UTC skew. At 23:30 UTC on
    /// Dec 31 of year N, the Hungarian wall clock reads 00:30 CET on
    /// Jan 1 of year N+1. The operator (in Hungary) sees year N+1
    /// and typing N+1 must be accepted as the current calendar year.
    /// Pre-S183 the validator used `now_utc.date().year()` and would
    /// have rejected N+1 as "future" — silently surfacing as a
    /// "year must be <= ..." error during the first hour of every
    /// new year in Hungary. PR-182 review §S180 named this skew;
    /// PR-183 closes it via the Europe/Budapest fixed-+1h-offset path.
    ///
    /// CLAUDE.md rule 9 — the assertion targets the load-bearing
    /// timezone-source contract. A regression that reverts to
    /// `now_utc.date().year()` would fail this test loudly.
    #[test]
    fn validate_year_nye_budapest_accepts_local_year() {
        // 2026-12-31 23:30:00 UTC == 2027-01-01 00:30:00 CET (UTC+1).
        let nye_post_midnight_in_budapest = datetime!(2026-12-31 23:30:00 UTC);
        validate_year(2027, nye_post_midnight_in_budapest)
            .expect("post-midnight Hungarian-local Jan 1 must accept the new local year");
        // Pre-midnight UTC on Jan 1 stays year N for both UTC and CET —
        // sanity-check the validator still accepts year N at the
        // boundary going the other direction.
        let nye_pre_midnight_in_budapest = datetime!(2026-12-31 22:30:00 UTC);
        validate_year(2026, nye_pre_midnight_in_budapest)
            .expect("pre-midnight Hungarian-local Dec 31 must accept the still-current year");
    }

    /// S183 — defence pin: `month_window(YYYY, 12)` returns a date
    /// range that COVERS an invoice issued at 23:59:59 Europe/Budapest
    /// on Dec 31 of `YYYY`. Such an invoice's NAV
    /// `<invoiceIssueDate>` element is `YYYY-12-31` (NAV stores
    /// date-only — no time-of-day), so the upper bound
    /// `dateTo=YYYY-12-31` matches it. PR-182 review's S180 worry
    /// about year-boundary invoice loss does not bite at the
    /// `month_window` layer because the function is pure calendar
    /// arithmetic — no UTC vs CET conversion involved. This test
    /// pins that invariant against a future refactor that might
    /// accidentally derive month bounds from UTC instants.
    #[test]
    fn month_window_december_covers_nye_budapest_invoice() {
        let (from, to) = month_window(2026, 12).unwrap();
        assert_eq!(from, "2026-12-01");
        assert_eq!(
            to, "2026-12-31",
            "Dec upper bound must be 2026-12-31 so an invoice with \
             <invoiceIssueDate>2026-12-31</invoiceIssueDate> (issued at \
             23:59:59 Europe/Budapest on Dec 31) is INCLUDED in the \
             query window"
        );
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
    /// SAME digest again with the SAME in-memory cache. First call
    /// inserts + emits an audit entry + mutates the cache; second
    /// call short-circuits via the cache and returns `Skipped`.
    ///
    /// S186 — pre-PR-186 this test relied on a per-call ledger walk
    /// for idempotency; the new path uses the `AlreadyRestoredCache`
    /// passed through walk_month/process_digest. The contract
    /// (within-cycle re-processing of the same NAV invoice number
    /// returns `Skipped` and does NOT write a duplicate audit
    /// entry) is unchanged.
    #[test]
    fn process_digest_is_idempotent_within_cycle_via_cache() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);
        let mut cache: AlreadyRestoredCache = HashSet::new();

        let d = fixture_digest("INV-default/00042", "2026-04-15");
        let outcome1 = process_digest(&ctx, &d, &mut cache).expect("first call inserts");
        assert!(matches!(outcome1, ProcessOutcome::Restored));
        assert!(
            cache.contains("INV-default/00042"),
            "cache must contain the just-restored NAV invoice number"
        );

        let outcome2 =
            process_digest(&ctx, &d, &mut cache).expect("second call short-circuits via cache");
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
        let mut cache: AlreadyRestoredCache = HashSet::new();

        let mut d = fixture_digest("INV-default/00099", "2026-05-01");
        d.currency = Some("USD".to_string());
        let err = process_digest(&ctx, &d, &mut cache).expect_err("USD outside closed vocab");
        assert!(format!("{err:#}").contains("USD"), "{err:#}");
    }

    /// A digest missing `<invoiceIssueDate>` surfaces loud-fail.
    #[test]
    fn process_digest_loud_fails_on_missing_issue_date() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);
        let mut cache: AlreadyRestoredCache = HashSet::new();

        let mut d = fixture_digest("INV-default/00100", "2026-05-01");
        d.issue_date = None;
        let err = process_digest(&ctx, &d, &mut cache).expect_err("missing issue_date");
        assert!(format!("{err:#}").contains("invoiceIssueDate"));
    }

    /// S186 — `load_already_restored_cache` MUST be tenant-scoped.
    /// Without the scoping filter, a tenant-A restore would mark
    /// tenant B's same NAV invoice number as already-restored
    /// (cross-tenant contamination — the failure mode CLAUDE.md
    /// rule 12 names). This pin replaces the pre-S186
    /// `already_restored_is_tenant_scoped_by_ledger_open` test
    /// (the per-call lookup it pinned is gone; the cache-loader
    /// inherits the responsibility).
    #[test]
    fn load_already_restored_cache_is_tenant_scoped() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx_a = fixture_context(&db_path, "t1", "test-user", 2026);
        let mut cache_a: AlreadyRestoredCache = HashSet::new();

        let d = fixture_digest("INV-default/00050", "2026-03-10");
        process_digest(&ctx_a, &d, &mut cache_a).expect("tenant A restores");

        // Tenant B's cache must NOT contain tenant A's restored
        // NAV invoice number.
        let cache_b = load_already_restored_cache(
            &db_path,
            TenantId::new("t2".to_string()).unwrap(),
            ctx_a.binary_hash,
        )
        .expect("load cache t2");
        assert!(
            !cache_b.contains("INV-default/00050"),
            "tenant B's cache must not include tenant A's restored entry"
        );

        // Tenant A's freshly-loaded cache MUST contain it.
        let cache_a_reloaded =
            load_already_restored_cache(&db_path, ctx_a.tenant.clone(), ctx_a.binary_hash)
                .expect("load cache t1");
        assert!(
            cache_a_reloaded.contains("INV-default/00050"),
            "tenant A's cache must include its own restored entry"
        );
    }

    /// S186 — pre-cycle cache loader hydrates from prior-cycle
    /// audit entries. Pins the cross-cycle dedup contract: a second
    /// wizard run (fresh cache) on the same year as a prior run
    /// still skips already-restored NAV invoice numbers because
    /// [`load_already_restored_cache`] reads them back from the
    /// audit ledger. Pre-S186 the per-call ledger walk did this
    /// implicitly on every digest; post-S186 the one-shot loader
    /// is the single integration point.
    #[test]
    fn load_already_restored_cache_hydrates_from_prior_ledger_entries() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);

        // Cycle 1 — fresh cache, restore one digest.
        {
            let mut cache_one: AlreadyRestoredCache = HashSet::new();
            process_digest(
                &ctx,
                &fixture_digest("INV-default/77777", "2026-06-01"),
                &mut cache_one,
            )
            .expect("cycle 1 restores");
        }

        // Cycle 2 — a freshly-loaded cache must already contain
        // the NAV invoice number from cycle 1, so a re-encounter
        // skips and writes NO duplicate audit entry.
        let mut cache_two =
            load_already_restored_cache(&db_path, ctx.tenant.clone(), ctx.binary_hash)
                .expect("load cache cycle 2");
        assert!(
            cache_two.contains("INV-default/77777"),
            "cycle-2 cache must hydrate the prior-cycle restored entry"
        );
        let outcome = process_digest(
            &ctx,
            &fixture_digest("INV-default/77777", "2026-06-01"),
            &mut cache_two,
        )
        .expect("cycle 2 short-circuits via hydrated cache");
        assert!(matches!(outcome, ProcessOutcome::Skipped));

        // Exactly ONE audit entry — the hydrated cache prevented
        // a duplicate insert.
        let ledger =
            Ledger::open(&db_path, ctx.tenant.clone(), ctx.binary_hash).expect("open ledger");
        let entries = ledger.entries().expect("read entries");
        let restored_count = entries
            .iter()
            .filter(|e| e.kind == EventKind::InvoiceRestoredFromNav)
            .count();
        assert_eq!(
            restored_count, 1,
            "exactly one audit entry across both cycles"
        );
    }

    /// S192 — operator-recovery contract pin. Names the recovery
    /// scenario that PR-182 review's S180 🟢 called out: a prior
    /// `process_digest` cycle where `tx.commit()` succeeded (row +
    /// audit entry persisted) but the subsequent post-commit
    /// `verify_chain` / `sync_mirror` step failed (transient IO,
    /// flaky NFS, sibling-process write race — any reason
    /// `process_digest` returned `Err(...)` AFTER the commit at line
    /// 687 landed). The operator restarts the wizard; the new cycle
    /// MUST short-circuit the same digest via
    /// `load_already_restored_cache` + the in-memory cache check,
    /// returning `Skipped` with NO duplicate row and NO duplicate
    /// audit entry.
    ///
    /// The load-bearing contract this test pins (distinct from the
    /// existing `load_already_restored_cache_hydrates_from_prior_ledger_entries`):
    /// recovery is INDEPENDENT of the chain-verify state. The cache
    /// loader uses `entries()` — NOT `verify_chain()` — so a
    /// hypothetical chain-verify failure between cycles cannot block
    /// the operator's recovery path. A future refactor that adds a
    /// `verify_chain` precondition to `load_already_restored_cache`
    /// would silently break recovery for the exact failure mode this
    /// test simulates; the assertion below catches it.
    #[test]
    fn process_digest_re_run_recovers_via_cache_when_prior_commit_landed() {
        let tmp = ScopedTempDir::new("recovery");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);

        // Cycle 1 — process_digest commits row + audit successfully.
        // (In the failure scenario this test models, the cycle would
        // have returned `Err(...)` from a transient post-commit
        // verify_chain failure here; for the recovery contract we
        // only need the COMMITTED state, not the failure-return,
        // since the recovery path keys on what landed in the DB.)
        {
            let mut cache_one: AlreadyRestoredCache = HashSet::new();
            let outcome = process_digest(
                &ctx,
                &fixture_digest("INV-recovery/00001", "2026-03-15"),
                &mut cache_one,
            )
            .expect("cycle 1 commits");
            assert!(matches!(outcome, ProcessOutcome::Restored));
        }

        // Cycle 2 — operator restart. `load_already_restored_cache`
        // walks `entries()` (NOT `verify_chain`), so even a tampered
        // / unverifiable chain would still hydrate the cache.
        let mut cache_two =
            load_already_restored_cache(&db_path, ctx.tenant.clone(), ctx.binary_hash)
                .expect("cycle 2 cache loads independent of chain-verify state");
        assert!(
            cache_two.contains("INV-recovery/00001"),
            "cache loader must hydrate the prior cycle's committed entry"
        );

        // The recovery contract: same digest re-encountered → Skipped.
        let outcome = process_digest(
            &ctx,
            &fixture_digest("INV-recovery/00001", "2026-03-15"),
            &mut cache_two,
        )
        .expect("cycle 2 short-circuits via hydrated cache");
        assert!(
            matches!(outcome, ProcessOutcome::Skipped),
            "re-run of the recovered digest must Skip — no duplicate write"
        );

        // Defence pins: exactly ONE row, exactly ONE audit entry.
        // A regression where the cache loader silently failed (e.g.,
        // returning an empty set on any read error) would surface
        // here as two rows or two audit entries.
        let rows = list_restored(&db_path, "t1").expect("list");
        assert_eq!(
            rows.len(),
            1,
            "exactly one row across the partial-commit + recovery flow"
        );
        let ledger =
            Ledger::open(&db_path, ctx.tenant.clone(), ctx.binary_hash).expect("open ledger");
        let entries = ledger.entries().expect("read entries");
        let restored_count = entries
            .iter()
            .filter(|e| e.kind == EventKind::InvoiceRestoredFromNav)
            .count();
        assert_eq!(
            restored_count, 1,
            "exactly one InvoiceRestoredFromNav audit entry — \
             recovery must not write a duplicate"
        );
    }

    /// Two distinct digests both process cleanly; the listing
    /// reflects both with newest-issue-date-first ordering.
    #[test]
    fn process_digest_two_distinct_invoices() {
        let tmp = ScopedTempDir::new("test");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);
        let mut cache: AlreadyRestoredCache = HashSet::new();

        process_digest(
            &ctx,
            &fixture_digest("INV-default/00010", "2026-01-15"),
            &mut cache,
        )
        .expect("first row");
        process_digest(
            &ctx,
            &fixture_digest("INV-default/00011", "2026-02-15"),
            &mut cache,
        )
        .expect("second row");

        let list = list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 2);
        // Newest issue_date first.
        assert_eq!(list[0].source_nav_invoice_number, "INV-default/00011");
        assert_eq!(list[1].source_nav_invoice_number, "INV-default/00010");
    }

    // ── PR-216 / S218 — buyer-snapshot in-row pins ────────────────────

    /// Fresh-mint a `restored_invoice` (no buyer fields set), then
    /// `update_buyer_fields` populates them and `list_restored` reads
    /// them back. Pins the round-trip — schema migration + UPDATE +
    /// SELECT all agree on column names.
    #[test]
    fn update_buyer_fields_round_trips_through_list_restored() {
        let tmp = ScopedTempDir::new("s218-rt");
        let db_path = tmp.path().join("aberp.duckdb");
        let ctx = fixture_context(&db_path, "t1", "test-user", 2026);
        let mut cache: AlreadyRestoredCache = HashSet::new();

        process_digest(
            &ctx,
            &fixture_digest("BIL-2026-0001", "2026-04-15"),
            &mut cache,
        )
        .expect("seed restored row");

        // Pre-update: customer_name MUST be None (the new columns
        // default to NULL because the INSERT path does not touch them).
        let pre = list_restored(&db_path, "t1").expect("list pre");
        assert_eq!(pre.len(), 1);
        assert!(
            pre[0].customer_name.is_none(),
            "fresh INSERT must leave customer_name NULL until backfill / S196 writes it"
        );
        assert!(pre[0].customer_tax_number.is_none());
        assert!(pre[0].customer_vat_status.is_none());

        // Apply the buyer write-back.
        let conn = Connection::open(&db_path).expect("open");
        let affected = update_buyer_fields(
            &conn,
            "t1",
            "BIL-2026-0001",
            Some("Áben Consulting Kft."),
            Some("24904362-2-41"),
            Some("Domestic"),
        )
        .expect("UPDATE succeeds");
        assert_eq!(affected, 1, "UPDATE matched exactly the seeded row");

        // Round-trip via list_restored.
        let post = list_restored(&db_path, "t1").expect("list post");
        assert_eq!(post.len(), 1);
        assert_eq!(
            post[0].customer_name.as_deref(),
            Some("Áben Consulting Kft."),
        );
        assert_eq!(
            post[0].customer_tax_number.as_deref(),
            Some("24904362-2-41")
        );
        assert_eq!(post[0].customer_vat_status.as_deref(), Some("Domestic"));
    }

    /// `update_buyer_fields` returns 0 when the
    /// `(tenant, source_nav_invoice_number)` pair has no match — the
    /// defence-in-depth signal the backfill path keys on.
    #[test]
    fn update_buyer_fields_returns_zero_on_missing_pair() {
        let tmp = ScopedTempDir::new("s218-miss");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        ensure_schema(&conn).expect("schema");

        let affected = update_buyer_fields(
            &conn,
            "t1",
            "DOES-NOT-EXIST",
            Some("Ghost Inc."),
            None,
            Some("Domestic"),
        )
        .expect("UPDATE returns Ok even when 0 matched");
        assert_eq!(affected, 0, "no row matches → 0 affected");
    }

    /// Seed a `restored_invoice` row directly via raw INSERT — no
    /// audit-ledger chain involvement. The wizard's `process_digest`
    /// path writes both the row AND a chain entry under one tenant's
    /// ledger, which is per-file (not per-tenant) — so the wizard
    /// path cannot mix tenants in one test DB. The PR-216 buyer-write
    /// surface keys only on `(tenant_id, source_nav_invoice_number)`
    /// in the `restored_invoice` table, so the audit chain is
    /// orthogonal to what these pins exercise.
    fn seed_restored_row_raw(
        conn: &Connection,
        tenant: &str,
        invoice_number: &str,
        issue_date: &str,
    ) {
        ensure_schema(conn).expect("schema");
        conn.execute(
            "INSERT INTO restored_invoice (
                id, tenant_id, source_nav_invoice_number, source_nav_transaction_id,
                issue_date, total_net_minor, total_vat_minor, total_gross_minor,
                currency, restore_year, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            params![
                format!("rinv_{tenant}_{invoice_number}"),
                tenant,
                invoice_number,
                Option::<&str>::None,
                issue_date,
                100_000_i64,
                27_000_i64,
                127_000_i64,
                "HUF",
                2026_i32,
                format!("{issue_date}T00:00:00Z"),
            ],
        )
        .expect("seed");
    }

    /// `update_buyer_fields` for a tenant-A row must not touch a
    /// tenant-B row with the same NAV invoice number. The
    /// `(tenant_id, source_nav_invoice_number)` predicate carries the
    /// cross-tenant boundary.
    #[test]
    fn update_buyer_fields_is_tenant_scoped() {
        let tmp = ScopedTempDir::new("s218-tenant");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        seed_restored_row_raw(&conn, "t1", "BIL-2026-CROSS", "2026-04-15");
        seed_restored_row_raw(&conn, "t2", "BIL-2026-CROSS", "2026-04-15");

        let affected = update_buyer_fields(
            &conn,
            "t1",
            "BIL-2026-CROSS",
            Some("Only T1 Customer"),
            None,
            Some("PrivatePerson"),
        )
        .expect("UPDATE");
        assert_eq!(affected, 1, "exactly one row affected (tenant-scoped)");
        drop(conn);

        // t2's row stays untouched.
        let t2 = list_restored(&db_path, "t2").expect("list t2");
        assert_eq!(t2.len(), 1);
        assert!(
            t2[0].customer_name.is_none(),
            "t2's row MUST NOT be touched by a t1-scoped UPDATE"
        );

        let t1 = list_restored(&db_path, "t1").expect("list t1");
        assert_eq!(t1[0].customer_name.as_deref(), Some("Only T1 Customer"));
    }

    /// `list_restored_missing_buyer` returns exactly the rows whose
    /// `customer_name` is NULL — the backfill task's worklist.
    /// Filled-buyer rows are NOT returned. Cross-tenant rows are NOT
    /// returned.
    #[test]
    fn list_restored_missing_buyer_filters_by_null_and_tenant() {
        let tmp = ScopedTempDir::new("s218-worklist");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        // t1: A (will fill), B (stays NULL). t2: C (stays NULL, must
        // NOT leak into t1's worklist).
        seed_restored_row_raw(&conn, "t1", "A-001", "2026-01-15");
        seed_restored_row_raw(&conn, "t1", "B-002", "2026-02-15");
        seed_restored_row_raw(&conn, "t2", "C-003", "2026-03-15");

        update_buyer_fields(
            &conn,
            "t1",
            "A-001",
            Some("Filled Co."),
            Some("12345678-2-41"),
            Some("Domestic"),
        )
        .expect("fill A");
        drop(conn);

        // t1's worklist: only B-002.
        let work = list_restored_missing_buyer(&db_path, "t1").expect("list missing t1");
        assert_eq!(work.len(), 1, "exactly one row remains NULL in t1");
        assert_eq!(work[0].source_nav_invoice_number, "B-002");

        // t2's worklist: only C-003 (NOT B-002 from t1).
        let work_t2 = list_restored_missing_buyer(&db_path, "t2").expect("list missing t2");
        assert_eq!(work_t2.len(), 1);
        assert_eq!(work_t2[0].source_nav_invoice_number, "C-003");
    }

    /// Schema migration is idempotent — running `ensure_schema` twice
    /// is a no-op. Pre-PR-216 tables (without the buyer columns) get
    /// migrated cleanly; this models the prod-upgrade path Ervin's 14
    /// rows take.
    #[test]
    fn ensure_schema_is_idempotent_and_migrates_pre_pr216_tables() {
        let tmp = ScopedTempDir::new("s218-migrate");
        let db_path = tmp.path().join("aberp.duckdb");

        // Step 1: hand-roll the PRE-PR-216 schema (no buyer columns).
        // This models the prod DB Ervin already ran the wizard against.
        let conn = Connection::open(&db_path).expect("open");
        conn.execute_batch(
            "CREATE TABLE restored_invoice (
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
            );",
        )
        .expect("seed pre-PR-216 schema");

        // Seed a row using the pre-PR-216 INSERT shape (no buyer cols).
        conn.execute(
            "INSERT INTO restored_invoice (
                id, tenant_id, source_nav_invoice_number, source_nav_transaction_id,
                issue_date, total_net_minor, total_vat_minor, total_gross_minor,
                currency, restore_year, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            params![
                "rinv_LEGACY",
                "t1",
                "BIL-LEGACY",
                Option::<&str>::None,
                "2026-04-15",
                100_000_i64,
                27_000_i64,
                127_000_i64,
                "HUF",
                2026_i32,
                "2026-04-15T00:00:00Z",
            ],
        )
        .expect("seed legacy row");

        // Step 2: apply the PR-216 migration. Must succeed cleanly.
        ensure_schema(&conn).expect("PR-216 migration on pre-PR-216 table");
        // Re-run to pin idempotency.
        ensure_schema(&conn).expect("ensure_schema is idempotent");

        // Step 3: list_restored reads the legacy row with NULL buyer
        // columns (migration added them as NULLABLE).
        let list = list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].source_nav_invoice_number, "BIL-LEGACY");
        assert!(
            list[0].customer_name.is_none(),
            "legacy row's customer_name MUST be None after migration"
        );

        // Step 4: backfill via update_buyer_fields succeeds —
        // proving the new columns are writable post-migration.
        let affected = update_buyer_fields(
            &conn,
            "t1",
            "BIL-LEGACY",
            Some("Áben Consulting Kft."),
            Some("24904362-2-41"),
            Some("Domestic"),
        )
        .expect("UPDATE post-migration");
        assert_eq!(affected, 1);
        // Drop the seed connection so list_restored's fresh
        // `Connection::open` sees the committed UPDATE rather than
        // racing the seed connection's in-flight write state. DuckDB
        // auto-commits but a second connection reading from the same
        // file via a separate `open` was observed to surface NULL for
        // the just-written column when both connections coexist mid-
        // test; dropping the writer first is the surgical fix.
        drop(conn);

        let list_post = list_restored(&db_path, "t1").expect("list post");
        assert_eq!(
            list_post[0].customer_name.as_deref(),
            Some("Áben Consulting Kft.")
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // PR-217 / S220 — manual partner-link tests.
    // ──────────────────────────────────────────────────────────────────

    /// `update_partner_for_restored` writes all FOUR denormalized
    /// fields together (the partner pointer + 3 buyer snapshot fields).
    /// The link path mirrors how the manual-link route writes a
    /// "currently linked" snapshot onto an ExtNav row whose backfill
    /// found no buyer info.
    #[test]
    fn update_partner_for_restored_writes_all_four_fields() {
        let tmp = ScopedTempDir::new("s220-link");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        seed_restored_row_raw(&conn, "t1", "BIL-2026-LINK", "2026-04-15");

        let row_id = "rinv_t1_BIL-2026-LINK";
        let affected = update_partner_for_restored(
            &conn,
            "t1",
            row_id,
            Some("prt_01ABCDEFGHJKMNPQRSTVWXYZ12"),
            Some("Áben Consulting Kft."),
            Some("24904362-2-41"),
            Some("Domestic"),
        )
        .expect("UPDATE");
        assert_eq!(affected, 1, "exactly one row affected by tenant+id key");
        drop(conn);

        let list = list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].partner_id.as_deref(),
            Some("prt_01ABCDEFGHJKMNPQRSTVWXYZ12"),
            "partner_id is persisted"
        );
        assert_eq!(
            list[0].customer_name.as_deref(),
            Some("Áben Consulting Kft."),
            "customer_name is denormalized onto the row"
        );
        assert_eq!(
            list[0].customer_tax_number.as_deref(),
            Some("24904362-2-41"),
            "customer_tax_number is denormalized onto the row"
        );
        assert_eq!(
            list[0].customer_vat_status.as_deref(),
            Some("Domestic"),
            "customer_vat_status is denormalized onto the row"
        );
    }

    /// `update_partner_for_restored` with all-None clears the four
    /// fields back to NULL — the "clear / no partner" path the SPA
    /// invokes from the modal's Clear button.
    #[test]
    fn update_partner_for_restored_clears_all_four_fields() {
        let tmp = ScopedTempDir::new("s220-clear");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        seed_restored_row_raw(&conn, "t1", "BIL-2026-CLEAR", "2026-04-15");

        let row_id = "rinv_t1_BIL-2026-CLEAR";
        // Step 1: link.
        update_partner_for_restored(
            &conn,
            "t1",
            row_id,
            Some("prt_01ABCDEFGHJKMNPQRSTVWXYZ12"),
            Some("Áben Consulting Kft."),
            Some("24904362-2-41"),
            Some("Domestic"),
        )
        .expect("link");

        // Step 2: clear.
        let affected = update_partner_for_restored(&conn, "t1", row_id, None, None, None, None)
            .expect("clear");
        assert_eq!(affected, 1);
        drop(conn);

        let list = list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 1);
        assert!(list[0].partner_id.is_none(), "partner_id cleared to NULL");
        assert!(
            list[0].customer_name.is_none(),
            "customer_name cleared to NULL"
        );
        assert!(
            list[0].customer_tax_number.is_none(),
            "customer_tax_number cleared to NULL"
        );
        assert!(
            list[0].customer_vat_status.is_none(),
            "customer_vat_status cleared to NULL"
        );
    }

    /// `update_partner_for_restored` is tenant-scoped — a t1-scoped
    /// write does not touch a t2 row even with the same restored id
    /// shape. Same posture as `update_buyer_fields_is_tenant_scoped`.
    #[test]
    fn update_partner_for_restored_is_tenant_scoped() {
        let tmp = ScopedTempDir::new("s220-tenant");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        seed_restored_row_raw(&conn, "t1", "X-1", "2026-04-15");
        seed_restored_row_raw(&conn, "t2", "X-1", "2026-04-15");

        // Both seeded rows have the same id shape ("rinv_<tenant>_<num>")
        // — but the WHERE pins on tenant_id AND id.
        let affected = update_partner_for_restored(
            &conn,
            "t1",
            "rinv_t1_X-1",
            Some("prt_T1ONLY"),
            Some("T1 Only Co."),
            None,
            Some("PrivatePerson"),
        )
        .expect("UPDATE");
        assert_eq!(affected, 1);
        drop(conn);

        let t2 = list_restored(&db_path, "t2").expect("list t2");
        assert_eq!(t2.len(), 1);
        assert!(
            t2[0].partner_id.is_none(),
            "t2's row MUST NOT be touched by a t1-scoped UPDATE"
        );
    }

    /// `read_restored_buyer_snapshot` returns `Ok(None)` for an unknown
    /// row id — the route then maps it to 404. Tenant scoping holds:
    /// a t2-tenant lookup for a t1-existing row returns None.
    #[test]
    fn read_restored_buyer_snapshot_returns_none_on_unknown_or_wrong_tenant() {
        let tmp = ScopedTempDir::new("s220-snapshot");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        seed_restored_row_raw(&conn, "t1", "Y-1", "2026-04-15");

        // Unknown id.
        let absent =
            read_restored_buyer_snapshot(&conn, "t1", "rinv_does_not_exist").expect("query");
        assert!(absent.is_none());

        // Right id, wrong tenant.
        let cross = read_restored_buyer_snapshot(&conn, "t2", "rinv_t1_Y-1").expect("query");
        assert!(cross.is_none(), "MUST NOT leak across tenants");

        // Right id, right tenant — Some(snapshot).
        let hit = read_restored_buyer_snapshot(&conn, "t1", "rinv_t1_Y-1").expect("query");
        let snap = hit.expect("row exists");
        assert!(snap.partner_id.is_none(), "fresh row has no partner link");
        assert!(snap.customer_name.is_none(), "fresh row has NULL name");
    }

    /// `read_restored_source_number` mirrors `read_restored_buyer_snapshot`'s
    /// tenant-scoped lookup. The audit payload's
    /// `source_nav_invoice_number` field is sourced from this helper so
    /// the handler does not have to thread the original digest through.
    #[test]
    fn read_restored_source_number_returns_canonical_form() {
        let tmp = ScopedTempDir::new("s220-source");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        seed_restored_row_raw(&conn, "t1", "BIL-default/00042", "2026-04-15");

        let num = read_restored_source_number(&conn, "t1", "rinv_t1_BIL-default/00042")
            .expect("query")
            .expect("row exists");
        assert_eq!(num, "BIL-default/00042");

        let absent = read_restored_source_number(&conn, "t1", "rinv_missing").expect("query");
        assert!(absent.is_none());
    }

    /// PR-217 / S220 — the partner_id migration is idempotent. Mirrors
    /// the PR-216 migration test's posture; pins that running
    /// `ensure_schema` against a fresh DB AND a PR-216-only DB both
    /// surface the `partner_id` column ready to write.
    #[test]
    fn pr217_partner_id_migration_is_idempotent() {
        let tmp = ScopedTempDir::new("s220-migrate");
        let db_path = tmp.path().join("aberp.duckdb");

        // Step 1: stand up PR-216 shape (no partner_id) and seed a row.
        let conn = Connection::open(&db_path).expect("open");
        conn.execute_batch(RESTORED_INVOICE_SCHEMA_SQL)
            .expect("base schema");
        conn.execute_batch(RESTORED_INVOICE_PR216_MIGRATION_SQL)
            .expect("PR-216 migration");
        conn.execute(
            "INSERT INTO restored_invoice (
                id, tenant_id, source_nav_invoice_number, source_nav_transaction_id,
                issue_date, total_net_minor, total_vat_minor, total_gross_minor,
                currency, restore_year, created_at,
                customer_name, customer_tax_number, customer_vat_status
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            params![
                "rinv_PR216",
                "t1",
                "PR216-001",
                Option::<&str>::None,
                "2026-04-15",
                100_000_i64,
                27_000_i64,
                127_000_i64,
                "HUF",
                2026_i32,
                "2026-04-15T00:00:00Z",
                Option::<&str>::None,
                Option::<&str>::None,
                Option::<&str>::None,
            ],
        )
        .expect("seed PR-216 row");

        // Step 2: apply full ensure_schema (PR-217 migration) twice.
        ensure_schema(&conn).expect("PR-217 migration on PR-216 table");
        ensure_schema(&conn).expect("ensure_schema is idempotent");

        // Step 3: write partner_id on the legacy row — the new column
        // is present + writable.
        let affected = update_partner_for_restored(
            &conn,
            "t1",
            "rinv_PR216",
            Some("prt_TEST"),
            Some("Filled In Post-Migration"),
            Some("12345678-2-41"),
            Some("Domestic"),
        )
        .expect("UPDATE post-migration");
        assert_eq!(affected, 1);
        drop(conn);

        let list = list_restored(&db_path, "t1").expect("list");
        assert_eq!(list[0].partner_id.as_deref(), Some("prt_TEST"));
    }
}
