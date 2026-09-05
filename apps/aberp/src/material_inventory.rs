//! S273 / PR-262 / ADR-0069 — material-side inventory balances +
//! reservations. The DEAL saga in [`crate::quote_deal`] writes through
//! this module to increment `committed_qty` on the
//! `inventory_balances` row keyed by `(tenant_id, material_grade)` and
//! insert a paired `inventory_reservations` row inside ONE DB
//! transaction.
//!
//! # Why a new module (and not [`aberp_inventory`])
//!
//! The pre-existing `aberp-inventory` crate (S231 / PR-227 /
//! ADR-0061) tracks **product-side** stock: an append-only
//! `stock_movements` ledger + a denormalised `stock_qty` cache on the
//! `products` table. That covers finished-goods + WIP; it does NOT
//! cover **raw material** balances keyed on
//! `quoting_materials.grade`. The two are different domains:
//!
//!   * product-side: `(tenant, product_id)` → `qty` of a FG SKU; reads
//!     for low-stock alerts on the workshop wall TV.
//!   * material-side: `(tenant, material_grade)` → four quantities
//!     (`on_hand` / `reserved` / `committed` / `consumed`) per the
//!     state machine in ADR-0069; reads for DEAL-saga validation +
//!     a forthcoming purchasing daemon.
//!
//! Brief pushback #1 was: "if Inventory v1 doesn't actually exist as
//! a real table, push back and decide between (a) creating it
//! minimally in this PR or (b) deferring." It DOES exist — for
//! products. The material-side does not, so (a) lands here, sitting
//! alongside the products crate rather than re-using it. The two are
//! orthogonal enough that wedging material balances into
//! `aberp-inventory::repository` would muddle the product `stock_qty`
//! reader against four material quantities.
//!
//! # ADR-0069 state machine
//!
//! The four quantities track the same physical material through its
//! lifecycle:
//!
//!   * `on_hand` — physical stock available to sell.
//!   * `reserved` — soft-committed to an INDICATIVE quote (the
//!     storefront will trigger this in a future PR; today no handler
//!     emits `MaterialReserved`).
//!   * `committed` — hard-committed at DEAL time. Customer is paying,
//!     so this material is OFF the sale-able pool but still ON the
//!     shelf.
//!   * `consumed` — physically used in production (future workshop-
//!     completion hook).
//!
//! Invariant — enforced at every write in this module, NOT as a SQL
//! CHECK per `[[no-sql-specific]]`:
//!
//!   `on_hand_qty >= reserved_qty + committed_qty`
//!
//! (`consumed_qty` is separate — when material is consumed it's
//! debited from BOTH `committed_qty` AND `on_hand_qty`; the invariant
//! continues to hold post-debit.)
//!
//! # What this PR wires
//!
//! Only one transition is wired today:
//!
//!   * DEAL saga → [`commit_material_in_tx`] increments
//!     `committed_qty += qty` and inserts an
//!     `inventory_reservations` row with `state = 'committed'`.
//!
//! The other three transitions (`reserved` / `consumed` / `released`)
//! have their EventKinds defined (the four-way `inventory.*` prefix
//! family is named in one F12 ritual) but no handler emits them yet.
//! Sequence (per the auto-quoting strand backlog):
//!
//!   * `reserved` — storefront-side indicative-quote hook (future).
//!   * `consumed` — workshop Work-Order-Complete hook (future).
//!   * `released` — operator-driven reservation cancel (future).
//!
//! # v1 limitations (documented in the PR body, surfaced in the
//! Inventory Balances SPA banner)
//!
//!   * **No reservation timeout.** Sticky like `stock_alert` — the
//!     operator must manually release per the brief.
//!   * **Reverse transitions are out of scope.** Reservation rows are
//!     append-only-by-state today; flipping a row back from
//!     `committed` → `reserved` requires an explicit operator action
//!     (future).
//!   * **`qty` is QUOTE quantity, not material volume.** A quote for
//!     12 units is stored as `qty = 12`. The real conversion is
//!     `units → mm³ (per-part) → kg (× density)`; mm³ comes from the
//!     CAD-extract [`aberp_quote_engine::FeatureGraph`] volume,
//!     density from [`crate::quoting_materials`]. The plumbing is
//!     S275+ — until then the units placeholder lets the DEAL saga
//!     book a reservation against the material-grade balance, even
//!     though the numbers are NOT in physical units. The SPA view's
//!     header banner names this explicitly so operators don't read
//!     the column as "kg on the shelf."
//!   * **Auto-upsert at zero.** A DEAL against a material with no
//!     `inventory_balances` row inserts an all-zero row first, then
//!     the validate-available check fires — the operator's 409 body
//!     names `on_hand: 0` so the fix path is obvious: open the SPA
//!     view, set `on_hand_qty` to the stocked amount, retry the DEAL.

use anyhow::{Context, Result};
use duckdb::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, BinaryHash, EventKind, LedgerMeta, TenantId};
use aberp_compliance::lot_heat::{validate_mtr_url, LotId};
use aberp_db::HandleArc;

/// `kg` is the default UoM for material-side balances. The catalogue
/// quotes cost in `cost_per_kg_eur` + density in `g_cm3`; mass (kg) is
/// the only UoM the rest of the auto-quoting strand consumes, so this
/// is the v1 invariant. A future PR widening this is a deliberate
/// schema decision (the SPA's UoM column makes the convention visible).
pub const DEFAULT_UOM: &str = "kg";

/// S275 / PR-264 / F1 — closed-vocab tag for the `qty_unit_kind` column
/// on `inventory_reservations` AND for the same field on the
/// [`MaterialCommittedPayload`] audit body. Pre-S275 the DEAL saga
/// passed quote *units* (number of parts) verbatim as `qty`, leaving
/// the inventory balance numerically misleading — the SPA banner
/// disclaimed it but the audit payload couldn't be cross-referenced for
/// forensics without operator memory. Stamping the kind on the row +
/// payload makes the unit explicit so a forensic walk N months out can
/// read `"units"` and know NOT to interpret it as kg.
///
/// `Units`: legacy v1 — quote-quantity verbatim, NOT a mass.
/// `Kg`: post-conversion — the units → mm³ (FeatureGraph volume) → kg
/// (catalogue density) pipeline has fired. Until that pipeline lands
/// the SPA never receives this variant; reserving the name now keeps
/// the audit walk forward-compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QtyUnitKind {
    Units,
    Kg,
}

impl QtyUnitKind {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            QtyUnitKind::Units => "units",
            QtyUnitKind::Kg => "kg",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "units" => Some(QtyUnitKind::Units),
            "kg" => Some(QtyUnitKind::Kg),
            _ => None,
        }
    }
}

/// Closed-vocab reservation state per ADR-0069. App-layer enforced; the
/// DB column is plain VARCHAR. Adding a state is a coordinated edit
/// across the storage-string round-trip pin, the round-trip helpers,
/// any SPA filter dropdown, and (if the new state is reachable from a
/// handler) the corresponding `EventKind` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    /// Soft-committed to an indicative quote. Not emitted today.
    Reserved,
    /// Hard-committed by the DEAL saga.
    Committed,
    /// Physically consumed in production.
    Consumed,
    /// Released back to the sale-able pool.
    Released,
}

impl ReservationState {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            ReservationState::Reserved => "reserved",
            ReservationState::Committed => "committed",
            ReservationState::Consumed => "consumed",
            ReservationState::Released => "released",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "reserved" => Some(ReservationState::Reserved),
            "committed" => Some(ReservationState::Committed),
            "consumed" => Some(ReservationState::Consumed),
            "released" => Some(ReservationState::Released),
            _ => None,
        }
    }

    pub const ALL: [ReservationState; 4] = [
        ReservationState::Reserved,
        ReservationState::Committed,
        ReservationState::Consumed,
        ReservationState::Released,
    ];
}

/// Closed-vocab failure modes for the material-side write paths. The
/// DEAL saga downcasts on this so the route layer can map to the right
/// HTTP 409 machine code.
#[derive(Debug, Error, PartialEq)]
pub enum MaterialInventoryError {
    /// The check `on_hand_qty >= reserved_qty + committed_qty +
    /// requested_qty` failed. Surfaces enough numbers for the SPA toast
    /// to render an actionable error: open the Inventory Balances view,
    /// set `on_hand_qty` to the true stocked amount, retry the DEAL.
    #[error(
        "material {material_grade}: insufficient stock (requested {requested}, on_hand {on_hand}, already reserved {already_reserved}, already committed {already_committed})"
    )]
    InsufficientMaterial {
        material_grade: String,
        requested: f64,
        on_hand: f64,
        already_reserved: f64,
        already_committed: f64,
    },
    /// D-11 — a release/consume named a `reservation_id` that does not
    /// exist for this tenant.
    #[error("reservation {reservation_id} not found")]
    ReservationNotFound { reservation_id: String },
    /// D-11 — a release/consume was attempted from a TERMINAL reservation
    /// state (`consumed` / `released`), or a consume from a state it does
    /// not accept. The reservation FSM (ADR-0069 §Invariant 3) only allows
    /// `{reserved, committed} → released` and `{reserved, committed} →
    /// consumed`; a terminal row is immutable (a correction is a new
    /// reservation, never an edit of the old one).
    #[error(
        "reservation {reservation_id} cannot be {attempted}: it is already {from} (a terminal state)"
    )]
    IllegalTransition {
        reservation_id: String,
        from: &'static str,
        attempted: &'static str,
    },
    /// D-11 — `attach_material_cert` was given a `cert_kind` outside the
    /// closed vocab (`mill_cert` / `cofa` / `heat_treatment`).
    #[error("unknown cert_kind `{cert_kind}` (expected mill_cert / cofa / heat_treatment)")]
    CertKindUnknown { cert_kind: String },
    /// D-11 — the cert retention URL is empty, over-long, or not one of the
    /// accepted schemes (`https://` / `http://` / `file://`). The URL is a
    /// reference to where the document is retained; the bytes are not stored
    /// here (same posture as the MTR `file://` reference).
    #[error("invalid cert_url: {reason}")]
    CertUrlInvalid { reason: String },
}

impl MaterialInventoryError {
    /// Machine code surfaced on the 409 body so the SPA toast routes to
    /// the right copy. Closed-vocab.
    pub fn machine_code(&self) -> &'static str {
        match self {
            MaterialInventoryError::InsufficientMaterial { .. } => "insufficient_material",
            MaterialInventoryError::ReservationNotFound { .. } => "reservation_not_found",
            MaterialInventoryError::IllegalTransition { .. } => "illegal_reservation_transition",
            MaterialInventoryError::CertKindUnknown { .. } => "cert_kind_unknown",
            MaterialInventoryError::CertUrlInvalid { .. } => "cert_url_invalid",
        }
    }
}

const INVENTORY_BALANCES_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS inventory_balances (
    tenant_id        VARCHAR NOT NULL,
    material_grade   VARCHAR NOT NULL,
    on_hand_qty      DOUBLE  NOT NULL DEFAULT 0,
    reserved_qty     DOUBLE  NOT NULL DEFAULT 0,
    committed_qty    DOUBLE  NOT NULL DEFAULT 0,
    consumed_qty     DOUBLE  NOT NULL DEFAULT 0,
    unit_of_measure  VARCHAR NOT NULL,
    last_updated     VARCHAR NOT NULL,
    PRIMARY KEY (tenant_id, material_grade)
);

CREATE TABLE IF NOT EXISTS inventory_reservations (
    reservation_id   VARCHAR NOT NULL PRIMARY KEY,
    tenant_id        VARCHAR NOT NULL,
    quote_id         VARCHAR NOT NULL,
    material_grade   VARCHAR NOT NULL,
    qty              DOUBLE  NOT NULL,
    state            VARCHAR NOT NULL,
    created_at       VARCHAR NOT NULL,
    transitioned_at  VARCHAR NOT NULL
);
-- S275 / PR-264 / F1 — additive `qty_unit_kind` column. Stamps what
-- `qty` actually means on this row: `units` (current — QUOTE units,
-- the storefront's part count, NOT mass) or `kg` (post-S275+ engine
-- conversion). A future ALTER must NOT add a SQL DEFAULT (DuckDB
-- replay-clobber trap, same as the S271 `stock_alert` and
-- `quoting_materials` notes). Coerced to a known value in the app
-- layer at write time; reads tolerate NULL on pre-S275 rows by
-- treating it as `units`.
ALTER TABLE inventory_reservations
    ADD COLUMN IF NOT EXISTS qty_unit_kind VARCHAR;
-- S432 (ADR-0085) — heat-lot traceability columns on the per-grade balance
-- row. All NULLABLE for back-compat: existing rows are NOT backfilled (a
-- missing heat lot is the signal the defense WO-start gate reads). A future
-- ALTER must NOT add a SQL DEFAULT (same DuckDB replay-clobber trap as the
-- qty_unit_kind note above). The app layer treats a NULL/empty heat_lot_number
-- as 'no heat lot assigned'.
ALTER TABLE inventory_balances
    ADD COLUMN IF NOT EXISTS heat_lot_number VARCHAR;
ALTER TABLE inventory_balances
    ADD COLUMN IF NOT EXISTS mill_test_report_url VARCHAR;
ALTER TABLE inventory_balances
    ADD COLUMN IF NOT EXISTS heat_assigned_at_utc VARCHAR;
ALTER TABLE inventory_balances
    ADD COLUMN IF NOT EXISTS heat_assigned_by_operator VARCHAR;
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` for both tables. Called at
/// the top of every reader + at the head of the DEAL saga's tx (the
/// saga's mutation is the only write path today; the SPA view is
/// read-only).
///
/// PUSHBACK on the brief's surrogate `BIGINT id` PRIMARY KEY: this
/// codebase's convention for natural-key tables is to use the natural
/// composite (`quoting_materials.grade`, `partners.tax_number`,
/// `invoice_series.code`) rather than a surrogate `id`. The
/// `(tenant_id, material_grade)` composite is what every reader will
/// `WHERE` on; an extra `BIGINT id` would be dead weight per CLAUDE.md
/// rule 13. For `inventory_reservations`, the natural identifier is a
/// ULID (`res_<ULID>` — same shape as `so_<ULID>` + `wo_<ULID>` minted
/// by the DEAL saga itself), which round-trips through the existing
/// `Ulid::new()` pattern.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn (read_returns_readonly
    // read()-side); the schema is created by a writer before any read reaches
    // here. A genuine write mis-routed through read() still fails loud (F5).
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(INVENTORY_BALANCES_SCHEMA_SQL)
        .context("ensure inventory_balances + inventory_reservations schema")?;
    Ok(())
}

/// A per-(tenant, material_grade) balance row. Returned by the SPA
/// reader and surfaced inside the DEAL saga's audit payload so a
/// forensic walk can prove the post-increment invariant held.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub tenant_id: String,
    pub material_grade: String,
    pub on_hand_qty: f64,
    pub reserved_qty: f64,
    pub committed_qty: f64,
    pub consumed_qty: f64,
    pub unit_of_measure: String,
    pub last_updated: String,
    /// Derived: `on_hand_qty - reserved_qty - committed_qty`. The
    /// invariant says this is `>= 0` at all times. The SPA view
    /// highlights NEGATIVE rows in red so an operator can spot
    /// invariant breach immediately (would only happen if someone
    /// bypassed `commit_material_in_tx` — defense-in-depth render).
    pub available_qty: f64,
    /// S432 (ADR-0085) — supplier-issued heat lot bound to this grade's
    /// stock, or `None` until an operator assigns one. The chain-of-custody
    /// anchor the defense/aerospace WO-start gate reads.
    pub heat_lot_number: Option<String>,
    /// S432 — `file://` URL of the Mill Test Report (3.1 cert), when uploaded.
    pub mill_test_report_url: Option<String>,
    /// S432 — RFC-3339 stamp of the heat-lot assignment.
    pub heat_assigned_at_utc: Option<String>,
    /// S432 — operator login who bound the heat lot (accountability anchor).
    pub heat_assigned_by_operator: Option<String>,
}

impl Balance {
    #[allow(clippy::too_many_arguments)]
    fn from_columns(
        tenant_id: String,
        material_grade: String,
        on_hand_qty: f64,
        reserved_qty: f64,
        committed_qty: f64,
        consumed_qty: f64,
        unit_of_measure: String,
        last_updated: String,
        heat_lot_number: Option<String>,
        mill_test_report_url: Option<String>,
        heat_assigned_at_utc: Option<String>,
        heat_assigned_by_operator: Option<String>,
    ) -> Self {
        let available_qty = on_hand_qty - reserved_qty - committed_qty;
        Self {
            tenant_id,
            material_grade,
            on_hand_qty,
            reserved_qty,
            committed_qty,
            consumed_qty,
            unit_of_measure,
            last_updated,
            available_qty,
            heat_lot_number,
            mill_test_report_url,
            heat_assigned_at_utc,
            heat_assigned_by_operator,
        }
    }
}

/// List every `inventory_balances` row for a tenant. SPA reader at
/// `GET /api/inventory-balances`. Ordering is by `material_grade` so
/// the SPA renders alphabetically without an explicit `ORDER BY`
/// option (consistent with `quoting_materials` posture).
pub fn list_balances_for_tenant(conn: &Connection, tenant: &str) -> Result<Vec<Balance>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT material_grade, on_hand_qty, reserved_qty, committed_qty,
                    consumed_qty, unit_of_measure, last_updated,
                    heat_lot_number, mill_test_report_url,
                    heat_assigned_at_utc, heat_assigned_by_operator
               FROM inventory_balances
              WHERE tenant_id = ?1
              ORDER BY material_grade",
        )
        .context("prepare list_balances_for_tenant")?;
    let rows = stmt
        .query_map(params![tenant], |r| {
            Ok(Balance::from_columns(
                tenant.to_string(),
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<String>>(10)?,
            ))
        })
        .context("query list_balances_for_tenant")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read inventory_balances row")?);
    }
    Ok(out)
}

/// Per-tx read of a single balance row. Returns `None` if the row does
/// not exist. Inside the saga tx, the caller upserts at zeros before
/// calling this if needed (see [`commit_material_in_tx`]).
fn read_balance_in_tx_inner<'a, R>(
    runner: &'a R,
    tenant: &str,
    material_grade: &str,
) -> Result<Option<Balance>>
where
    R: ConnLike<'a>,
{
    let mut stmt = runner
        .prepare_raw(
            "SELECT on_hand_qty, reserved_qty, committed_qty,
                    consumed_qty, unit_of_measure, last_updated,
                    heat_lot_number, mill_test_report_url,
                    heat_assigned_at_utc, heat_assigned_by_operator
               FROM inventory_balances
              WHERE tenant_id = ?1 AND material_grade = ?2
              LIMIT 1",
        )
        .context("prepare read_balance_in_tx")?;
    let mut rows = stmt
        .query(params![tenant, material_grade])
        .context("query read_balance_in_tx")?;
    let Some(row) = rows.next().context("read inventory_balances row")? else {
        return Ok(None);
    };
    Ok(Some(Balance::from_columns(
        tenant.to_string(),
        material_grade.to_string(),
        row.get::<_, f64>(0).context("get on_hand_qty")?,
        row.get::<_, f64>(1).context("get reserved_qty")?,
        row.get::<_, f64>(2).context("get committed_qty")?,
        row.get::<_, f64>(3).context("get consumed_qty")?,
        row.get::<_, String>(4).context("get unit_of_measure")?,
        row.get::<_, String>(5).context("get last_updated")?,
        row.get::<_, Option<String>>(6)
            .context("get heat_lot_number")?,
        row.get::<_, Option<String>>(7)
            .context("get mill_test_report_url")?,
        row.get::<_, Option<String>>(8)
            .context("get heat_assigned_at_utc")?,
        row.get::<_, Option<String>>(9)
            .context("get heat_assigned_by_operator")?,
    )))
}

/// Trait wrapping `Connection` + `Transaction` so the read helper can
/// be called inside the DEAL saga's tx without `Connection`-vs-`Transaction`
/// method-signature divergence. DuckDB's tx type wraps `Connection`'s
/// `prepare` under the same name + signature — but the borrow checker
/// resolves them through different deref chains, so a thin trait keeps
/// the inner function generic.
trait ConnLike<'a> {
    fn prepare_raw(&'a self, sql: &str) -> duckdb::Result<duckdb::Statement<'a>>;
}

impl<'a> ConnLike<'a> for Connection {
    fn prepare_raw(&'a self, sql: &str) -> duckdb::Result<duckdb::Statement<'a>> {
        self.prepare(sql)
    }
}

impl<'a, 'tx> ConnLike<'a> for Transaction<'tx> {
    fn prepare_raw(&'a self, sql: &str) -> duckdb::Result<duckdb::Statement<'a>> {
        self.prepare(sql)
    }
}

/// Public read for a single balance row — convenient for tests + the
/// SPA's after-DEAL response shape.
pub fn read_balance(
    conn: &Connection,
    tenant: &str,
    material_grade: &str,
) -> Result<Option<Balance>> {
    ensure_schema(conn)?;
    read_balance_in_tx_inner(conn, tenant, material_grade)
}

/// Successful outcome of a DEAL-saga material commit. Returned so the
/// saga can fold it into [`crate::quote_deal::DealSagaOutcome`] and
/// the audit payload can record the post-increment snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialCommitOutcome {
    pub reservation_id: String,
    pub material_grade: String,
    pub qty: f64,
    /// S275 / PR-264 / F1 — what `qty` actually means. Defaults to
    /// [`QtyUnitKind::Units`] on legacy commits; will flip to
    /// [`QtyUnitKind::Kg`] when the units → mm³ → kg conversion lands.
    /// `#[serde(default)]` so a wire body without the field deserializes
    /// to the legacy default (back-compat for anything that round-trips
    /// the saga outcome through JSON before the storefront cutover).
    #[serde(default = "default_qty_unit_kind")]
    pub qty_unit_kind: QtyUnitKind,
    pub balance_after: Balance,
}

fn default_qty_unit_kind() -> QtyUnitKind {
    QtyUnitKind::Units
}

/// The core write path — called inside the DEAL saga's tx. Five steps,
/// all atomic with the rest of the saga:
///
///   1. Ensure schema (idempotent — safe to re-run inside tx).
///   2. Upsert the `(tenant, material_grade)` balance row at zeros if
///      it does not exist. A first-time DEAL against a new material
///      lands the row so step 3 has something to read; the
///      validate-available check then fires against `on_hand: 0` and
///      surfaces 409 `insufficient_material` (the operator's fix path
///      is "go open the Inventory Balances view, set on_hand_qty").
///   3. Read the row, validate
///      `on_hand_qty >= reserved_qty + committed_qty + qty` — fail
///      loudly with `InsufficientMaterial` if not.
///   4. Increment `committed_qty += qty` and bump `last_updated`.
///   5. Insert a paired `inventory_reservations` row with
///      `state = 'committed'`, the freshly-minted `res_<ULID>` as PK.
///
/// Returns the after-state balance + the reservation id so the saga
/// can fold them into its outcome + audit payload. The
/// `MaterialCommitted` audit append happens OUTSIDE this function (in
/// `quote_deal.rs`) so the saga emits all four entries through one
/// `append_in_tx` cadence.
pub fn commit_material_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    quote_id: &str,
    material_grade: &str,
    qty: f64,
    qty_unit_kind: QtyUnitKind,
) -> Result<MaterialCommitOutcome> {
    // Step 1 — ensure schema. CREATE TABLE IF NOT EXISTS inside a tx is
    // fine in DuckDB; the saga's tx is the same connection.
    tx.execute_batch(INVENTORY_BALANCES_SCHEMA_SQL)
        .context("ensure inventory schema in saga tx")?;

    let now_iso = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format now for inventory writes")?;

    // Step 2 — upsert at zeros if the row doesn't exist. Use INSERT ...
    // ON CONFLICT DO NOTHING so a concurrent saga that lost the race
    // does not double-insert. DuckDB supports this since 0.7.x.
    tx.execute(
        "INSERT INTO inventory_balances (
            tenant_id, material_grade,
            on_hand_qty, reserved_qty, committed_qty, consumed_qty,
            unit_of_measure, last_updated
         ) VALUES (?1, ?2, 0, 0, 0, 0, ?3, ?4)
         ON CONFLICT (tenant_id, material_grade) DO NOTHING",
        params![tenant, material_grade, DEFAULT_UOM, &now_iso],
    )
    .context("upsert inventory_balances at zeros")?;

    // Step 3 — read + validate. The tx's row-lock is implicit because
    // we are about to UPDATE the same row.
    let before = read_balance_in_tx_inner(tx, tenant, material_grade)
        .context("read inventory_balances after upsert")?
        .ok_or_else(|| anyhow::anyhow!("balance row missing after upsert (impossible)"))?;
    let projected_available = before.on_hand_qty - before.reserved_qty - before.committed_qty - qty;
    if projected_available < 0.0 {
        // Surface the typed error wrapped in anyhow so the saga's
        // `e.downcast::<MaterialInventoryError>()` can route to the
        // right HTTP 409 machine code.
        return Err(anyhow::Error::new(
            MaterialInventoryError::InsufficientMaterial {
                material_grade: material_grade.to_string(),
                requested: qty,
                on_hand: before.on_hand_qty,
                already_reserved: before.reserved_qty,
                already_committed: before.committed_qty,
            },
        ));
    }

    // Step 4 — increment committed_qty.
    let n = tx
        .execute(
            "UPDATE inventory_balances
                SET committed_qty = committed_qty + ?1,
                    last_updated = ?2
              WHERE tenant_id = ?3 AND material_grade = ?4",
            params![qty, &now_iso, tenant, material_grade],
        )
        .context("UPDATE inventory_balances committed_qty")?;
    if n != 1 {
        anyhow::bail!("inventory_balances UPDATE touched {n} rows (expected 1)");
    }

    // Step 5 — insert the reservation row.
    let reservation_id = format!("res_{}", Ulid::new());
    tx.execute(
        "INSERT INTO inventory_reservations (
            reservation_id, tenant_id, quote_id, material_grade,
            qty, state, created_at, transitioned_at, qty_unit_kind
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
        params![
            &reservation_id,
            tenant,
            quote_id,
            material_grade,
            qty,
            ReservationState::Committed.as_db_str(),
            &now_iso,
            qty_unit_kind.as_db_str(),
        ],
    )
    .context("INSERT inventory_reservations")?;

    // Re-read post-state so the outcome carries the invariant-verified
    // snapshot. (The audit payload uses this for forensic walks; the
    // SPA shows it on the DEAL success toast.)
    let after = read_balance_in_tx_inner(tx, tenant, material_grade)
        .context("re-read inventory_balances post-increment")?
        .ok_or_else(|| anyhow::anyhow!("balance row missing post-increment (impossible)"))?;

    // Defence-in-depth: the invariant must hold post-increment too. If
    // it doesn't, something is very wrong (concurrent write that
    // bypassed the tx? schema drift?) — surface the breach loudly per
    // CLAUDE.md rule 12.
    if after.available_qty < 0.0 {
        anyhow::bail!(
            "post-increment invariant breach: material {material_grade} available_qty = {} < 0",
            after.available_qty
        );
    }

    Ok(MaterialCommitOutcome {
        reservation_id,
        material_grade: material_grade.to_string(),
        qty,
        qty_unit_kind,
        balance_after: after,
    })
}

/// JSON payload for the [`EventKind::MaterialCommitted`] entry the DEAL
/// saga emits alongside `QuoteDealIssued` + `QuoteSalesOrderCreated` +
/// `QuoteWorkOrderCreated`. Forensic-walk shape — carries every number
/// a future audit reader needs to reconstruct the saga's material side
/// without re-deriving from sibling entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaterialCommittedPayload {
    pub quote_id: String,
    pub tenant_id: String,
    pub material_grade: String,
    pub qty: f64,
    /// S275 / PR-264 / F1 — what `qty` actually means on this entry.
    /// A forensic walk reading `qty = 12` AND `qty_unit_kind = "units"`
    /// knows NOT to interpret the 12 as kilograms. `#[serde(default)]`
    /// so a payload written before the field existed deserialises to
    /// the legacy `Units` default (the only path that ever wrote
    /// `MaterialCommittedPayload` ran with `qty` as quote units).
    #[serde(default = "default_qty_unit_kind")]
    pub qty_unit_kind: QtyUnitKind,
    pub reservation_id: String,
    pub actor: String,
    pub idempotency_key: String,
    pub created_at: String,
    /// Post-increment snapshot: the saga proves the invariant by
    /// embedding the numbers it just wrote. A divergence between this
    /// payload and a later balance read is a smoking gun for an
    /// out-of-tx write.
    pub balance_after_on_hand: f64,
    pub balance_after_reserved: f64,
    pub balance_after_committed: f64,
    pub balance_after_consumed: f64,
}

impl MaterialCommittedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize MaterialCommittedPayload")
    }
}

/// Convenience for the saga: emit the audit entry inside the same tx
/// the commit + reservation rows wrote. Idempotency key is
/// `quote_deal:<quote_id>:material` so it's distinct from the
/// `:so` / `:wo` siblings.
pub fn append_material_committed_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    payload: &MaterialCommittedPayload,
) -> Result<()> {
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::MaterialCommitted,
        payload.to_bytes(),
        ledger_actor,
        Some(payload.idempotency_key.clone()),
    )
    .context("audit append MaterialCommitted")?;
    Ok(())
}

// ── D-11 — reserve / release / consume state transitions ─────────────────
//
// These complete the shipped material-side balance model: `commit` (above,
// DEAL-driven) was the only live writer, leaving `reserved_qty`,
// `consumed_qty`, and three of the four `inventory.*` audit kinds dead. The
// three transitions here make them live, quantity-conservingly:
//
//   available = on_hand − reserved − committed   (invariant ≥ 0)
//
//   reserve   (∅ → reserved)          reserved += qty      (guard available ≥ qty)
//   release   ({reserved,committed} → released)   earmark −= qty
//   consume   ({reserved,committed} → consumed)   on_hand −= qty, earmark −= qty, consumed += qty
//
// A terminal row (`consumed` / `released`) is immutable — a correction is a
// NEW reservation, never an edit (ADR-0069 §Invariant 3). release/consume
// accept EITHER a `reserved` or a `committed` earmark: the shipped model
// treats the two as parallel holds on set-aside material (a soft indicative
// reserve vs a firm DEAL commit), and both are physically drawable-down. That
// is a deliberate, documented liberalisation of ADR-0069's committed-only
// consume — flagged because it diverges from that (product-side) design,
// which this material-side model already departed from at S273.
//
// Every write here rides the SAME `Transaction` the caller opened on the
// shared `aberp_db::Handle` writer (ADR-0099) — no independent opener, so the
// opener census stays flat.

/// One reservation row, resolved by id for a transition.
struct ReservationRow {
    quote_id: String,
    material_grade: String,
    qty: f64,
    state: ReservationState,
    qty_unit_kind: QtyUnitKind,
}

fn read_reservation_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    reservation_id: &str,
) -> Result<Option<ReservationRow>> {
    tx.execute_batch(INVENTORY_BALANCES_SCHEMA_SQL)
        .context("ensure inventory schema for reservation read")?;
    let mut stmt = tx
        .prepare(
            "SELECT quote_id, material_grade, qty, state, qty_unit_kind
               FROM inventory_reservations
              WHERE reservation_id = ?1 AND tenant_id = ?2",
        )
        .context("prepare reservation read")?;
    let mut rows = stmt
        .query(params![reservation_id, tenant])
        .context("query reservation")?;
    let Some(row) = rows.next().context("read reservation row")? else {
        return Ok(None);
    };
    let state_str: String = row.get(3).context("read reservation.state")?;
    let state = ReservationState::from_db_str(&state_str).ok_or_else(|| {
        anyhow::anyhow!("reservation {reservation_id} has unknown state {state_str:?} (corrupt)")
    })?;
    let unit_str: Option<String> = row.get(4).context("read reservation.qty_unit_kind")?;
    let qty_unit_kind = unit_str
        .as_deref()
        .and_then(QtyUnitKind::from_db_str)
        .unwrap_or_else(default_qty_unit_kind);
    Ok(Some(ReservationRow {
        quote_id: row.get(0).context("read reservation.quote_id")?,
        material_grade: row.get(1).context("read reservation.material_grade")?,
        qty: row.get(2).context("read reservation.qty")?,
        state,
        qty_unit_kind,
    }))
}

/// Outcome of a reserve / release / consume transition. `previous_state`
/// is `None` for a fresh reserve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialReservationOutcome {
    pub reservation_id: String,
    /// The quote the reservation is held against — the caller's quote on a
    /// fresh reserve, or the reservation row's own quote on release/consume.
    pub quote_id: String,
    pub material_grade: String,
    pub qty: f64,
    pub qty_unit_kind: QtyUnitKind,
    pub previous_state: Option<ReservationState>,
    pub new_state: ReservationState,
    pub balance_after: Balance,
}

fn now_iso() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format now for inventory writes")
}

/// Reserve `qty` of `material_grade` against `quote_id`: a soft earmark on
/// available stock. Increments `reserved_qty` (guarded so the reserve fits
/// in `available`) and inserts a new `reserved` reservation row. Mirrors
/// [`commit_material_in_tx`] but for the reserved column.
pub fn reserve_material_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    quote_id: &str,
    material_grade: &str,
    qty: f64,
    qty_unit_kind: QtyUnitKind,
) -> Result<MaterialReservationOutcome> {
    tx.execute_batch(INVENTORY_BALANCES_SCHEMA_SQL)
        .context("ensure inventory schema for reserve")?;
    let now = now_iso()?;
    tx.execute(
        "INSERT INTO inventory_balances (
            tenant_id, material_grade,
            on_hand_qty, reserved_qty, committed_qty, consumed_qty,
            unit_of_measure, last_updated
         ) VALUES (?1, ?2, 0, 0, 0, 0, ?3, ?4)
         ON CONFLICT (tenant_id, material_grade) DO NOTHING",
        params![tenant, material_grade, DEFAULT_UOM, &now],
    )
    .context("upsert inventory_balances at zeros (reserve)")?;

    let before = read_balance_in_tx_inner(tx, tenant, material_grade)
        .context("read inventory_balances before reserve")?
        .ok_or_else(|| anyhow::anyhow!("balance row missing after upsert (impossible)"))?;
    if before.on_hand_qty - before.reserved_qty - before.committed_qty - qty < 0.0 {
        return Err(anyhow::Error::new(
            MaterialInventoryError::InsufficientMaterial {
                material_grade: material_grade.to_string(),
                requested: qty,
                on_hand: before.on_hand_qty,
                already_reserved: before.reserved_qty,
                already_committed: before.committed_qty,
            },
        ));
    }

    let n = tx
        .execute(
            "UPDATE inventory_balances
                SET reserved_qty = reserved_qty + ?1, last_updated = ?2
              WHERE tenant_id = ?3 AND material_grade = ?4",
            params![qty, &now, tenant, material_grade],
        )
        .context("UPDATE inventory_balances reserved_qty")?;
    if n != 1 {
        anyhow::bail!("inventory_balances reserve UPDATE touched {n} rows (expected 1)");
    }

    let reservation_id = format!("res_{}", Ulid::new());
    tx.execute(
        "INSERT INTO inventory_reservations (
            reservation_id, tenant_id, quote_id, material_grade,
            qty, state, created_at, transitioned_at, qty_unit_kind
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
        params![
            &reservation_id,
            tenant,
            quote_id,
            material_grade,
            qty,
            ReservationState::Reserved.as_db_str(),
            &now,
            qty_unit_kind.as_db_str(),
        ],
    )
    .context("INSERT inventory_reservations (reserved)")?;

    let after = read_balance_in_tx_inner(tx, tenant, material_grade)
        .context("re-read inventory_balances post-reserve")?
        .ok_or_else(|| anyhow::anyhow!("balance row missing post-reserve (impossible)"))?;
    if after.available_qty < 0.0 {
        anyhow::bail!(
            "post-reserve invariant breach: material {material_grade} available_qty = {} < 0",
            after.available_qty
        );
    }
    Ok(MaterialReservationOutcome {
        reservation_id,
        quote_id: quote_id.to_string(),
        material_grade: material_grade.to_string(),
        qty,
        qty_unit_kind,
        previous_state: None,
        new_state: ReservationState::Reserved,
        balance_after: after,
    })
}

/// Release a `reserved` or `committed` reservation: return its earmark to
/// the available pool and flip the row to `released`. Rejects a terminal
/// (`consumed` / `released`) row.
pub fn release_reservation_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    reservation_id: &str,
) -> Result<MaterialReservationOutcome> {
    let row = read_reservation_in_tx(tx, tenant, reservation_id)?.ok_or_else(|| {
        anyhow::Error::new(MaterialInventoryError::ReservationNotFound {
            reservation_id: reservation_id.to_string(),
        })
    })?;
    let column = match row.state {
        ReservationState::Reserved => "reserved_qty",
        ReservationState::Committed => "committed_qty",
        terminal @ (ReservationState::Consumed | ReservationState::Released) => {
            return Err(anyhow::Error::new(
                MaterialInventoryError::IllegalTransition {
                    reservation_id: reservation_id.to_string(),
                    from: terminal.as_db_str(),
                    attempted: "released",
                },
            ));
        }
    };
    let now = now_iso()?;
    // The earmark column holds at least this row's qty (the reservation put
    // it there); guard against an underflow anyway (defense-in-depth).
    let n = tx
        .execute(
            &format!(
                "UPDATE inventory_balances
                    SET {column} = {column} - ?1, last_updated = ?2
                  WHERE tenant_id = ?3 AND material_grade = ?4 AND {column} >= ?1"
            ),
            params![row.qty, &now, tenant, &row.material_grade],
        )
        .context("UPDATE inventory_balances (release)")?;
    if n != 1 {
        anyhow::bail!(
            "release of {reservation_id} touched {n} rows (expected 1) — earmark underflow?"
        );
    }
    tx.execute(
        "UPDATE inventory_reservations
            SET state = ?1, transitioned_at = ?2
          WHERE reservation_id = ?3 AND tenant_id = ?4",
        params![
            ReservationState::Released.as_db_str(),
            &now,
            reservation_id,
            tenant
        ],
    )
    .context("flip reservation to released")?;

    let after = read_balance_in_tx_inner(tx, tenant, &row.material_grade)
        .context("re-read inventory_balances post-release")?
        .ok_or_else(|| anyhow::anyhow!("balance row missing post-release (impossible)"))?;
    Ok(MaterialReservationOutcome {
        reservation_id: reservation_id.to_string(),
        quote_id: row.quote_id,
        material_grade: row.material_grade,
        qty: row.qty,
        qty_unit_kind: row.qty_unit_kind,
        previous_state: Some(row.state),
        new_state: ReservationState::Released,
        balance_after: after,
    })
}

/// Consume a `reserved` or `committed` reservation: physically draw the
/// material off the shelf. Debits `on_hand_qty` AND the earmark column, and
/// credits `consumed_qty`; flips the row to `consumed`. Rejects a terminal
/// row. `available_qty` is unchanged (the material was already earmarked out
/// of the available pool).
pub fn consume_reservation_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    reservation_id: &str,
) -> Result<MaterialReservationOutcome> {
    let row = read_reservation_in_tx(tx, tenant, reservation_id)?.ok_or_else(|| {
        anyhow::Error::new(MaterialInventoryError::ReservationNotFound {
            reservation_id: reservation_id.to_string(),
        })
    })?;
    let earmark = match row.state {
        ReservationState::Reserved => "reserved_qty",
        ReservationState::Committed => "committed_qty",
        terminal @ (ReservationState::Consumed | ReservationState::Released) => {
            return Err(anyhow::Error::new(
                MaterialInventoryError::IllegalTransition {
                    reservation_id: reservation_id.to_string(),
                    from: terminal.as_db_str(),
                    attempted: "consumed",
                },
            ));
        }
    };
    let now = now_iso()?;
    let n = tx
        .execute(
            &format!(
                "UPDATE inventory_balances
                    SET on_hand_qty = on_hand_qty - ?1,
                        {earmark} = {earmark} - ?1,
                        consumed_qty = consumed_qty + ?1,
                        last_updated = ?2
                  WHERE tenant_id = ?3 AND material_grade = ?4
                    AND on_hand_qty >= ?1 AND {earmark} >= ?1"
            ),
            params![row.qty, &now, tenant, &row.material_grade],
        )
        .context("UPDATE inventory_balances (consume)")?;
    if n != 1 {
        anyhow::bail!(
            "consume of {reservation_id} touched {n} rows (expected 1) — on_hand/earmark underflow?"
        );
    }
    tx.execute(
        "UPDATE inventory_reservations
            SET state = ?1, transitioned_at = ?2
          WHERE reservation_id = ?3 AND tenant_id = ?4",
        params![
            ReservationState::Consumed.as_db_str(),
            &now,
            reservation_id,
            tenant
        ],
    )
    .context("flip reservation to consumed")?;

    let after = read_balance_in_tx_inner(tx, tenant, &row.material_grade)
        .context("re-read inventory_balances post-consume")?
        .ok_or_else(|| anyhow::anyhow!("balance row missing post-consume (impossible)"))?;
    if after.available_qty < 0.0 {
        anyhow::bail!(
            "post-consume invariant breach: material {} available_qty = {} < 0",
            row.material_grade,
            after.available_qty
        );
    }
    Ok(MaterialReservationOutcome {
        reservation_id: reservation_id.to_string(),
        quote_id: row.quote_id,
        material_grade: row.material_grade,
        qty: row.qty,
        qty_unit_kind: row.qty_unit_kind,
        previous_state: Some(row.state),
        new_state: ReservationState::Consumed,
        balance_after: after,
    })
}

/// Shared forensic-walk payload for the reserve / release / consume audit
/// entries — the same post-transition-snapshot shape as
/// [`MaterialCommittedPayload`], plus the FSM edge (`previous_state` →
/// `new_state`) and an optional `reason` (release only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaterialReservationPayload {
    pub reservation_id: String,
    pub tenant_id: String,
    pub quote_id: String,
    pub material_grade: String,
    pub qty: f64,
    #[serde(default = "default_qty_unit_kind")]
    pub qty_unit_kind: QtyUnitKind,
    pub actor: String,
    pub created_at: String,
    /// `null` for a fresh reserve; the prior FSM state for release/consume.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_state: Option<String>,
    pub new_state: String,
    /// Operator-typed release reason (release entries only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub balance_after_on_hand: f64,
    pub balance_after_reserved: f64,
    pub balance_after_committed: f64,
    pub balance_after_consumed: f64,
}

impl MaterialReservationPayload {
    /// Build from a transition outcome. `reason` is set by the release
    /// caller only.
    pub fn from_outcome(
        tenant: &str,
        actor: &str,
        created_at: &str,
        outcome: &MaterialReservationOutcome,
        reason: Option<String>,
    ) -> Self {
        let b = &outcome.balance_after;
        Self {
            reservation_id: outcome.reservation_id.clone(),
            tenant_id: tenant.to_string(),
            quote_id: outcome.quote_id.clone(),
            material_grade: outcome.material_grade.clone(),
            qty: outcome.qty,
            qty_unit_kind: outcome.qty_unit_kind,
            actor: actor.to_string(),
            created_at: created_at.to_string(),
            previous_state: outcome.previous_state.map(|s| s.as_db_str().to_string()),
            new_state: outcome.new_state.as_db_str().to_string(),
            reason,
            balance_after_on_hand: b.on_hand_qty,
            balance_after_reserved: b.reserved_qty,
            balance_after_committed: b.committed_qty,
            balance_after_consumed: b.consumed_qty,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize MaterialReservationPayload")
    }
}

/// Append the `inventory.material_reserved` audit entry inside the caller's
/// tx. Idempotency key `material_reserve:<reservation_id>` (the res id is
/// minted fresh per reserve, so this dedupes a double-fire).
pub fn append_material_reserved_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    payload: &MaterialReservationPayload,
) -> Result<()> {
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::MaterialReserved,
        payload.to_bytes(),
        ledger_actor,
        Some(format!("material_reserve:{}", payload.reservation_id)),
    )
    .context("audit append MaterialReserved")?;
    Ok(())
}

/// Append the `inventory.material_released` audit entry inside the caller's tx.
pub fn append_material_released_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    payload: &MaterialReservationPayload,
) -> Result<()> {
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::MaterialReleased,
        payload.to_bytes(),
        ledger_actor,
        Some(format!("material_release:{}", payload.reservation_id)),
    )
    .context("audit append MaterialReleased")?;
    Ok(())
}

/// Append the `inventory.material_consumed` audit entry inside the caller's tx.
pub fn append_material_consumed_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    payload: &MaterialReservationPayload,
) -> Result<()> {
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::MaterialConsumed,
        payload.to_bytes(),
        ledger_actor,
        Some(format!("material_consume:{}", payload.reservation_id)),
    )
    .context("audit append MaterialConsumed")?;
    Ok(())
}

// ── D-11 — material certificate capture (material.cert_attached) ──────────
//
// A RECORD event (a cert was filed), NOT a state transition (ADR-0074): a
// grade accrues many certs over its life, each attach an append-only
// landmark. URL-only — the bytes are NOT stored here; `cert_url` is a
// reference to where the document is retained (same posture as the MTR
// `file://` reference). A dedicated encrypted cert-blob store is a separate,
// deferred item.

/// The certificate-class discriminator (closed vocab, ADR-0074 payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialCertKind {
    /// Mill certificate — the EN 10204 3.1 / material test report.
    MillCert,
    /// Certificate of Analysis.
    Cofa,
    /// Heat-treatment certificate.
    HeatTreatment,
}

impl MaterialCertKind {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            MaterialCertKind::MillCert => "mill_cert",
            MaterialCertKind::Cofa => "cofa",
            MaterialCertKind::HeatTreatment => "heat_treatment",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "mill_cert" => Some(MaterialCertKind::MillCert),
            "cofa" => Some(MaterialCertKind::Cofa),
            "heat_treatment" => Some(MaterialCertKind::HeatTreatment),
            _ => None,
        }
    }

    pub const ALL: [MaterialCertKind; 3] = [
        MaterialCertKind::MillCert,
        MaterialCertKind::Cofa,
        MaterialCertKind::HeatTreatment,
    ];
}

const MATERIAL_CERTIFICATES_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS material_certificates (
    cert_id              VARCHAR NOT NULL PRIMARY KEY,
    tenant_id            VARCHAR NOT NULL,
    material_grade       VARCHAR NOT NULL,
    cert_kind            VARCHAR NOT NULL,
    cert_url             VARCHAR NOT NULL,
    lot_id               VARCHAR,
    attached_at_utc      VARCHAR NOT NULL,
    attached_by_operator VARCHAR NOT NULL
);
";

/// Validate the cert retention URL: non-empty, length-capped, and one of the
/// accepted schemes. The bytes are never fetched or stored — this only
/// constrains the reference shape (mirrors `validate_mtr_url`'s posture, but
/// admits `https`/`http` too, since a cert may live in an off-box archive).
fn validate_cert_url(url: &str) -> std::result::Result<String, MaterialInventoryError> {
    let u = url.trim();
    if u.is_empty() {
        return Err(MaterialInventoryError::CertUrlInvalid {
            reason: "empty".to_string(),
        });
    }
    if u.len() > 2048 {
        return Err(MaterialInventoryError::CertUrlInvalid {
            reason: format!("too long ({} chars, max 2048)", u.len()),
        });
    }
    if !(u.starts_with("https://") || u.starts_with("http://") || u.starts_with("file://")) {
        return Err(MaterialInventoryError::CertUrlInvalid {
            reason: "must start with https:// / http:// / file://".to_string(),
        });
    }
    Ok(u.to_string())
}

/// One filed material certificate row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialCertRecord {
    pub cert_id: String,
    pub material_grade: String,
    pub cert_kind: MaterialCertKind,
    pub cert_url: String,
    pub lot_id: Option<String>,
    pub attached_at_utc: String,
    pub attached_by_operator: String,
}

/// Attach a certificate to a material grade. Validates the kind + URL, inserts
/// an append-only `material_certificates` row, and returns it. The caller
/// emits [`EventKind::MaterialCertAttached`] via
/// [`append_material_cert_attached_in_tx`] inside the same tx.
#[allow(clippy::too_many_arguments)]
pub fn attach_material_cert_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    material_grade: &str,
    cert_kind: &str,
    cert_url: &str,
    lot_id: Option<&str>,
    operator: &str,
    now: OffsetDateTime,
) -> Result<MaterialCertRecord> {
    tx.execute_batch(MATERIAL_CERTIFICATES_SCHEMA_SQL)
        .context("ensure material_certificates schema")?;
    let kind = MaterialCertKind::from_db_str(cert_kind.trim()).ok_or_else(|| {
        anyhow::Error::new(MaterialInventoryError::CertKindUnknown {
            cert_kind: cert_kind.to_string(),
        })
    })?;
    let url = validate_cert_url(cert_url).map_err(anyhow::Error::new)?;
    let lot = lot_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let cert_id = format!("mcert_{}", Ulid::new());
    let now_iso = now.format(&Rfc3339).context("format now for cert attach")?;
    tx.execute(
        "INSERT INTO material_certificates (
            cert_id, tenant_id, material_grade, cert_kind, cert_url,
            lot_id, attached_at_utc, attached_by_operator
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            &cert_id,
            tenant,
            material_grade.trim(),
            kind.as_db_str(),
            &url,
            &lot,
            &now_iso,
            operator,
        ],
    )
    .context("INSERT material_certificates")?;
    Ok(MaterialCertRecord {
        cert_id,
        material_grade: material_grade.trim().to_string(),
        cert_kind: kind,
        cert_url: url,
        lot_id: lot,
        attached_at_utc: now_iso,
        attached_by_operator: operator.to_string(),
    })
}

/// Read the certificates filed against a material grade, newest first.
pub fn list_material_certs(
    conn: &Connection,
    tenant: &str,
    material_grade: &str,
) -> Result<Vec<MaterialCertRecord>> {
    // A read-only conn just queries (schema is created by a writer); a
    // writable conn ensures the table first (ADR-0098 posture).
    if !aberp_audit_ledger::connection_is_read_only(conn) {
        conn.execute_batch(MATERIAL_CERTIFICATES_SCHEMA_SQL)
            .context("ensure material_certificates schema for read")?;
    }
    let mut stmt = conn
        .prepare(
            "SELECT cert_id, material_grade, cert_kind, cert_url, lot_id,
                    attached_at_utc, attached_by_operator
               FROM material_certificates
              WHERE tenant_id = ?1 AND material_grade = ?2
              ORDER BY attached_at_utc DESC, cert_id DESC",
        )
        .context("prepare list_material_certs")?;
    let rows = stmt
        .query_map(params![tenant, material_grade.trim()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })
        .context("query list_material_certs")?;
    let mut out = Vec::new();
    for row in rows {
        let (cert_id, grade, kind_str, cert_url, lot_id, at, by) =
            row.context("read material_certificates row")?;
        let cert_kind = MaterialCertKind::from_db_str(&kind_str).ok_or_else(|| {
            anyhow::anyhow!("material_certificates.cert_kind {kind_str:?} is not a known kind")
        })?;
        out.push(MaterialCertRecord {
            cert_id,
            material_grade: grade,
            cert_kind,
            cert_url,
            lot_id,
            attached_at_utc: at,
            attached_by_operator: by,
        });
    }
    Ok(out)
}

/// Payload for [`EventKind::MaterialCertAttached`] — matches the ADR-0074
/// pinned shape. `material_id` carries the material grade the cert backs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaterialCertAttachedPayload {
    pub material_id: String,
    pub cert_kind: String,
    pub cert_url: String,
    pub attached_at_ms: i64,
    pub operator_user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lot_id: Option<String>,
}

impl MaterialCertAttachedPayload {
    /// Build from a filed record + the attach instant (epoch-ms).
    pub fn from_record(record: &MaterialCertRecord, now: OffsetDateTime) -> Self {
        Self {
            material_id: record.material_grade.clone(),
            cert_kind: record.cert_kind.as_db_str().to_string(),
            cert_url: record.cert_url.clone(),
            attached_at_ms: (now.unix_timestamp_nanos() / 1_000_000) as i64,
            operator_user_id: record.attached_by_operator.clone(),
            lot_id: record.lot_id.clone(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize MaterialCertAttachedPayload")
    }
}

/// Append the `material.cert_attached` audit entry inside the caller's tx.
/// Idempotency key `material_cert:<cert_id>`.
pub fn append_material_cert_attached_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    cert_id: &str,
    payload: &MaterialCertAttachedPayload,
) -> Result<()> {
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::MaterialCertAttached,
        payload.to_bytes(),
        ledger_actor,
        Some(format!("material_cert:{cert_id}")),
    )
    .context("audit append MaterialCertAttached")?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// S432 (ADR-0085) — heat-lot traceability assignment.
//
// An operator binds a supplier-issued heat lot (+ optional Mill Test Report
// `file://` URL) to a material grade's stock row. This is the chain-of-custody
// anchor the defense/aerospace WO-start gate in `serve.rs` reads.
//
// Vocab note: this fires the EXISTING `material.heat_lot_assigned` EventKind
// (S357 / ADR-0074 shipped the kind never-fired). The S357 doc-comment sketched
// a `lot_id` + `heat_id` payload split; S432's operator surface is one
// supplier-issued `heat_lot_number` string (validated via the same
// `aberp_compliance::lot_heat::LotId` rules), so the emitted payload follows the
// S432 brief shape `{material_id, heat_lot_number, mtr_url, assigned_by,
// assigned_at}`. Divergence flagged in ADR-0085 (same posture as S431's reuse of
// `supplier.export_screened`).
// ─────────────────────────────────────────────────────────────────────────

/// Typed failure modes for [`assign_heat_lot`]. Wrapped in `anyhow` at the
/// write site; the route layer downcasts to map 400 / 404 (same posture as
/// [`MaterialInventoryError`]).
#[derive(Debug, Error, PartialEq)]
pub enum HeatLotError {
    /// `heat_lot_number` failed [`LotId`] validation (empty / too long / bad
    /// char).
    #[error("heat lot number invalid: {0}")]
    InvalidHeatLot(String),
    /// `mtr_url` was non-empty but not a `file://` URL (or too long).
    #[error("MTR url invalid: {0}")]
    InvalidMtrUrl(String),
    /// No `inventory_balances` row for the grade — the operator picked a
    /// material with no stock row yet. Fix path: seed the balance first.
    #[error("material grade {0} has no inventory balance row to assign a heat lot to")]
    MaterialNotFound(String),
}

impl HeatLotError {
    /// Closed-vocab machine code for the route 4xx body.
    pub fn machine_code(&self) -> &'static str {
        match self {
            HeatLotError::InvalidHeatLot(_) => "invalid_heat_lot",
            HeatLotError::InvalidMtrUrl(_) => "invalid_mtr_url",
            HeatLotError::MaterialNotFound(_) => "material_not_found",
        }
    }
}

/// Outcome of a successful heat-lot assignment — the canonical (validated,
/// trimmed) values written to the row, returned so the route layer can build
/// the audit payload + success response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeatLotAssignment {
    pub material_grade: String,
    pub heat_lot_number: String,
    pub mill_test_report_url: Option<String>,
    pub heat_assigned_at_utc: String,
    pub heat_assigned_by_operator: String,
}

/// Bind a heat lot (+ optional MTR `file://` URL) to a material grade's stock
/// row. Validates both inputs ([[hulye-biztos]] — one form, one save), then
/// UPDATEs the four S432 columns. The row MUST already exist (the operator
/// assigns from a row on the stock screen); a missing row is
/// [`HeatLotError::MaterialNotFound`], NOT a silent upsert.
///
/// Audit emission is OUTSIDE this function (route layer, via
/// [`append_heat_lot_events`]) so the DB write half stays unit-testable without
/// a ledger — same split as `avl_vendors::append_vendor_event`.
pub fn assign_heat_lot(
    conn: &Connection,
    tenant: &str,
    material_grade: &str,
    heat_lot_number: &str,
    mtr_url: &str,
    operator_login: &str,
) -> Result<HeatLotAssignment> {
    ensure_schema(conn)?;

    let lot = LotId::new(heat_lot_number.trim())
        .map_err(|e| anyhow::Error::new(HeatLotError::InvalidHeatLot(e.to_string())))?;
    let mtr = validate_mtr_url(mtr_url)
        .map_err(|e| anyhow::Error::new(HeatLotError::InvalidMtrUrl(e.to_string())))?;

    let now_iso = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format now for heat-lot assignment")?;

    let n = conn
        .execute(
            "UPDATE inventory_balances
                SET heat_lot_number = ?1,
                    mill_test_report_url = ?2,
                    heat_assigned_at_utc = ?3,
                    heat_assigned_by_operator = ?4,
                    last_updated = ?3
              WHERE tenant_id = ?5 AND material_grade = ?6",
            params![
                lot.as_str(),
                mtr.as_deref(),
                &now_iso,
                operator_login,
                tenant,
                material_grade,
            ],
        )
        .context("UPDATE inventory_balances heat-lot columns")?;
    if n == 0 {
        return Err(anyhow::Error::new(HeatLotError::MaterialNotFound(
            material_grade.to_string(),
        )));
    }
    if n != 1 {
        anyhow::bail!("heat-lot UPDATE touched {n} rows (expected 1) for {material_grade}");
    }

    Ok(HeatLotAssignment {
        material_grade: material_grade.to_string(),
        heat_lot_number: lot.as_str().to_string(),
        mill_test_report_url: mtr,
        heat_assigned_at_utc: now_iso,
        heat_assigned_by_operator: operator_login.to_string(),
    })
}

/// Append the heat-lot audit trail: one `material.heat_lot_assigned` always,
/// plus one `material.mtr_uploaded` when an MTR URL was recorded. Mirrors
/// `avl_vendors::append_vendor_event` (opens its own `Ledger` after the write
/// conn is dropped — DuckDB rejects a second writer). Returns the count of
/// entries appended so a caller / test can assert the MTR branch fired.
pub fn append_heat_lot_events(
    db: &HandleArc,
    tenant: TenantId,
    binary_hash: BinaryHash,
    assignment: &HeatLotAssignment,
) -> Result<usize> {
    // ADR-0099 — route the heat-lot audit appends through the ONE shared
    // aberp_db::Handle writer instead of an independent Ledger::open on the
    // live tenant DB. This runs in-process under `aberp serve` (the assign-
    // heat-lot handler); an independent opener off a stale chain head self-
    // assigns an already-used seq (the 369→515 fork class) racing a daemon.
    // Both events land in ONE transaction on the shared instance; the
    // WriteGuard drop runs the lockstep mirror sync.
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for heat-lot audit (ADR-0099): {e}"))?;
    aberp_audit_ledger::ensure_schema(&guard)
        .context("ensure audit-ledger schema for heat-lot events")?;
    let tx = guard
        .transaction()
        .context("begin DuckDB transaction for heat-lot events")?;
    let meta = LedgerMeta::new(tenant, binary_hash);
    let operator = assignment.heat_assigned_by_operator.clone();

    let assigned_payload = serde_json::json!({
        "material_id": assignment.material_grade,
        "heat_lot_number": assignment.heat_lot_number,
        "mtr_url": assignment.mill_test_report_url,
        "assigned_by": operator,
        "assigned_at": assignment.heat_assigned_at_utc,
        "operator_user_id": operator,
    });
    append_in_tx(
        &tx,
        &meta,
        EventKind::MaterialHeatLotAssigned,
        serde_json::to_vec(&assigned_payload).expect("serialize heat-lot payload"),
        Actor::from_local_cli(Ulid::new().to_string(), &operator),
        None,
    )
    .map_err(|e| anyhow::anyhow!("append material.heat_lot_assigned: {e}"))?;
    let mut appended = 1;

    if let Some(mtr) = assignment.mill_test_report_url.as_deref() {
        let mtr_payload = serde_json::json!({
            "material_id": assignment.material_grade,
            "heat_lot_number": assignment.heat_lot_number,
            "mtr_url": mtr,
            "operator_user_id": operator,
            "recorded_at": assignment.heat_assigned_at_utc,
        });
        append_in_tx(
            &tx,
            &meta,
            EventKind::MaterialMtrUploaded,
            serde_json::to_vec(&mtr_payload).expect("serialize mtr payload"),
            Actor::from_local_cli(Ulid::new().to_string(), &operator),
            None,
        )
        .map_err(|e| anyhow::anyhow!("append material.mtr_uploaded: {e}"))?;
        appended += 1;
    }

    tx.commit()
        .context("commit DuckDB transaction for heat-lot events")?;
    Ok(appended)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::ensure_schema as audit_ensure_schema;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        audit_ensure_schema(&conn).expect("audit-ledger schema");
        ensure_schema(&conn).expect("inventory schema");
        conn
    }

    fn ledger_meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new("test-tenant").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    #[test]
    fn reservation_state_round_trip_for_every_variant() {
        for v in ReservationState::ALL {
            let s = v.as_db_str();
            let back = ReservationState::from_db_str(s).unwrap_or_else(|| panic!("{s:?}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn reservation_state_rejects_unknown_string() {
        assert!(ReservationState::from_db_str("not_a_real_state").is_none());
        assert!(ReservationState::from_db_str("").is_none());
    }

    #[test]
    fn schema_ensure_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    #[test]
    fn list_balances_for_empty_tenant_is_empty() {
        let conn = open_conn();
        assert!(list_balances_for_tenant(&conn, "test-tenant")
            .unwrap()
            .is_empty());
    }

    // ── D-11 reserve / release / consume state machine ──────────────────

    fn seed_on_hand(conn: &Connection, grade: &str, on_hand: f64) {
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t', ?1, ?2, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
            params![grade, on_hand],
        )
        .unwrap();
    }

    fn balance_of(conn: &Connection, grade: &str) -> Balance {
        read_balance(conn, "t", grade)
            .unwrap()
            .expect("balance row")
    }

    fn count_kind(conn: &Connection, kind: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn machine_code_of(err: &anyhow::Error) -> &'static str {
        err.downcast_ref::<MaterialInventoryError>()
            .expect("typed material error")
            .machine_code()
    }

    #[test]
    fn reserve_increments_reserved_and_drops_available() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "6061-T6", 100.0);
        let tx = conn.transaction().unwrap();
        let out = reserve_material_in_tx(&tx, "t", "q1", "6061-T6", 30.0, QtyUnitKind::Kg).unwrap();
        tx.commit().unwrap();
        assert_eq!(out.new_state, ReservationState::Reserved);
        assert_eq!(out.previous_state, None);
        let b = balance_of(&conn, "6061-T6");
        assert_eq!(b.on_hand_qty, 100.0);
        assert_eq!(b.reserved_qty, 30.0);
        assert_eq!(b.available_qty, 70.0);
    }

    #[test]
    fn reserve_beyond_available_is_insufficient() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 10.0);
        let tx = conn.transaction().unwrap();
        let err = reserve_material_in_tx(&tx, "t", "q", "g", 11.0, QtyUnitKind::Kg).unwrap_err();
        assert_eq!(machine_code_of(&err), "insufficient_material");
    }

    #[test]
    fn release_from_reserved_restores_available() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 100.0);
        let tx = conn.transaction().unwrap();
        let r = reserve_material_in_tx(&tx, "t", "q", "g", 40.0, QtyUnitKind::Kg).unwrap();
        let rel = release_reservation_in_tx(&tx, "t", &r.reservation_id).unwrap();
        tx.commit().unwrap();
        assert_eq!(rel.previous_state, Some(ReservationState::Reserved));
        assert_eq!(rel.new_state, ReservationState::Released);
        let b = balance_of(&conn, "g");
        assert_eq!(b.reserved_qty, 0.0);
        assert_eq!(b.available_qty, 100.0);
    }

    #[test]
    fn release_from_committed_restores_available() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 100.0);
        let tx = conn.transaction().unwrap();
        let c = commit_material_in_tx(&tx, "t", "q", "g", 25.0, QtyUnitKind::Kg).unwrap();
        let rel = release_reservation_in_tx(&tx, "t", &c.reservation_id).unwrap();
        tx.commit().unwrap();
        assert_eq!(rel.previous_state, Some(ReservationState::Committed));
        let b = balance_of(&conn, "g");
        assert_eq!(b.committed_qty, 0.0);
        assert_eq!(b.available_qty, 100.0);
    }

    #[test]
    fn release_of_terminal_row_is_illegal() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 100.0);
        let tx = conn.transaction().unwrap();
        let r = reserve_material_in_tx(&tx, "t", "q", "g", 10.0, QtyUnitKind::Kg).unwrap();
        release_reservation_in_tx(&tx, "t", &r.reservation_id).unwrap();
        let err = release_reservation_in_tx(&tx, "t", &r.reservation_id).unwrap_err();
        assert_eq!(machine_code_of(&err), "illegal_reservation_transition");
    }

    #[test]
    fn release_of_unknown_reservation_is_not_found() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 100.0);
        let tx = conn.transaction().unwrap();
        let err = release_reservation_in_tx(&tx, "t", "res_nope").unwrap_err();
        assert_eq!(machine_code_of(&err), "reservation_not_found");
    }

    #[test]
    fn consume_from_committed_draws_down_on_hand_and_conserves_available() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 100.0);
        let tx = conn.transaction().unwrap();
        let c = commit_material_in_tx(&tx, "t", "q", "g", 20.0, QtyUnitKind::Kg).unwrap();
        let con = consume_reservation_in_tx(&tx, "t", &c.reservation_id).unwrap();
        tx.commit().unwrap();
        assert_eq!(con.previous_state, Some(ReservationState::Committed));
        assert_eq!(con.new_state, ReservationState::Consumed);
        let b = balance_of(&conn, "g");
        assert_eq!(b.on_hand_qty, 80.0);
        assert_eq!(b.committed_qty, 0.0);
        assert_eq!(b.consumed_qty, 20.0);
        // available was 80 (100 − 20 committed) and stays 80 after consume.
        assert_eq!(b.available_qty, 80.0);
    }

    #[test]
    fn consume_from_reserved_draws_down_on_hand() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 50.0);
        let tx = conn.transaction().unwrap();
        let r = reserve_material_in_tx(&tx, "t", "q", "g", 15.0, QtyUnitKind::Kg).unwrap();
        consume_reservation_in_tx(&tx, "t", &r.reservation_id).unwrap();
        tx.commit().unwrap();
        let b = balance_of(&conn, "g");
        assert_eq!(b.on_hand_qty, 35.0);
        assert_eq!(b.reserved_qty, 0.0);
        assert_eq!(b.consumed_qty, 15.0);
        assert_eq!(b.available_qty, 35.0);
    }

    #[test]
    fn consume_of_terminal_row_is_illegal() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 100.0);
        let tx = conn.transaction().unwrap();
        let r = reserve_material_in_tx(&tx, "t", "q", "g", 10.0, QtyUnitKind::Kg).unwrap();
        consume_reservation_in_tx(&tx, "t", &r.reservation_id).unwrap();
        let err = consume_reservation_in_tx(&tx, "t", &r.reservation_id).unwrap_err();
        assert_eq!(machine_code_of(&err), "illegal_reservation_transition");
    }

    #[test]
    fn transitions_emit_their_audit_kinds() {
        let mut conn = open_conn();
        seed_on_hand(&conn, "g", 100.0);
        let meta = ledger_meta();
        let now = now_iso().unwrap();
        let tx = conn.transaction().unwrap();

        let r = reserve_material_in_tx(&tx, "t", "q1", "g", 30.0, QtyUnitKind::Kg).unwrap();
        let rp = MaterialReservationPayload::from_outcome("t", "op", &now, &r, None);
        append_material_reserved_in_tx(
            &tx,
            &meta,
            Actor::from_local_cli(Ulid::new().to_string(), "op"),
            &rp,
        )
        .unwrap();

        let c = commit_material_in_tx(&tx, "t", "q1", "g", 20.0, QtyUnitKind::Kg).unwrap();
        let con = consume_reservation_in_tx(&tx, "t", &c.reservation_id).unwrap();
        let cp = MaterialReservationPayload::from_outcome("t", "op", &now, &con, None);
        append_material_consumed_in_tx(
            &tx,
            &meta,
            Actor::from_local_cli(Ulid::new().to_string(), "op"),
            &cp,
        )
        .unwrap();

        let rel = release_reservation_in_tx(&tx, "t", &r.reservation_id).unwrap();
        let relp = MaterialReservationPayload::from_outcome(
            "t",
            "op",
            &now,
            &rel,
            Some("superseded".to_string()),
        );
        append_material_released_in_tx(
            &tx,
            &meta,
            Actor::from_local_cli(Ulid::new().to_string(), "op"),
            &relp,
        )
        .unwrap();
        tx.commit().unwrap();

        assert_eq!(count_kind(&conn, "inventory.material_reserved"), 1);
        assert_eq!(count_kind(&conn, "inventory.material_consumed"), 1);
        assert_eq!(count_kind(&conn, "inventory.material_released"), 1);
        // The released payload carried the operator reason.
        assert_eq!(relp.reason.as_deref(), Some("superseded"));
        assert_eq!(relp.previous_state.as_deref(), Some("reserved"));
    }

    // ── D-11 material.cert_attached ─────────────────────────────────────

    #[test]
    fn cert_kind_round_trips_every_variant() {
        for v in MaterialCertKind::ALL {
            assert_eq!(MaterialCertKind::from_db_str(v.as_db_str()), Some(v));
        }
        assert!(MaterialCertKind::from_db_str("bogus").is_none());
    }

    #[test]
    fn attach_cert_inserts_and_reads_back_newest_first() {
        let mut conn = open_conn();
        let now = OffsetDateTime::now_utc();
        let tx = conn.transaction().unwrap();
        let a = attach_material_cert_in_tx(
            &tx,
            "t",
            "6061-T6",
            "mill_cert",
            "file:///certs/mtr-heat-9921.pdf",
            Some("LOT-2026-0042"),
            "op-1",
            now,
        )
        .unwrap();
        let b = attach_material_cert_in_tx(
            &tx,
            "t",
            "6061-T6",
            "cofa",
            "https://certs.example/coa/abc.pdf",
            None,
            "op-1",
            now + time::Duration::seconds(1),
        )
        .unwrap();
        tx.commit().unwrap();

        assert_eq!(a.cert_kind, MaterialCertKind::MillCert);
        assert_eq!(a.lot_id.as_deref(), Some("LOT-2026-0042"));
        assert_eq!(b.lot_id, None);
        assert!(a.cert_id.starts_with("mcert_"));

        let certs = list_material_certs(&conn, "t", "6061-T6").unwrap();
        assert_eq!(certs.len(), 2);
        // Newest first: the CoA (later timestamp) leads.
        assert_eq!(certs[0].cert_id, b.cert_id);
        assert_eq!(certs[1].cert_id, a.cert_id);
        // Grade-scoped: another grade sees none.
        assert!(list_material_certs(&conn, "t", "316").unwrap().is_empty());
    }

    #[test]
    fn attach_cert_rejects_unknown_kind() {
        let mut conn = open_conn();
        let tx = conn.transaction().unwrap();
        let err = attach_material_cert_in_tx(
            &tx,
            "t",
            "g",
            "not_a_cert",
            "https://x/y.pdf",
            None,
            "op",
            OffsetDateTime::now_utc(),
        )
        .unwrap_err();
        assert_eq!(machine_code_of(&err), "cert_kind_unknown");
    }

    #[test]
    fn attach_cert_rejects_bad_url() {
        let mut conn = open_conn();
        let tx = conn.transaction().unwrap();
        for bad in ["", "   ", "ftp://x/y", "just-a-path.pdf"] {
            let err = attach_material_cert_in_tx(
                &tx,
                "t",
                "g",
                "mill_cert",
                bad,
                None,
                "op",
                OffsetDateTime::now_utc(),
            )
            .unwrap_err();
            assert_eq!(machine_code_of(&err), "cert_url_invalid", "url={bad:?}");
        }
    }

    #[test]
    fn attach_cert_emits_material_cert_attached_with_pinned_payload_shape() {
        let mut conn = open_conn();
        let meta = ledger_meta();
        let now = OffsetDateTime::now_utc();
        let tx = conn.transaction().unwrap();
        let rec = attach_material_cert_in_tx(
            &tx,
            "t",
            "6061-T6",
            "heat_treatment",
            "https://certs.example/ht/xyz.pdf",
            Some("LOT-7"),
            "op-1",
            now,
        )
        .unwrap();
        let payload = MaterialCertAttachedPayload::from_record(&rec, now);
        append_material_cert_attached_in_tx(
            &tx,
            &meta,
            Actor::from_local_cli(Ulid::new().to_string(), "op-1"),
            &rec.cert_id,
            &payload,
        )
        .unwrap();
        tx.commit().unwrap();

        assert_eq!(count_kind(&conn, "material.cert_attached"), 1);
        // Pinned shape: material_id carries the grade; ms is epoch-ms.
        assert_eq!(payload.material_id, "6061-T6");
        assert_eq!(payload.cert_kind, "heat_treatment");
        assert_eq!(payload.lot_id.as_deref(), Some("LOT-7"));
        assert!(payload.attached_at_ms > 1_600_000_000_000);
        let json: serde_json::Value = serde_json::from_slice(&payload.to_bytes()).unwrap();
        for k in [
            "material_id",
            "cert_kind",
            "cert_url",
            "attached_at_ms",
            "operator_user_id",
            "lot_id",
        ] {
            assert!(json.get(k).is_some(), "payload missing {k}");
        }
    }

    /// Happy path: first DEAL against a material with positive on_hand
    /// upserts a balance row, increments committed_qty, and inserts a
    /// reservation row in `committed` state.
    #[test]
    fn commit_material_happy_path_increments_committed_and_inserts_reservation() {
        let mut conn = open_conn();
        // Seed: 100 kg on hand for 6061-T6.
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t', '6061-T6', 100.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
            [],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        let outcome =
            commit_material_in_tx(&tx, "t", "q1", "6061-T6", 12.0, QtyUnitKind::Units).unwrap();
        tx.commit().unwrap();

        assert_eq!(outcome.material_grade, "6061-T6");
        assert_eq!(outcome.qty, 12.0);
        assert!(outcome.reservation_id.starts_with("res_"));
        assert_eq!(outcome.balance_after.on_hand_qty, 100.0);
        assert_eq!(outcome.balance_after.committed_qty, 12.0);
        assert_eq!(outcome.balance_after.available_qty, 88.0);

        // Confirm the reservation row landed.
        let (state, qty, quote_id): (String, f64, String) = conn
            .query_row(
                "SELECT state, qty, quote_id FROM inventory_reservations
                  WHERE reservation_id = ?1",
                params![&outcome.reservation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "committed");
        assert_eq!(qty, 12.0);
        assert_eq!(quote_id, "q1");
    }

    /// First-time DEAL against a material with NO `inventory_balances`
    /// row should upsert at zeros + then immediately fail on the
    /// validate-available check (since on_hand = 0). The 409 body
    /// surfaces the numbers so the operator's fix path is obvious.
    #[test]
    fn commit_material_against_missing_balance_upserts_then_fails_insufficient() {
        let mut conn = open_conn();
        let tx = conn.transaction().unwrap();
        let err = commit_material_in_tx(&tx, "t", "q1", "InconeL 718", 5.0, QtyUnitKind::Units)
            .unwrap_err();
        // The tx rolls back the upsert when dropped (we don't commit).
        drop(tx);
        let typed = err
            .downcast::<MaterialInventoryError>()
            .expect("typed insufficient error");
        match typed {
            MaterialInventoryError::InsufficientMaterial {
                requested,
                on_hand,
                already_reserved,
                already_committed,
                ..
            } => {
                assert_eq!(requested, 5.0);
                assert_eq!(on_hand, 0.0);
                assert_eq!(already_reserved, 0.0);
                assert_eq!(already_committed, 0.0);
            }
            other => unreachable!("expected InsufficientMaterial, got {other:?}"),
        }
    }

    /// Insufficient when the available pool is too small: 100 on_hand,
    /// 90 already committed, 15 requested → available = 10 < 15.
    #[test]
    fn commit_material_insufficient_when_already_committed_above_capacity() {
        let mut conn = open_conn();
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t', '316', 100.0, 0, 90.0, 0, 'kg', '2026-06-06T00:00:00Z')",
            [],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        let err =
            commit_material_in_tx(&tx, "t", "q1", "316", 15.0, QtyUnitKind::Units).unwrap_err();
        drop(tx);
        let typed = err.downcast::<MaterialInventoryError>().unwrap();
        assert_eq!(typed.machine_code(), "insufficient_material");
        match typed {
            MaterialInventoryError::InsufficientMaterial {
                requested,
                on_hand,
                already_committed,
                ..
            } => {
                assert_eq!(requested, 15.0);
                assert_eq!(on_hand, 100.0);
                assert_eq!(already_committed, 90.0);
            }
            other => unreachable!("expected InsufficientMaterial, got {other:?}"),
        }
    }

    /// Reserved + committed share the available pool: 100 on_hand, 40
    /// reserved, 50 committed → available = 10. Asking for 15 fails;
    /// asking for 10 succeeds. The reserved+committed sum is the
    /// invariant the saga enforces.
    #[test]
    fn commit_material_respects_reserved_plus_committed_capacity() {
        let mut conn = open_conn();
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t', 'Ti-6Al-4V', 100.0, 40.0, 50.0, 0, 'kg', '2026-06-06T00:00:00Z')",
            [],
        )
        .unwrap();

        // 15 fails (available is 10).
        let tx = conn.transaction().unwrap();
        let err = commit_material_in_tx(&tx, "t", "q1", "Ti-6Al-4V", 15.0, QtyUnitKind::Units)
            .unwrap_err();
        drop(tx);
        assert_eq!(
            err.downcast::<MaterialInventoryError>()
                .unwrap()
                .machine_code(),
            "insufficient_material"
        );

        // 10 succeeds — exactly drains the available pool.
        let tx = conn.transaction().unwrap();
        let outcome =
            commit_material_in_tx(&tx, "t", "q1", "Ti-6Al-4V", 10.0, QtyUnitKind::Units).unwrap();
        tx.commit().unwrap();
        assert_eq!(outcome.balance_after.committed_qty, 60.0);
        assert_eq!(outcome.balance_after.available_qty, 0.0);
    }

    /// Two sequential commits against the same balance row roll up
    /// `committed_qty` correctly + leave the invariant intact.
    #[test]
    fn two_commits_against_same_grade_accumulate_committed_qty() {
        let mut conn = open_conn();
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t', '304', 100.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
            [],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        commit_material_in_tx(&tx, "t", "q1", "304", 30.0, QtyUnitKind::Units).unwrap();
        tx.commit().unwrap();
        let tx = conn.transaction().unwrap();
        commit_material_in_tx(&tx, "t", "q2", "304", 20.0, QtyUnitKind::Units).unwrap();
        tx.commit().unwrap();

        let bal = read_balance(&conn, "t", "304").unwrap().unwrap();
        assert_eq!(bal.committed_qty, 50.0);
        assert_eq!(bal.available_qty, 50.0);

        // Two distinct reservation rows landed (one per quote).
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM inventory_reservations
                  WHERE tenant_id='t' AND material_grade='304'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }

    /// Tenant isolation — a balance row for tenant A is not visible
    /// when reading as tenant B, and a commit against B does not
    /// touch A's row.
    #[test]
    fn commit_material_is_tenant_isolated() {
        let mut conn = open_conn();
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t-A', '6061-T6', 100.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
            [],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        commit_material_in_tx(&tx, "t-B", "q1", "6061-T6", 5.0, QtyUnitKind::Units).unwrap_err(); // t-B has 0 on_hand
        drop(tx);

        // t-A's row is untouched.
        let bal = read_balance(&conn, "t-A", "6061-T6").unwrap().unwrap();
        assert_eq!(bal.committed_qty, 0.0);
    }

    /// `list_balances_for_tenant` returns rows in alphabetical
    /// `material_grade` order, scoped to the queried tenant.
    #[test]
    fn list_balances_is_alphabetical_and_tenant_scoped() {
        let conn = open_conn();
        for (tenant, grade) in [
            ("t-A", "Ti-6Al-4V"),
            ("t-A", "6061-T6"),
            ("t-A", "304"),
            ("t-B", "Inconel 718"),
        ] {
            conn.execute(
                "INSERT INTO inventory_balances (
                    tenant_id, material_grade, on_hand_qty, reserved_qty,
                    committed_qty, consumed_qty, unit_of_measure, last_updated
                 ) VALUES (?1, ?2, 10.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
                params![tenant, grade],
            )
            .unwrap();
        }
        let rows = list_balances_for_tenant(&conn, "t-A").unwrap();
        let grades: Vec<&str> = rows.iter().map(|b| b.material_grade.as_str()).collect();
        assert_eq!(grades, vec!["304", "6061-T6", "Ti-6Al-4V"]);
    }

    /// `available_qty` is derived (`on_hand - reserved - committed`)
    /// and reported alongside the raw quantities so the SPA can render
    /// it without a client-side computation. Pinning the math here so
    /// a future refactor that drops the column doesn't silently
    /// regress the SPA red-highlight rule.
    #[test]
    fn balance_available_qty_is_derived_on_hand_minus_reserved_minus_committed() {
        let conn = open_conn();
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t', 'PEEK', 50.0, 7.0, 13.0, 0, 'kg', '2026-06-06T00:00:00Z')",
            [],
        )
        .unwrap();
        let bal = read_balance(&conn, "t", "PEEK").unwrap().unwrap();
        assert_eq!(bal.on_hand_qty, 50.0);
        assert_eq!(bal.reserved_qty, 7.0);
        assert_eq!(bal.committed_qty, 13.0);
        assert_eq!(bal.available_qty, 30.0);
    }

    /// Payload round-trip pin so a future contributor renaming a field
    /// breaks the test rather than silently desyncing forensic walks
    /// from the saga's emit.
    #[test]
    fn material_committed_payload_round_trips() {
        let p = MaterialCommittedPayload {
            quote_id: "q-X".into(),
            tenant_id: "t".into(),
            material_grade: "6061-T6".into(),
            qty: 12.0,
            qty_unit_kind: QtyUnitKind::Units,
            reservation_id: "res_TEST".into(),
            actor: "operator".into(),
            idempotency_key: "quote_deal:q-X:material".into(),
            created_at: "2026-06-06T12:00:00Z".into(),
            balance_after_on_hand: 100.0,
            balance_after_reserved: 0.0,
            balance_after_committed: 12.0,
            balance_after_consumed: 0.0,
        };
        let back: MaterialCommittedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    /// S275 / F1 — a payload written BEFORE `qty_unit_kind` existed
    /// (the legacy S273 shape) deserialises with `QtyUnitKind::Units`
    /// as the default. Forensic walks across the pre-S275 ledger stay
    /// readable; the field is only loud on new entries.
    #[test]
    fn s275_material_committed_payload_deserialises_legacy_shape_to_units() {
        let legacy = r#"{
            "quote_id": "q-X", "tenant_id": "t",
            "material_grade": "6061-T6", "qty": 12.0,
            "reservation_id": "res_T", "actor": "op",
            "idempotency_key": "quote_deal:q-X:material",
            "created_at": "2026-06-06T12:00:00Z",
            "balance_after_on_hand": 100.0,
            "balance_after_reserved": 0.0,
            "balance_after_committed": 12.0,
            "balance_after_consumed": 0.0
        }"#;
        let p: MaterialCommittedPayload = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.qty_unit_kind, QtyUnitKind::Units);
    }

    /// `append_material_committed_in_tx` writes one
    /// `inventory.material_committed` ledger row inside the saga's tx
    /// and the idempotency_key carries through to the audit row.
    #[test]
    fn append_material_committed_writes_one_audit_row() {
        let mut conn = open_conn();
        let meta = ledger_meta();
        let tx = conn.transaction().unwrap();
        let payload = MaterialCommittedPayload {
            quote_id: "q-X".into(),
            tenant_id: "test-tenant".into(),
            material_grade: "6061-T6".into(),
            qty: 5.0,
            qty_unit_kind: QtyUnitKind::Units,
            reservation_id: "res_TEST".into(),
            actor: "operator".into(),
            idempotency_key: "quote_deal:q-X:material".into(),
            created_at: "2026-06-06T12:00:00Z".into(),
            balance_after_on_hand: 100.0,
            balance_after_reserved: 0.0,
            balance_after_committed: 5.0,
            balance_after_consumed: 0.0,
        };
        append_material_committed_in_tx(&tx, &meta, Actor::test_only(), &payload).unwrap();
        tx.commit().unwrap();

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'inventory.material_committed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    // ── S432 (ADR-0085) — heat-lot assignment ───────────────────────────────

    fn seed_balance(conn: &Connection, grade: &str) {
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t', ?1, 100.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
            params![grade],
        )
        .unwrap();
    }

    /// Happy path: assigning a heat lot stamps all four columns + the read-back
    /// balance surfaces them. [[hulye-biztos]] — one validated save.
    #[test]
    fn assign_heat_lot_stamps_columns_and_read_back_shows_them() {
        let conn = open_conn();
        seed_balance(&conn, "Ti-6Al-4V");
        let out = assign_heat_lot(
            &conn,
            "t",
            "Ti-6Al-4V",
            "  HEAT-9F3A  ",
            "file:///certs/h.pdf",
            "op-001",
        )
        .unwrap();
        assert_eq!(out.heat_lot_number, "HEAT-9F3A"); // trimmed
        assert_eq!(
            out.mill_test_report_url.as_deref(),
            Some("file:///certs/h.pdf")
        );
        assert_eq!(out.heat_assigned_by_operator, "op-001");

        let bal = read_balance(&conn, "t", "Ti-6Al-4V").unwrap().unwrap();
        assert_eq!(bal.heat_lot_number.as_deref(), Some("HEAT-9F3A"));
        assert_eq!(
            bal.mill_test_report_url.as_deref(),
            Some("file:///certs/h.pdf")
        );
        assert!(bal.heat_assigned_at_utc.is_some());
        assert_eq!(bal.heat_assigned_by_operator.as_deref(), Some("op-001"));
    }

    /// Empty MTR is allowed (the cert may lag the heat binding) — None stored.
    #[test]
    fn assign_heat_lot_allows_empty_mtr() {
        let conn = open_conn();
        seed_balance(&conn, "304");
        let out = assign_heat_lot(&conn, "t", "304", "LOT-1", "", "op").unwrap();
        assert_eq!(out.mill_test_report_url, None);
    }

    #[test]
    fn assign_heat_lot_rejects_invalid_heat_lot() {
        let conn = open_conn();
        seed_balance(&conn, "304");
        let err = assign_heat_lot(&conn, "t", "304", "BAD LOT!", "", "op").unwrap_err();
        assert_eq!(
            err.downcast::<HeatLotError>().unwrap().machine_code(),
            "invalid_heat_lot"
        );
    }

    #[test]
    fn assign_heat_lot_rejects_non_file_mtr() {
        let conn = open_conn();
        seed_balance(&conn, "304");
        let err = assign_heat_lot(&conn, "t", "304", "LOT-1", "https://x/y.pdf", "op").unwrap_err();
        assert_eq!(
            err.downcast::<HeatLotError>().unwrap().machine_code(),
            "invalid_mtr_url"
        );
    }

    #[test]
    fn assign_heat_lot_missing_material_is_not_found() {
        let conn = open_conn();
        let err = assign_heat_lot(&conn, "t", "NOPE", "LOT-1", "", "op").unwrap_err();
        let typed = err.downcast::<HeatLotError>().unwrap();
        assert_eq!(typed.machine_code(), "material_not_found");
    }

    /// Audit: a no-MTR assignment fires exactly one
    /// `material.heat_lot_assigned`; an assignment WITH an MTR fires that plus
    /// one `material.mtr_uploaded`.
    #[test]
    fn append_heat_lot_events_fires_one_without_mtr_two_with() {
        let dir = std::env::temp_dir()
            .join("aberp-heat-lot-test")
            .join(Ulid::new().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("aberp.duckdb");
        {
            let conn = Connection::open(&db_path).unwrap();
            audit_ensure_schema(&conn).unwrap();
            ensure_schema(&conn).unwrap();
            seed_balance_at(&conn, "t", "Ti-6Al-4V");
        }
        let tenant = TenantId::new("t").unwrap();
        let hash = BinaryHash::from_bytes([0u8; 32]);
        // ADR-0099 — append through a real shared Handle (checkpoint off);
        // drop it before the readback so the assertion connection sees the
        // committed rows via WAL replay.
        let handle = aberp_db::Handle::open(
            &db_path,
            tenant.clone(),
            aberp_db::HandleConfig {
                checkpoint_enabled: false,
                ..Default::default()
            },
        )
        .unwrap();

        // No MTR → 1 event.
        let a1 = HeatLotAssignment {
            material_grade: "Ti-6Al-4V".into(),
            heat_lot_number: "HEAT-1".into(),
            mill_test_report_url: None,
            heat_assigned_at_utc: "2026-06-16T00:00:00Z".into(),
            heat_assigned_by_operator: "op".into(),
        };
        assert_eq!(
            append_heat_lot_events(&handle, tenant.clone(), hash, &a1).unwrap(),
            1
        );

        // With MTR → 2 events.
        let a2 = HeatLotAssignment {
            mill_test_report_url: Some("file:///c.pdf".into()),
            ..a1.clone()
        };
        assert_eq!(
            append_heat_lot_events(&handle, tenant, hash, &a2).unwrap(),
            2
        );
        drop(handle);

        let conn = Connection::open(&db_path).unwrap();
        let assigned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'material.heat_lot_assigned'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mtr: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'material.mtr_uploaded'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(assigned, 2);
        assert_eq!(mtr, 1);
    }

    fn seed_balance_at(conn: &Connection, tenant: &str, grade: &str) {
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES (?1, ?2, 100.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
            params![tenant, grade],
        )
        .unwrap();
    }
}
