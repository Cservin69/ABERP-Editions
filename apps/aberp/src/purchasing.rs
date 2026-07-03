//! S440 (ADR-0068) — purchase orders: procurement + AVL gate + receiving→NCR.
//!
//! Closes the last Proposed ADR of the defense pivot. An operator drafts a
//! purchase order against a vendor partner, issues it, and records deliveries
//! as receipts. Three invariants live in code, never in operator discipline
//! ([[trust-code-not-operator]]):
//!
//!   1. **AVL gate** — a `Suspended`/`Revoked` vendor is refused at PO-create
//!      (fires the S431 `supplier.po_blocked_by_vendor_status` kind); a
//!      `Pending` vendor may draft but not *issue* (the issue re-checks the
//!      live AVL status); a `Conditional` vendor flows but its status is
//!      snapshotted onto the PO so the SPA can flag it.
//!   2. **State machine** — only the allowed edges of the lifecycle graph
//!      ([`allowed_transition`]); `Draft → IssuedToVendor` requires an
//!      `approved_by_operator`. The receipt-driven `PartiallyReceived` /
//!      `Received` edges are computed from line quantities
//!      ([`receipt_state_after`]), never set by hand.
//!   3. **Receiving → quality** — a failed incoming inspection on any line
//!      auto-creates an NCR (S439) and fires `po.incoming_inspection_failed`,
//!      so a bad delivery can never be "received" without a quality record.
//!
//! ## Sparse by design (CLAUDE.md rule 12)
//!
//! No CHECK / no DEFAULT / no surrogate ids ([[no-sql-specific]] + the DuckDB
//! replay-clobber trap). A non-procurement tenant simply has zero rows here.
//! Money is integer minor units (mirrors `products.unit_price_minor`); the PO
//! `currency` is a free ISO-4217 token (procurement spans USD/EUR/HUF) kept
//! independent of the NAV fiscal `Currency` enum.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_compliance::avl::ApprovedStatus;

// ── Closed-vocab state enum ─────────────────────────────────────────

/// PO lifecycle state. Happy path:
/// `Draft → IssuedToVendor → PartiallyReceived → Received → Closed`.
/// `Cancelled` is the terminal abort branch from any pre-`Received` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoState {
    Draft,
    IssuedToVendor,
    PartiallyReceived,
    Received,
    Closed,
    Cancelled,
}

impl PoState {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            PoState::Draft => "draft",
            PoState::IssuedToVendor => "issued_to_vendor",
            PoState::PartiallyReceived => "partially_received",
            PoState::Received => "received",
            PoState::Closed => "closed",
            PoState::Cancelled => "cancelled",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(PoState::Draft),
            "issued_to_vendor" => Some(PoState::IssuedToVendor),
            "partially_received" => Some(PoState::PartiallyReceived),
            "received" => Some(PoState::Received),
            "closed" => Some(PoState::Closed),
            "cancelled" => Some(PoState::Cancelled),
            _ => None,
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, PoState::Closed | PoState::Cancelled)
    }
    /// A receipt can be recorded only against an issued or partly-received PO.
    pub fn accepts_receipt(&self) -> bool {
        matches!(self, PoState::IssuedToVendor | PoState::PartiallyReceived)
    }
}

// ── Pure: state transitions + receipt-state derivation ──────────────

/// The allowed *operator-driven* transition edges ([[trust-code-not-operator]]).
/// Pure — unit-testable without a DB. The receipt-driven edges
/// (`→ PartiallyReceived` / `→ Received`) are NOT here: they are computed from
/// line quantities by [`receipt_state_after`], never operator-set.
pub fn allowed_transition(from: PoState, to: PoState) -> bool {
    use PoState::*;
    matches!(
        (from, to),
        (Draft, IssuedToVendor)
            | (Draft, Cancelled)
            | (IssuedToVendor, Cancelled)
            | (PartiallyReceived, Cancelled)
            | (Received, Closed)
    )
}

/// Derive the post-receipt state from the (already-incremented) line set
/// ([[trust-code-not-operator]] — the brief's "PartiallyReceived only if some
/// lines have received > 0 AND not all fully received"). Pure.
///
/// - All lines fully received (`received >= quantity`) → `Received`.
/// - Some line has `received > 0` but not all are full → `PartiallyReceived`.
/// - Nothing received yet → `IssuedToVendor` (unchanged).
pub fn receipt_state_after(lines: &[PoLine]) -> PoState {
    if lines.is_empty() {
        return PoState::IssuedToVendor;
    }
    let all_full = lines.iter().all(|l| l.received_quantity >= l.quantity);
    if all_full {
        return PoState::Received;
    }
    let any = lines.iter().any(|l| l.received_quantity > 0);
    if any {
        PoState::PartiallyReceived
    } else {
        PoState::IssuedToVendor
    }
}

/// Mint `po_<26-char-ULID>` (the internal PK; distinct from the operator-facing
/// sequential `po_number`).
pub fn generate_po_id() -> String {
    format!("po_{}", Ulid::new())
}

/// Render the operator-facing annual sequence number: `PO-YYYY-NNNN`. The pad is
/// a floor — past 9999 it grows (`PO-2026-10000`).
pub fn format_po_number(year: i32, n: u64) -> String {
    format!("PO-{year}-{n:04}")
}

// ── Money (integer minor units; mirrors products.unit_price_minor) ──

/// `quantity * unit_price_minor`, overflow-checked. A line that overflows i64
/// minor units is a loud-reject, not a silent wrap (CLAUDE.md rule 12).
pub fn line_total_minor(quantity: i64, unit_price_minor: i64) -> Option<i64> {
    quantity.checked_mul(unit_price_minor)
}

/// VAT in minor units: `subtotal * rate% / 100`, floor division. Computed in
/// i128 so a large subtotal × rate cannot overflow before the divide.
pub fn vat_minor(subtotal_minor: i64, vat_rate_pct: i32) -> i64 {
    let v = (subtotal_minor as i128) * (vat_rate_pct as i128) / 100;
    v as i64
}

// ── Validation ──────────────────────────────────────────────────────

fn validate_currency(c: &str) -> std::result::Result<(), &'static str> {
    let t = c.trim();
    if t.len() != 3 || !t.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err("currency must be a 3-letter ISO-4217 code (e.g. HUF, EUR, USD)");
    }
    Ok(())
}

// ── Schema ──────────────────────────────────────────────────────────

/// Additive procurement tables + the per-tenant annual PO sequence. NO surrogate
/// id (natural prefixed-ULID / `(tenant, year)` PK), NO CHECK / NO DEFAULT, NO
/// index ([[no-sql-specific]] — scan/filter in Rust).
const PURCHASING_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS purchase_orders (
    po_id                  VARCHAR NOT NULL,
    tenant_id              VARCHAR NOT NULL,
    po_number              VARCHAR NOT NULL,
    vendor_partner_id      VARCHAR NOT NULL,
    currency               VARCHAR NOT NULL,
    subtotal_minor         BIGINT  NOT NULL,
    vat_rate_pct           INTEGER NOT NULL,
    vat_minor              BIGINT  NOT NULL,
    total_minor            BIGINT  NOT NULL,
    state                  VARCHAR NOT NULL,
    vendor_avl_status      VARCHAR,
    issued_at_utc          VARCHAR,
    expected_delivery_utc  VARCHAR,
    notes                  VARCHAR NOT NULL,
    requested_by_operator  VARCHAR NOT NULL,
    approved_by_operator   VARCHAR,
    approved_at_utc        VARCHAR,
    created_at_utc         VARCHAR NOT NULL
);
CREATE TABLE IF NOT EXISTS purchase_order_lines (
    pol_id                      VARCHAR NOT NULL,
    po_id                       VARCHAR NOT NULL,
    tenant_id                   VARCHAR NOT NULL,
    seq                         INTEGER NOT NULL,
    product_id                  VARCHAR,
    description                 VARCHAR NOT NULL,
    quantity                    BIGINT  NOT NULL,
    unit_price_minor            BIGINT  NOT NULL,
    currency                    VARCHAR NOT NULL,
    line_total_minor            BIGINT  NOT NULL,
    expected_heat_lot_required  BOOLEAN NOT NULL,
    received_quantity           BIGINT  NOT NULL
);
CREATE TABLE IF NOT EXISTS purchase_order_receipts (
    por_id                VARCHAR NOT NULL,
    po_id                 VARCHAR NOT NULL,
    pol_id                VARCHAR NOT NULL,
    tenant_id             VARCHAR NOT NULL,
    received_at_utc       VARCHAR NOT NULL,
    received_by_operator  VARCHAR NOT NULL,
    delivery_note_number  VARCHAR NOT NULL,
    received_quantity     BIGINT  NOT NULL,
    inspection_pass       BOOLEAN NOT NULL,
    inspection_notes      VARCHAR NOT NULL,
    heat_lot_assigned     VARCHAR,
    ncr_id                VARCHAR
);
CREATE TABLE IF NOT EXISTS po_number_state (
    tenant_id    VARCHAR NOT NULL,
    year         INTEGER NOT NULL,
    next_number  BIGINT  NOT NULL,
    updated_at   VARCHAR NOT NULL
);
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` for all purchasing tables.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn (read_returns_readonly
    // read()-side); the schema is created by a writer before any read reaches
    // here. A genuine write mis-routed through read() still fails loud (F5).
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(PURCHASING_SCHEMA_SQL)
        .context("ensure purchasing schema")
}

// ── Row shapes ──────────────────────────────────────────────────────

/// One purchase-order header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurchaseOrder {
    pub po_id: String,
    pub po_number: String,
    pub vendor_partner_id: String,
    pub currency: String,
    pub subtotal_minor: i64,
    pub vat_rate_pct: i32,
    pub vat_minor: i64,
    pub total_minor: i64,
    pub state: PoState,
    /// The vendor's AVL status snapshotted at create time (`None` = unlisted).
    /// Drives the SPA's `Conditional` yellow chip.
    pub vendor_avl_status: Option<String>,
    pub issued_at_utc: Option<String>,
    pub expected_delivery_utc: Option<String>,
    pub notes: String,
    pub requested_by_operator: String,
    pub approved_by_operator: Option<String>,
    pub approved_at_utc: Option<String>,
    pub created_at_utc: String,
}

/// One purchase-order line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoLine {
    pub pol_id: String,
    pub po_id: String,
    pub product_id: Option<String>,
    pub description: String,
    pub quantity: i64,
    pub unit_price_minor: i64,
    pub currency: String,
    pub line_total_minor: i64,
    pub expected_heat_lot_required: bool,
    pub received_quantity: i64,
}

/// One receipt row (one per delivered line; a delivery is the set of rows that
/// share `delivery_note_number` + `received_at_utc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoReceiptLine {
    pub por_id: String,
    pub po_id: String,
    pub pol_id: String,
    pub received_at_utc: String,
    pub received_by_operator: String,
    pub delivery_note_number: String,
    pub received_quantity: i64,
    pub inspection_pass: bool,
    pub inspection_notes: String,
    pub heat_lot_assigned: Option<String>,
    /// Set when `inspection_pass == false` — the auto-created NCR (S439).
    pub ncr_id: Option<String>,
}

/// PO header + lines + receipt history (the detail-page payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoDetail {
    #[serde(flatten)]
    pub po: PurchaseOrder,
    pub lines: Vec<PoLine>,
    pub receipts: Vec<PoReceiptLine>,
}

const PO_COLS: &str = "po_id, po_number, vendor_partner_id, currency, subtotal_minor, \
     vat_rate_pct, vat_minor, total_minor, state, vendor_avl_status, issued_at_utc, \
     expected_delivery_utc, notes, requested_by_operator, approved_by_operator, \
     approved_at_utc, created_at_utc";

fn row_to_po(r: &duckdb::Row) -> duckdb::Result<PurchaseOrder> {
    Ok(PurchaseOrder {
        po_id: r.get::<_, String>(0)?,
        po_number: r.get::<_, String>(1)?,
        vendor_partner_id: r.get::<_, String>(2)?,
        currency: r.get::<_, String>(3)?,
        subtotal_minor: r.get::<_, i64>(4)?,
        vat_rate_pct: r.get::<_, i32>(5)?,
        vat_minor: r.get::<_, i64>(6)?,
        total_minor: r.get::<_, i64>(7)?,
        state: PoState::from_db_str(&r.get::<_, String>(8)?).unwrap_or(PoState::Draft),
        vendor_avl_status: r.get::<_, Option<String>>(9)?,
        issued_at_utc: r.get::<_, Option<String>>(10)?,
        expected_delivery_utc: r.get::<_, Option<String>>(11)?,
        notes: r.get::<_, String>(12)?,
        requested_by_operator: r.get::<_, String>(13)?,
        approved_by_operator: r.get::<_, Option<String>>(14)?,
        approved_at_utc: r.get::<_, Option<String>>(15)?,
        created_at_utc: r.get::<_, String>(16)?,
    })
}

const POL_COLS: &str = "pol_id, po_id, product_id, description, quantity, unit_price_minor, \
     currency, line_total_minor, expected_heat_lot_required, received_quantity";

fn row_to_line(r: &duckdb::Row) -> duckdb::Result<PoLine> {
    Ok(PoLine {
        pol_id: r.get::<_, String>(0)?,
        po_id: r.get::<_, String>(1)?,
        product_id: r.get::<_, Option<String>>(2)?,
        description: r.get::<_, String>(3)?,
        quantity: r.get::<_, i64>(4)?,
        unit_price_minor: r.get::<_, i64>(5)?,
        currency: r.get::<_, String>(6)?,
        line_total_minor: r.get::<_, i64>(7)?,
        expected_heat_lot_required: r.get::<_, bool>(8)?,
        received_quantity: r.get::<_, i64>(9)?,
    })
}

const POR_COLS: &str = "por_id, po_id, pol_id, received_at_utc, received_by_operator, \
     delivery_note_number, received_quantity, inspection_pass, inspection_notes, \
     heat_lot_assigned, ncr_id";

fn row_to_receipt(r: &duckdb::Row) -> duckdb::Result<PoReceiptLine> {
    Ok(PoReceiptLine {
        por_id: r.get::<_, String>(0)?,
        po_id: r.get::<_, String>(1)?,
        pol_id: r.get::<_, String>(2)?,
        received_at_utc: r.get::<_, String>(3)?,
        received_by_operator: r.get::<_, String>(4)?,
        delivery_note_number: r.get::<_, String>(5)?,
        received_quantity: r.get::<_, i64>(6)?,
        inspection_pass: r.get::<_, bool>(7)?,
        inspection_notes: r.get::<_, String>(8)?,
        heat_lot_assigned: r.get::<_, Option<String>>(9)?,
        ncr_id: r.get::<_, Option<String>>(10)?,
    })
}

// ── Reads ───────────────────────────────────────────────────────────

/// Filter spec for [`list_pos`]. Empty fields match all. Resolved in Rust — no
/// index, no SQL-specific WHERE building ([[no-sql-specific]]).
#[derive(Debug, Clone, Default)]
pub struct PoFilter {
    pub state: Option<PoState>,
    pub vendor_partner_id: Option<String>,
    /// Inclusive ISO lower bound on `created_at_utc`.
    pub from: Option<String>,
    /// Inclusive ISO upper bound on `created_at_utc`.
    pub to: Option<String>,
}

/// List POs (newest first), filtered in Rust over a full scan.
pub fn list_pos(conn: &Connection, tenant: &str, filter: &PoFilter) -> Result<Vec<PurchaseOrder>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {PO_COLS} FROM purchase_orders WHERE tenant_id = ?1 \
         ORDER BY created_at_utc DESC, po_id DESC"
    );
    let mut stmt = conn.prepare(&sql).context("prepare list_pos")?;
    let rows = stmt
        .query_map(params![tenant], row_to_po)
        .context("query list_pos")?;
    let mut out = Vec::new();
    for r in rows {
        let po = r.context("read po row")?;
        if let Some(s) = filter.state {
            if po.state != s {
                continue;
            }
        }
        if let Some(v) = filter
            .vendor_partner_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if po.vendor_partner_id != v {
                continue;
            }
        }
        if let Some(from) = filter
            .from
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if po.created_at_utc.as_str() < from {
                continue;
            }
        }
        if let Some(to) = filter
            .to
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let keep = po.created_at_utc.as_str() <= to || po.created_at_utc.starts_with(to);
            if !keep {
                continue;
            }
        }
        out.push(po);
    }
    Ok(out)
}

/// Read one PO header by id.
pub fn get_po(conn: &Connection, tenant: &str, po_id: &str) -> Result<Option<PurchaseOrder>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {PO_COLS} FROM purchase_orders WHERE tenant_id = ?1 AND po_id = ?2 LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql).context("prepare get_po")?;
    let mut rows = stmt.query(params![tenant, po_id]).context("query get_po")?;
    match rows.next().context("read get_po row")? {
        Some(r) => Ok(Some(row_to_po(r)?)),
        None => Ok(None),
    }
}

/// Read a PO's lines, in entry order.
pub fn list_po_lines(conn: &Connection, tenant: &str, po_id: &str) -> Result<Vec<PoLine>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {POL_COLS} FROM purchase_order_lines WHERE tenant_id = ?1 AND po_id = ?2 ORDER BY seq"
    );
    let mut stmt = conn.prepare(&sql).context("prepare list_po_lines")?;
    let rows = stmt
        .query_map(params![tenant, po_id], row_to_line)
        .context("query list_po_lines")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read po line row")?);
    }
    Ok(out)
}

/// Read a PO's receipt history, oldest first.
pub fn list_po_receipts(
    conn: &Connection,
    tenant: &str,
    po_id: &str,
) -> Result<Vec<PoReceiptLine>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {POR_COLS} FROM purchase_order_receipts WHERE tenant_id = ?1 AND po_id = ?2 \
         ORDER BY received_at_utc, por_id"
    );
    let mut stmt = conn.prepare(&sql).context("prepare list_po_receipts")?;
    let rows = stmt
        .query_map(params![tenant, po_id], row_to_receipt)
        .context("query list_po_receipts")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read po receipt row")?);
    }
    Ok(out)
}

/// Full detail payload: PO + lines + receipts.
pub fn get_po_detail(conn: &Connection, tenant: &str, po_id: &str) -> Result<Option<PoDetail>> {
    let Some(po) = get_po(conn, tenant, po_id)? else {
        return Ok(None);
    };
    let lines = list_po_lines(conn, tenant, po_id)?;
    let receipts = list_po_receipts(conn, tenant, po_id)?;
    Ok(Some(PoDetail {
        po,
        lines,
        receipts,
    }))
}

// ── Errors ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PoError {
    #[error("purchase order {0} not found")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    /// A transition not permitted by [`allowed_transition`], a missing approver,
    /// or a receipt against a non-receivable PO. Maps to HTTP 409.
    #[error("{0}")]
    IllegalTransition(String),
    /// The AVL gate refused this vendor (`Suspended`/`Revoked`). Maps to HTTP 409.
    #[error("vendor {partner_id} is {status}; resolve AVL screening before the PO")]
    BlockedByVendorStatus { partner_id: String, status: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

// ── Inputs ──────────────────────────────────────────────────────────

/// One operator-supplied PO line at create time.
#[derive(Debug, Clone, Deserialize)]
pub struct NewPoLine {
    #[serde(default)]
    pub product_id: Option<String>,
    pub description: String,
    pub quantity: i64,
    pub unit_price_minor: i64,
    #[serde(default)]
    pub expected_heat_lot_required: bool,
}

/// Operator-supplied PO creation input.
#[derive(Debug, Clone, Deserialize)]
pub struct NewPo {
    pub vendor_partner_id: String,
    pub currency: String,
    #[serde(default)]
    pub vat_rate_pct: i32,
    #[serde(default)]
    pub expected_delivery_utc: Option<String>,
    #[serde(default)]
    pub notes: String,
    pub lines: Vec<NewPoLine>,
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

// ── PO numbering (atomic per-tenant annual sequence; mirrors S159) ──

/// Reserve the next `PO-YYYY-NNNN` for `(tenant, year)` inside an open
/// transaction — read `next_number` (seeding the bucket at 1 if absent), then
/// advance it by exactly 1. Monotonic + gap-free within a year; a new calendar
/// year is a fresh bucket starting at 1 (annual reset). Mirrors the invoice
/// `allocate_in_tx` floor-and-advance, minus the template/NAV machinery.
fn reserve_po_number(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    year: i32,
    now: &str,
) -> Result<String> {
    let next: u64 = {
        let mut stmt = tx
            .prepare("SELECT next_number FROM po_number_state WHERE tenant_id = ? AND year = ?")
            .context("prepare po_number read")?;
        let mut rows = stmt
            .query_map(params![tenant, year], |r| r.get::<_, i64>(0))
            .context("query po_number")?;
        match rows.next() {
            Some(r) => r.context("read po_number")? as u64,
            None => {
                tx.execute(
                    "INSERT INTO po_number_state (tenant_id, year, next_number, updated_at) \
                     VALUES (?, ?, ?, ?)",
                    params![tenant, year, 1_i64, now],
                )
                .context("seed po_number_state")?;
                1
            }
        }
    };
    tx.execute(
        "UPDATE po_number_state SET next_number = ?, updated_at = ? WHERE tenant_id = ? AND year = ?",
        params![(next + 1) as i64, now, tenant, year],
    )
    .context("advance po_number_state")?;
    Ok(format_po_number(year, next))
}

// ── Writes ──────────────────────────────────────────────────────────

/// Resolve a partner's live AVL vendor row + parsed status (`None` = unlisted).
fn resolve_avl(
    conn: &Connection,
    tenant: &str,
    partner_id: &str,
) -> Result<Option<(crate::avl_vendors::AvlVendor, ApprovedStatus)>> {
    let Some(vendor) = crate::avl_vendors::get_vendor_by_partner(conn, tenant, partner_id)? else {
        return Ok(None);
    };
    let status = ApprovedStatus::from_storage_str(&vendor.approved_status)
        .map_err(|e| anyhow::anyhow!("corrupt stored AVL status on {}: {e}", vendor.id))?;
    Ok(Some((vendor, status)))
}

/// Create a PO (state `Draft`) with its lines, after the AVL gate clears.
/// Fires `po.created` + one `po.line_added` per line. A `Suspended`/`Revoked`
/// vendor is refused here ([[trust-code-not-operator]]) and fires the S431
/// `supplier.po_blocked_by_vendor_status` kind.
pub fn create_po(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    input: NewPo,
) -> std::result::Result<PurchaseOrder, PoError> {
    // ── Validate.
    validate_currency(&input.currency).map_err(|e| PoError::Invalid(e.to_string()))?;
    let currency = input.currency.trim().to_string();
    if input.vat_rate_pct < 0 || input.vat_rate_pct > 100 {
        return Err(PoError::Invalid(
            "VAT rate must be between 0 and 100".into(),
        ));
    }
    if input.lines.is_empty() {
        return Err(PoError::Invalid("a PO must have at least one line".into()));
    }
    let mut subtotal: i64 = 0;
    let mut prepared: Vec<(NewPoLine, i64)> = Vec::with_capacity(input.lines.len());
    for (i, l) in input.lines.into_iter().enumerate() {
        if l.description.trim().is_empty() {
            return Err(PoError::Invalid(format!("line {i}: description is blank")));
        }
        if l.quantity <= 0 {
            return Err(PoError::Invalid(format!("line {i}: quantity must be > 0")));
        }
        if l.unit_price_minor < 0 {
            return Err(PoError::Invalid(format!(
                "line {i}: unit price must be >= 0"
            )));
        }
        let lt = line_total_minor(l.quantity, l.unit_price_minor)
            .ok_or_else(|| PoError::Invalid(format!("line {i}: line total overflows")))?;
        subtotal = subtotal
            .checked_add(lt)
            .ok_or_else(|| PoError::Invalid("PO subtotal overflows".into()))?;
        prepared.push((l, lt));
    }
    let vat = vat_minor(subtotal, input.vat_rate_pct);
    let total = subtotal
        .checked_add(vat)
        .ok_or_else(|| PoError::Invalid("PO total overflows".into()))?;

    let now = now_rfc3339();
    let year = OffsetDateTime::now_utc().year();

    // ── AVL gate ([[trust-code-not-operator]]). A Suspended/Revoked vendor is
    //    refused before any number is burned; the snapshot of an allowed
    //    status rides onto the PO.
    let avl_snapshot: Option<String> = {
        let conn = Connection::open(db_path)
            .map_err(|e| PoError::Other(anyhow::anyhow!("open DuckDB for AVL gate: {e}")))?;
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .map_err(|e| PoError::Other(anyhow::anyhow!("PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}")))?;
        match resolve_avl(&conn, tenant.as_str(), input.vendor_partner_id.trim())? {
            Some((vendor, status)) if status.blocks_po() => {
                drop(conn);
                let payload = serde_json::json!({
                    "vendor_id": vendor.id,
                    "partner_id": vendor.partner_id,
                    "vendor_status": status.as_str(),
                    "attempted_at_utc": now,
                });
                append_event(
                    db_path,
                    tenant.clone(),
                    binary_hash,
                    operator,
                    EventKind::PoBlockedByVendorStatus,
                    payload,
                )?;
                return Err(PoError::BlockedByVendorStatus {
                    partner_id: vendor.partner_id,
                    status: status.as_str().to_string(),
                });
            }
            Some((_, status)) => Some(status.as_str().to_string()),
            None => None,
        }
    };

    // ── Persist (one transaction: number reserve + header + lines).
    let po_id = generate_po_id();
    let po_number;
    let mut lines_persisted: Vec<PoLine> = Vec::with_capacity(prepared.len());
    {
        let mut conn = Connection::open(db_path)
            .map_err(|e| PoError::Other(anyhow::anyhow!("open DuckDB for PO create: {e}")))?;
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .map_err(|e| PoError::Other(anyhow::anyhow!("PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}")))?;
        ensure_schema(&conn)?;
        let tx = conn.transaction().context("begin PO create transaction")?;
        po_number = reserve_po_number(&tx, tenant.as_str(), year, &now)?;
        tx.execute(
            "INSERT INTO purchase_orders (po_id, tenant_id, po_number, vendor_partner_id, currency, \
             subtotal_minor, vat_rate_pct, vat_minor, total_minor, state, vendor_avl_status, \
             issued_at_utc, expected_delivery_utc, notes, requested_by_operator, \
             approved_by_operator, approved_at_utc, created_at_utc) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'draft',?10,NULL,?11,?12,?13,NULL,NULL,?14)",
            params![
                po_id,
                tenant.as_str(),
                po_number,
                input.vendor_partner_id.trim(),
                currency,
                subtotal,
                input.vat_rate_pct,
                vat,
                total,
                avl_snapshot,
                input
                    .expected_delivery_utc
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                input.notes.trim(),
                operator,
                now,
            ],
        )
        .context("insert purchase_orders row")?;
        for (seq, (l, lt)) in prepared.iter().enumerate() {
            let pol_id = format!("pol_{}", Ulid::new());
            tx.execute(
                "INSERT INTO purchase_order_lines (pol_id, po_id, tenant_id, seq, product_id, \
                 description, quantity, unit_price_minor, currency, line_total_minor, \
                 expected_heat_lot_required, received_quantity) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0)",
                params![
                    pol_id,
                    po_id,
                    tenant.as_str(),
                    seq as i32,
                    l.product_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty()),
                    l.description.trim(),
                    l.quantity,
                    l.unit_price_minor,
                    currency,
                    lt,
                    l.expected_heat_lot_required,
                ],
            )
            .context("insert purchase_order_lines row")?;
            lines_persisted.push(PoLine {
                pol_id,
                po_id: po_id.clone(),
                product_id: l
                    .product_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                description: l.description.trim().to_string(),
                quantity: l.quantity,
                unit_price_minor: l.unit_price_minor,
                currency: currency.clone(),
                line_total_minor: *lt,
                expected_heat_lot_required: l.expected_heat_lot_required,
                received_quantity: 0,
            });
        }
        tx.commit().context("commit PO create transaction")?;
    }

    // ── Audit (after the write conn drops — DuckDB single-writer rule).
    append_event(
        db_path,
        tenant.clone(),
        binary_hash,
        operator,
        EventKind::PoCreated,
        serde_json::json!({
            "po_id": po_id,
            "po_number": po_number,
            "vendor_partner_id": input.vendor_partner_id.trim(),
            "currency": currency,
            "subtotal_minor": subtotal,
            "vat_minor": vat,
            "total_minor": total,
            "vendor_avl_status": avl_snapshot.clone().unwrap_or_else(|| "none".to_string()),
            "line_count": lines_persisted.len(),
            "operator_user_id": operator,
            "created_at_utc": now,
        }),
    )?;
    for l in &lines_persisted {
        append_event(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            EventKind::PoLineAdded,
            serde_json::json!({
                "po_id": po_id,
                "pol_id": l.pol_id,
                "product_id": l.product_id,
                "description": l.description,
                "quantity": l.quantity,
                "unit_price_minor": l.unit_price_minor,
                "line_total_minor": l.line_total_minor,
                "expected_heat_lot_required": l.expected_heat_lot_required,
                "operator_user_id": operator,
            }),
        )?;
    }

    reread_po(db_path, tenant, &po_id)
}

/// Apply an operator-driven PO transition (issue / cancel / close). Validates the
/// edge against [`allowed_transition`]; an issue additionally requires an
/// `approved_by_operator` and re-checks the live AVL status
/// ([[trust-code-not-operator]]). Fires the matching `po.*` event.
pub fn transition_po(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    po_id: &str,
    to: PoState,
    approved_by_operator: Option<&str>,
) -> std::result::Result<PurchaseOrder, PoError> {
    let now = now_rfc3339();
    // Load + validate + (for issue) AVL re-check, then write, all under a scoped
    // conn so the audit append opens its own writer afterward.
    let (from, po_number, vendor_partner_id, approver) = {
        let conn = Connection::open(db_path)
            .map_err(|e| PoError::Other(anyhow::anyhow!("open DuckDB for PO transition: {e}")))?;
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .map_err(|e| PoError::Other(anyhow::anyhow!("PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}")))?;
        ensure_schema(&conn)?;
        let Some(po) = get_po(&conn, tenant.as_str(), po_id)? else {
            return Err(PoError::NotFound(po_id.to_string()));
        };
        let from = po.state;
        if from == to {
            return Err(PoError::IllegalTransition(format!(
                "PO already in state {}",
                to.as_db_str()
            )));
        }
        if !allowed_transition(from, to) {
            return Err(PoError::IllegalTransition(format!(
                "transition {} → {} is not allowed",
                from.as_db_str(),
                to.as_db_str()
            )));
        }
        let approver = if to == PoState::IssuedToVendor {
            let a = approved_by_operator
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    PoError::Invalid("issuing a PO requires approved_by_operator".into())
                })?;
            // Re-check the live AVL status — a vendor suspended/revoked AFTER
            // create, or still Pending approval, blocks the issue.
            if let Some((_, status)) = resolve_avl(&conn, tenant.as_str(), &po.vendor_partner_id)? {
                if status.blocks_po() {
                    return Err(PoError::BlockedByVendorStatus {
                        partner_id: po.vendor_partner_id.clone(),
                        status: status.as_str().to_string(),
                    });
                }
                if status == ApprovedStatus::Pending {
                    return Err(PoError::IllegalTransition(format!(
                        "vendor {} is still Pending AVL approval; approve the vendor before issuing",
                        po.vendor_partner_id
                    )));
                }
            }
            Some(a.to_string())
        } else {
            None
        };
        // Apply the write.
        match to {
            PoState::IssuedToVendor => {
                conn.execute(
                    "UPDATE purchase_orders SET state = 'issued_to_vendor', issued_at_utc = ?3, \
                     approved_by_operator = ?4, approved_at_utc = ?3 \
                     WHERE tenant_id = ?1 AND po_id = ?2",
                    params![tenant.as_str(), po_id, now, approver],
                )
                .context("update po (issued)")?;
            }
            _ => {
                conn.execute(
                    "UPDATE purchase_orders SET state = ?3 WHERE tenant_id = ?1 AND po_id = ?2",
                    params![tenant.as_str(), po_id, to.as_db_str()],
                )
                .context("update po state")?;
            }
        }
        (from, po.po_number, po.vendor_partner_id, approver)
    };

    match to {
        PoState::IssuedToVendor => append_event(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            EventKind::PoIssued,
            serde_json::json!({
                "po_id": po_id,
                "po_number": po_number,
                "vendor_partner_id": vendor_partner_id,
                "approved_by_operator": approver,
                "issued_at_utc": now,
            }),
        )?,
        PoState::Cancelled => append_event(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            EventKind::PoCancelled,
            serde_json::json!({
                "po_id": po_id,
                "po_number": po_number,
                "from_state": from.as_db_str(),
                "operator_user_id": operator,
                "cancelled_at_utc": now,
            }),
        )?,
        PoState::Closed => append_event(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            EventKind::PoClosed,
            serde_json::json!({
                "po_id": po_id,
                "po_number": po_number,
                "operator_user_id": operator,
                "closed_at_utc": now,
            }),
        )?,
        _ => unreachable!("allowed_transition only permits issue/cancel/close as targets"),
    }

    reread_po(db_path, tenant, po_id)
}

/// One operator-supplied receipt line.
#[derive(Debug, Clone, Deserialize)]
pub struct ReceiptLineInput {
    pub pol_id: String,
    pub received_quantity: i64,
    pub inspection_pass: bool,
    #[serde(default)]
    pub inspection_notes: String,
    #[serde(default)]
    pub heat_lot: Option<String>,
}

/// Operator-supplied delivery receipt.
#[derive(Debug, Clone, Deserialize)]
pub struct NewReceipt {
    pub delivery_note_number: String,
    pub lines: Vec<ReceiptLineInput>,
}

/// Record a delivery receipt: increment per-line `received_quantity`, advance the
/// PO state ([`receipt_state_after`]), fire `po.receipt_recorded` +
/// `po.partially_received`/`po.received`, and auto-create an NCR (S439) for any
/// line whose `inspection_pass == false` ([[trust-code-not-operator]] — a failed
/// delivery cannot be received without a quality record).
pub fn record_receipt(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    po_id: &str,
    input: NewReceipt,
) -> std::result::Result<PurchaseOrder, PoError> {
    let now = now_rfc3339();
    let delivery_note = input.delivery_note_number.trim().to_string();
    if delivery_note.is_empty() {
        return Err(PoError::Invalid("delivery note number is required".into()));
    }

    // ── Load + validate against the live lines, then write under one tx.
    struct AppliedReceipt {
        por_id: String,
        pol_id: String,
        received_quantity: i64,
        inspection_pass: bool,
        inspection_notes: String,
        heat_lot: Option<String>,
        line_description: String,
    }
    let (po_number, vendor_partner_id, applied, new_state) = {
        let mut conn = Connection::open(db_path)
            .map_err(|e| PoError::Other(anyhow::anyhow!("open DuckDB for PO receipt: {e}")))?;
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .map_err(|e| PoError::Other(anyhow::anyhow!("PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}")))?;
        ensure_schema(&conn)?;
        let Some(po) = get_po(&conn, tenant.as_str(), po_id)? else {
            return Err(PoError::NotFound(po_id.to_string()));
        };
        if !po.state.accepts_receipt() {
            return Err(PoError::IllegalTransition(format!(
                "cannot receive against a PO in state {}",
                po.state.as_db_str()
            )));
        }
        let mut lines = list_po_lines(&conn, tenant.as_str(), po_id)?;

        // Validate each receipt line against the live line set.
        let mut applied: Vec<AppliedReceipt> = Vec::new();
        for rl in &input.lines {
            if rl.received_quantity == 0 {
                continue; // line not part of this delivery
            }
            if rl.received_quantity < 0 {
                return Err(PoError::Invalid("received quantity must be >= 0".into()));
            }
            let Some(line) = lines.iter().find(|l| l.pol_id == rl.pol_id) else {
                return Err(PoError::Invalid(format!("unknown line {}", rl.pol_id)));
            };
            let remaining = line.quantity - line.received_quantity;
            if rl.received_quantity > remaining {
                return Err(PoError::Invalid(format!(
                    "line {}: received {} exceeds remaining {}",
                    rl.pol_id, rl.received_quantity, remaining
                )));
            }
            let heat_lot = rl
                .heat_lot
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if line.expected_heat_lot_required && heat_lot.is_none() {
                return Err(PoError::Invalid(format!(
                    "line {}: this material requires a heat/lot number on receipt",
                    rl.pol_id
                )));
            }
            applied.push(AppliedReceipt {
                por_id: format!("por_{}", Ulid::new()),
                pol_id: rl.pol_id.clone(),
                received_quantity: rl.received_quantity,
                inspection_pass: rl.inspection_pass,
                inspection_notes: rl.inspection_notes.trim().to_string(),
                heat_lot,
                line_description: line.description.clone(),
            });
        }
        if applied.is_empty() {
            return Err(PoError::Invalid(
                "a receipt must record at least one received line".into(),
            ));
        }

        // Write the receipt rows + advance the line counters under one tx.
        let tx = conn.transaction().context("begin PO receipt transaction")?;
        for a in &applied {
            tx.execute(
                "INSERT INTO purchase_order_receipts (por_id, po_id, pol_id, tenant_id, \
                 received_at_utc, received_by_operator, delivery_note_number, received_quantity, \
                 inspection_pass, inspection_notes, heat_lot_assigned, ncr_id) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL)",
                params![
                    a.por_id,
                    po_id,
                    a.pol_id,
                    tenant.as_str(),
                    now,
                    operator,
                    delivery_note,
                    a.received_quantity,
                    a.inspection_pass,
                    a.inspection_notes,
                    a.heat_lot,
                ],
            )
            .context("insert purchase_order_receipts row")?;
            tx.execute(
                "UPDATE purchase_order_lines SET received_quantity = received_quantity + ?3 \
                 WHERE tenant_id = ?1 AND pol_id = ?2",
                params![tenant.as_str(), a.pol_id, a.received_quantity],
            )
            .context("advance line received_quantity")?;
            // Reflect into the in-memory copy for the state derivation.
            if let Some(l) = lines.iter_mut().find(|l| l.pol_id == a.pol_id) {
                l.received_quantity += a.received_quantity;
            }
        }
        let new_state = receipt_state_after(&lines);
        tx.execute(
            "UPDATE purchase_orders SET state = ?3 WHERE tenant_id = ?1 AND po_id = ?2",
            params![tenant.as_str(), po_id, new_state.as_db_str()],
        )
        .context("update po state after receipt")?;
        tx.commit().context("commit PO receipt transaction")?;
        (po.po_number, po.vendor_partner_id, applied, new_state)
    };

    let any_failed = applied.iter().any(|a| !a.inspection_pass);
    append_event(
        db_path,
        tenant.clone(),
        binary_hash,
        operator,
        EventKind::PoReceiptRecorded,
        serde_json::json!({
            "po_id": po_id,
            "po_number": po_number,
            "delivery_note_number": delivery_note,
            "received_line_count": applied.len(),
            "any_inspection_failed": any_failed,
            "received_by_operator": operator,
            "received_at_utc": now,
        }),
    )?;

    // Auto-NCR for each failed inspection (S439). create_ncr opens its own conn,
    // so this runs after the receipt tx has committed.
    for a in applied.iter().filter(|a| !a.inspection_pass) {
        let description = format!(
            "Bejövő ellenőrzés FAIL — PO {po_number} tétel „{}” (beszállító {vendor_partner_id}): {} / \
             Incoming inspection FAIL on PO {po_number} line \"{}\" (vendor {vendor_partner_id}): {}",
            a.line_description,
            a.inspection_notes,
            a.line_description,
            a.inspection_notes,
        );
        let ncr = crate::quality::create_ncr(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            crate::quality::NewNcr {
                severity: crate::quality::NcrSeverity::Major,
                category: crate::quality::NcrCategory::SupplierIssue,
                description,
                affected_part_uids: vec![],
                affected_wo_ids: vec![],
                affected_heat_lots: a.heat_lot.clone().into_iter().collect(),
                photos: vec![],
            },
        )
        .map_err(|e| match e {
            crate::quality::QualityError::Other(e) => PoError::Other(e),
            other => PoError::Other(anyhow::anyhow!("auto-NCR failed: {other}")),
        })?;
        // Stamp the receipt row with its NCR id.
        {
            let conn = Connection::open(db_path)
                .map_err(|e| PoError::Other(anyhow::anyhow!("reopen DuckDB to link NCR: {e}")))?;
            conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
                .map_err(|e| PoError::Other(anyhow::anyhow!("PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}")))?;
            conn.execute(
                "UPDATE purchase_order_receipts SET ncr_id = ?3 WHERE tenant_id = ?1 AND por_id = ?2",
                params![tenant.as_str(), a.por_id, ncr.ncr_id],
            )
            .context("link receipt row to NCR")?;
        }
        append_event(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            EventKind::PoIncomingInspectionFailed,
            serde_json::json!({
                "po_id": po_id,
                "po_number": po_number,
                "pol_id": a.pol_id,
                "vendor_partner_id": vendor_partner_id,
                "ncr_id": ncr.ncr_id,
                "inspection_notes": a.inspection_notes,
                "operator_user_id": operator,
                "at_utc": now,
            }),
        )?;
    }

    // State-change event.
    let kind = match new_state {
        PoState::Received => EventKind::PoReceived,
        _ => EventKind::PoPartiallyReceived,
    };
    append_event(
        db_path,
        tenant.clone(),
        binary_hash,
        operator,
        kind,
        serde_json::json!({
            "po_id": po_id,
            "po_number": po_number,
            "operator_user_id": operator,
            "changed_at_utc": now,
        }),
    )?;

    reread_po(db_path, tenant, po_id)
}

fn reread_po(
    db_path: &std::path::Path,
    tenant: TenantId,
    po_id: &str,
) -> std::result::Result<PurchaseOrder, PoError> {
    let conn = Connection::open(db_path)
        .map_err(|e| PoError::Other(anyhow::anyhow!("reopen DuckDB: {e}")))?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .map_err(|e| PoError::Other(anyhow::anyhow!("PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}")))?;
    get_po(&conn, tenant.as_str(), po_id)?.ok_or_else(|| PoError::NotFound(po_id.to_string()))
}

/// Open a fresh `Ledger` (after the read/write conn drops — DuckDB single-writer
/// rule) and append one purchasing audit entry.
fn append_event(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<()> {
    let mut ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to record purchasing event")?;
    ledger
        .append(
            kind,
            serde_json::to_vec(&payload).expect("serialize purchasing payload"),
            Actor::from_local_cli(Ulid::new().to_string(), operator),
            None,
        )
        .context("append purchasing audit entry")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_line(qty: i64, received: i64) -> PoLine {
        PoLine {
            pol_id: format!("pol_{}", Ulid::new()),
            po_id: "po_x".into(),
            product_id: None,
            description: "bar stock".into(),
            quantity: qty,
            unit_price_minor: 1000,
            currency: "HUF".into(),
            line_total_minor: qty * 1000,
            expected_heat_lot_required: false,
            received_quantity: received,
        }
    }

    #[test]
    fn po_id_is_prefixed_ulid() {
        let id = generate_po_id();
        assert!(id.starts_with("po_"), "{id}");
        assert_eq!(id.len(), 3 + 26);
    }

    #[test]
    fn po_number_format_pads_to_four_then_grows() {
        assert_eq!(format_po_number(2026, 1), "PO-2026-0001");
        assert_eq!(format_po_number(2026, 42), "PO-2026-0042");
        assert_eq!(format_po_number(2026, 9999), "PO-2026-9999");
        assert_eq!(format_po_number(2026, 10000), "PO-2026-10000");
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        use PoState::*;
        assert!(allowed_transition(Draft, IssuedToVendor));
        assert!(allowed_transition(Received, Closed));
    }

    #[test]
    fn illegal_transitions_are_refused() {
        use PoState::*;
        // Cannot skip straight from Draft to Received.
        assert!(!allowed_transition(Draft, Received));
        // Cannot operator-set the receipt-driven states.
        assert!(!allowed_transition(IssuedToVendor, PartiallyReceived));
        assert!(!allowed_transition(IssuedToVendor, Received));
        // Closed / Cancelled are terminal.
        assert!(Closed.is_terminal());
        assert!(Cancelled.is_terminal());
        assert!(!allowed_transition(Closed, IssuedToVendor));
        assert!(!allowed_transition(Received, Cancelled));
    }

    #[test]
    fn cancel_allowed_from_pre_received_states_only() {
        use PoState::*;
        assert!(allowed_transition(Draft, Cancelled));
        assert!(allowed_transition(IssuedToVendor, Cancelled));
        assert!(allowed_transition(PartiallyReceived, Cancelled));
        assert!(!allowed_transition(Received, Cancelled));
    }

    #[test]
    fn receipt_state_derivation_is_pure() {
        // Nothing received → unchanged (issued).
        assert_eq!(
            receipt_state_after(&[mk_line(5, 0), mk_line(3, 0)]),
            PoState::IssuedToVendor
        );
        // Some but not all → partial.
        assert_eq!(
            receipt_state_after(&[mk_line(5, 2), mk_line(3, 0)]),
            PoState::PartiallyReceived
        );
        // One fully, one partly → partial.
        assert_eq!(
            receipt_state_after(&[mk_line(5, 5), mk_line(3, 1)]),
            PoState::PartiallyReceived
        );
        // All fully received → received.
        assert_eq!(
            receipt_state_after(&[mk_line(5, 5), mk_line(3, 3)]),
            PoState::Received
        );
        // Over-received (defensive: >= counts as full).
        assert_eq!(receipt_state_after(&[mk_line(5, 6)]), PoState::Received);
    }

    #[test]
    fn money_helpers_compute_and_guard_overflow() {
        assert_eq!(line_total_minor(3, 1500), Some(4500));
        assert_eq!(line_total_minor(i64::MAX, 2), None);
        // 27% VAT on 10000 minor = 2700.
        assert_eq!(vat_minor(10000, 27), 2700);
        assert_eq!(vat_minor(10000, 0), 0);
        // Floor division.
        assert_eq!(vat_minor(101, 27), 27);
    }

    #[test]
    fn currency_validation_requires_iso_4217() {
        assert!(validate_currency("HUF").is_ok());
        assert!(validate_currency("eur").is_err());
        assert!(validate_currency("EURO").is_err());
        assert!(validate_currency("").is_err());
    }

    #[test]
    fn po_numbering_is_monotonic_per_tenant_and_resets_annually() {
        let dir = std::env::temp_dir()
            .join("aberp-po-numbering-test")
            .join(Ulid::new().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("aberp.duckdb");
        let mut conn = Connection::open(&db).unwrap();
        ensure_schema(&conn).unwrap();
        let now = "2026-06-16T00:00:00Z";

        let reserve = |conn: &mut Connection, tenant: &str, year: i32| -> String {
            let tx = conn.transaction().unwrap();
            let n = reserve_po_number(&tx, tenant, year, now).unwrap();
            tx.commit().unwrap();
            n
        };

        // Monotonic + gap-free within (tenant, year).
        assert_eq!(reserve(&mut conn, "t1", 2026), "PO-2026-0001");
        assert_eq!(reserve(&mut conn, "t1", 2026), "PO-2026-0002");
        assert_eq!(reserve(&mut conn, "t1", 2026), "PO-2026-0003");
        // Annual reset: a new year is a fresh bucket starting at 1.
        assert_eq!(reserve(&mut conn, "t1", 2027), "PO-2027-0001");
        // Per-tenant isolation: a different tenant has its own sequence.
        assert_eq!(reserve(&mut conn, "t2", 2026), "PO-2026-0001");
        // The 2026 t1 bucket keeps advancing independently.
        assert_eq!(reserve(&mut conn, "t1", 2026), "PO-2026-0004");
    }
}
