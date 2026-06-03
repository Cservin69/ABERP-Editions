//! Inventory repository — the load-bearing surface per ADR-0061 §3.
//!
//! [`record_movement`] is the ONLY write path for `stock_movements`
//! and for `products.stock_qty` / `products.last_movement_at`. The
//! application-layer invariant
//!
//! ```text
//! stock_qty = SUM(qty_delta) FROM stock_movements WHERE product_id = $1
//! ```
//!
//! is enforced in this function: every write reads the post-INSERT
//! SUM from the same transaction and stamps it back onto the cache.
//! Per ADR-0061 §"Adversarial review" the rebuild-from-SUM (not
//! `current_stock_qty + qty_delta`) is the concurrency-safe choice
//! that works across engines without `SELECT FOR UPDATE`.
//!
//! The audit-ledger append rides the SAME transaction; rollback
//! collapses both halves cleanly per ADR-0008 §"Storage".

use anyhow::{anyhow, Context};
use duckdb::{params, Connection, Transaction};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};

use crate::audit::StockMovementRecordedPayload;
use crate::error::InventoryError;
use crate::types::{ActorKind, MovementReason, MovementRefKind, RequiredSign};

/// Ensure the `stock_movements` table + the products cache columns
/// exist. Idempotent — calling against an already-migrated tenant DB
/// is a no-op. Mirrors `products::ensure_schema` /
/// `partners::ensure_schema` — the products module owns the products
/// table itself; this function owns the inventory-specific additions.
pub fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(include_str!("../migrations/V001__inventory.sql"))
        .context("ensure inventory schema")
}

/// One row from `stock_movements` — the queryable shape for the SPA's
/// "Stock movements" tab + the future Work Orders / Dispatch
/// trace-back surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StockMovement {
    /// `mvt_<ULID>`.
    pub movement_id: String,
    /// `prd_<ULID>`.
    pub product_id: String,
    /// Signed delta.
    pub qty_delta: Decimal,
    /// Why this movement happened.
    pub reason: MovementReason,
    /// Trace-back label; `Some(Manual)` for operator-typed.
    pub ref_kind: Option<MovementRefKind>,
    /// Entity id matching `ref_kind`; `None` for Manual.
    pub ref_id: Option<String>,
    /// Operator-visible timestamp of the movement (RFC3339 UTC).
    pub at: String,
    /// Operator attribution string.
    pub operator: String,
    /// F8 idempotency key.
    pub idempotency_key: String,
    /// Optional free-text operator note.
    pub notes: Option<String>,
}

/// Inputs to [`record_movement`]. The caller (route layer / future
/// Work Order Release handler / future Dispatch handler) populates
/// this struct; `record_movement` mints `movement_id` and the `at`
/// stamp internally so the cache invariant cannot be smuggled around.
#[derive(Debug, Clone)]
pub struct RecordMovementInputs {
    pub product_id: String,
    pub qty_delta: Decimal,
    pub reason: MovementReason,
    /// Trace-back kind. Manual + ref_id=None is the operator-typed
    /// pattern; other variants carry the upstream entity id.
    pub ref_kind: MovementRefKind,
    pub ref_id: Option<String>,
    pub notes: Option<String>,
    pub idempotency_key: String,
}

/// Per-tenant context needed by the write path (the same data the
/// `LedgerMeta` carries, plus the tenant string for the
/// `stock_movements.tenant_id` column).
#[derive(Debug)]
pub struct RecordMovementContext<'a> {
    pub tenant: &'a str,
    pub actor: ActorKind,
    pub ledger_meta: &'a LedgerMeta,
    pub ledger_actor: Actor,
}

/// Validate `inputs.qty_delta` against the reason-sign matrix per
/// ADR-0061 §5 BEFORE any DB write. Zero is always refused as
/// structurally meaningless.
pub fn validate_reason_sign(
    reason: MovementReason,
    qty_delta: Decimal,
) -> Result<(), InventoryError> {
    if qty_delta.is_zero() {
        return Err(InventoryError::WrongSignForReason {
            reason: reason.as_str(),
            required: reason.required_sign(),
            got: qty_delta,
        });
    }
    let ok = match reason.required_sign() {
        RequiredSign::Positive => qty_delta.is_sign_positive(),
        RequiredSign::Negative => qty_delta.is_sign_negative(),
        RequiredSign::Any => true,
    };
    if ok {
        Ok(())
    } else {
        Err(InventoryError::WrongSignForReason {
            reason: reason.as_str(),
            required: reason.required_sign(),
            got: qty_delta,
        })
    }
}

/// Append a movement to the ledger, rebuild the cache from
/// `SUM(qty_delta)`, and emit one audit-ledger entry — all in the
/// supplied transaction. The caller `commit()`s.
///
/// Per ADR-0061 §3 the rebuild reads `SUM(qty_delta)` in the same tx
/// (concurrency-safe across engines without `SELECT FOR UPDATE`); per
/// ADR-0061 §5 the reason-sign matrix is enforced at this boundary;
/// per ADR-0061 §4 the audit-ledger entry is `mes.stock_movement_recorded`.
///
/// The function does NOT validate that `ref_id` matches `ref_kind` —
/// the upstream caller is on the hook for that. The only structural
/// expectation is that `ref_kind = Manual` implies `ref_id = None`
/// at the SPA layer; passing `ref_id = Some(_)` together with
/// `Manual` is a programming error the upstream caller surfaces. The
/// repository accepts both shapes faithfully so a future migration /
/// import tool can pass historical Manual rows with operator notes
/// referencing external systems.
pub fn record_movement(
    tx: &Transaction<'_>,
    ctx: &RecordMovementContext<'_>,
    inputs: RecordMovementInputs,
) -> Result<StockMovement, InventoryError> {
    validate_reason_sign(inputs.reason, inputs.qty_delta)?;

    // Idempotency check is application-level — we deliberately do NOT
    // add a UNIQUE INDEX on `idempotency_key` per [[no-sql-specific]].
    // The SELECT-then-INSERT pattern is safe under tx isolation
    // because the entire flow runs inside the caller-owned tx (same
    // posture as the audit-ledger's append_in_tx).
    let existing: Option<String> = tx
        .query_row(
            "SELECT movement_id FROM stock_movements
             WHERE tenant_id = ? AND idempotency_key = ?
             LIMIT 1;",
            params![ctx.tenant, &inputs.idempotency_key],
            |row| row.get::<_, String>(0),
        )
        .ok();
    if let Some(prior) = existing {
        return Err(InventoryError::DuplicateIdempotencyKey(prior));
    }

    // Product-must-exist check. Per [[no-sql-specific]] we do not lean
    // on a FK; the explicit lookup loud-fails with 404 instead of
    // letting an INSERT against an unknown product land in the ledger
    // (which would corrupt the SUM cache rebuild for future writes
    // against the typo'd id).
    let product_exists: bool = tx
        .query_row(
            "SELECT 1 FROM products
             WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL
             LIMIT 1;",
            params![ctx.tenant, &inputs.product_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|_| true)
        .unwrap_or(false);
    if !product_exists {
        return Err(InventoryError::ProductNotFound(inputs.product_id.clone()));
    }

    let movement_id = format!("mvt_{}", Ulid::new());
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| InventoryError::Storage(anyhow!("format at as Rfc3339: {e}")))?;
    let operator = ctx.actor.as_operator_string();

    // 1. Append the ledger row.
    tx.execute(
        "INSERT INTO stock_movements (
            movement_id, tenant_id, product_id, qty_delta, reason,
            ref_kind, ref_id, at_iso8601, operator, idempotency_key, notes
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &movement_id,
            ctx.tenant,
            &inputs.product_id,
            inputs.qty_delta.to_string(),
            inputs.reason.as_str(),
            inputs.ref_kind.as_str(),
            inputs.ref_id.as_deref(),
            &now,
            &operator,
            &inputs.idempotency_key,
            inputs.notes.as_deref(),
        ],
    )
    .map_err(|e| InventoryError::Storage(anyhow!("INSERT into stock_movements: {e}")))?;

    // 2. Rebuild the cache from the SUM inside the same tx.
    let sum_str: String = tx
        .query_row(
            "SELECT CAST(COALESCE(SUM(qty_delta), 0) AS VARCHAR)
             FROM stock_movements
             WHERE tenant_id = ? AND product_id = ?;",
            params![ctx.tenant, &inputs.product_id],
            |row| row.get(0),
        )
        .map_err(|e| InventoryError::Storage(anyhow!("SUM(qty_delta) for cache rebuild: {e}")))?;
    let new_sum = Decimal::from_str(&sum_str)
        .map_err(|e| InventoryError::Storage(anyhow!("parse SUM decimal {sum_str}: {e}")))?;

    let max_at_str: String = tx
        .query_row(
            "SELECT MAX(at_iso8601) FROM stock_movements
             WHERE tenant_id = ? AND product_id = ?;",
            params![ctx.tenant, &inputs.product_id],
            |row| {
                row.get::<_, Option<String>>(0)
                    .map(|o| o.unwrap_or_default())
            },
        )
        .map_err(|e| InventoryError::Storage(anyhow!("MAX(at_iso8601) for cache rebuild: {e}")))?;

    tx.execute(
        "UPDATE products SET
            stock_qty        = ?,
            last_movement_at = ?
         WHERE tenant_id = ? AND id = ?;",
        params![
            new_sum.to_string(),
            &max_at_str,
            ctx.tenant,
            &inputs.product_id
        ],
    )
    .map_err(|e| InventoryError::Storage(anyhow!("UPDATE products cache: {e}")))?;

    // 3. Emit one audit-ledger entry inside the same tx.
    let payload = StockMovementRecordedPayload {
        movement_id: movement_id.clone(),
        product_id: inputs.product_id.clone(),
        qty_delta: inputs.qty_delta,
        reason: inputs.reason,
        ref_kind: Some(inputs.ref_kind),
        ref_id: inputs.ref_id.clone(),
        operator: operator.clone(),
        idempotency_key: inputs.idempotency_key.clone(),
    };
    append_in_tx(
        tx,
        ctx.ledger_meta,
        EventKind::StockMovementRecorded,
        payload.to_bytes(),
        ctx.ledger_actor.clone(),
        Some(inputs.idempotency_key.clone()),
    )
    .map_err(|e| InventoryError::Storage(anyhow!("audit append_in_tx: {e}")))?;

    Ok(StockMovement {
        movement_id,
        product_id: inputs.product_id,
        qty_delta: inputs.qty_delta,
        reason: inputs.reason,
        ref_kind: Some(inputs.ref_kind),
        ref_id: inputs.ref_id,
        at: now,
        operator,
        idempotency_key: inputs.idempotency_key,
        notes: inputs.notes,
    })
}

/// Read the cached `stock_qty` for a product directly from the
/// `products` row. Mirrors the cache by definition — the rebuild
/// invariant ensures this matches `SUM(qty_delta)`.
pub fn current_stock(
    conn: &Connection,
    tenant: &str,
    product_id: &str,
) -> anyhow::Result<Option<Decimal>> {
    let sum: Option<String> = conn
        .query_row(
            "SELECT CAST(stock_qty AS VARCHAR) FROM products
             WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL
             LIMIT 1;",
            params![tenant, product_id],
            |row| row.get::<_, String>(0).map(Some),
        )
        .or_else(|e| match e {
            duckdb::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .context("SELECT products.stock_qty")?;
    match sum {
        Some(s) => Ok(Some(
            Decimal::from_str(&s).context("parse stock_qty decimal")?,
        )),
        None => Ok(None),
    }
}

/// List `stock_movements` for one product, descending by `at` then by
/// `movement_id` (the second key keeps the order deterministic when
/// two movements share an `at` second — RFC3339-stamping is at
/// second resolution today). Pagination uses LIMIT/OFFSET; the route
/// layer caps `limit` per [[trust-code-not-operator]].
pub fn list_movements_for_product(
    conn: &Connection,
    tenant: &str,
    product_id: &str,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<StockMovement>> {
    let mut stmt = conn.prepare(
        "SELECT movement_id, product_id, CAST(qty_delta AS VARCHAR), reason,
                ref_kind, ref_id, at_iso8601, operator, idempotency_key, notes
         FROM stock_movements
         WHERE tenant_id = ? AND product_id = ?
         ORDER BY at_iso8601 DESC, movement_id DESC
         LIMIT ? OFFSET ?;",
    )?;
    let rows = stmt.query_map(
        params![tenant, product_id, limit, offset],
        row_to_stock_movement,
    )?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r??);
    }
    Ok(out)
}

fn row_to_stock_movement(row: &duckdb::Row<'_>) -> duckdb::Result<anyhow::Result<StockMovement>> {
    let movement_id: String = row.get(0)?;
    let product_id: String = row.get(1)?;
    let qty_delta_str: String = row.get(2)?;
    let reason_str: String = row.get(3)?;
    let ref_kind_str: Option<String> = row.get(4)?;
    let ref_id: Option<String> = row.get(5)?;
    let at: String = row.get(6)?;
    let operator: String = row.get(7)?;
    let idempotency_key: String = row.get(8)?;
    let notes: Option<String> = row.get(9)?;

    let parse = || -> anyhow::Result<StockMovement> {
        let qty_delta = Decimal::from_str(&qty_delta_str)
            .with_context(|| format!("parse qty_delta {qty_delta_str:?}"))?;
        let reason = MovementReason::from_storage_str(&reason_str)
            .map_err(|e| anyhow!("{e}: {reason_str:?}"))?;
        let ref_kind = match ref_kind_str {
            Some(s) => {
                Some(MovementRefKind::from_storage_str(&s).map_err(|e| anyhow!("{e}: {s:?}"))?)
            }
            None => None,
        };
        Ok(StockMovement {
            movement_id,
            product_id,
            qty_delta,
            reason,
            ref_kind,
            ref_id,
            at,
            operator,
            idempotency_key,
            notes,
        })
    };
    Ok(parse())
}

/// Inventory cache fields for one product. The products module owns
/// `Product`; this struct is the additive payload the route layer
/// composes onto each product row when surfacing the GET /api/products
/// response per ADR-0061 §6 (Products list gains a Stock column +
/// low-stock chip).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InventoryFields {
    pub stock_qty: Decimal,
    pub min_stock: Decimal,
    pub bin_location: Option<String>,
    pub last_movement_at: Option<String>,
    /// Derived: `stock_qty < min_stock`. The chip in the SPA reads
    /// this rather than re-computing client-side per CLAUDE.md rule 5
    /// (code answers when code can; the SPA does not need to know the
    /// chip rule).
    pub is_low_stock: bool,
}

/// Read inventory cache fields for every active product in the
/// tenant. Returns a map keyed by product id so the route layer can
/// merge with the existing `Product` list response without a second
/// per-row lookup. Excludes soft-deleted rows (matches
/// `products::list_products` posture).
pub fn inventory_fields_for_tenant(
    conn: &Connection,
    tenant: &str,
) -> anyhow::Result<std::collections::HashMap<String, InventoryFields>> {
    let mut stmt = conn.prepare(
        "SELECT id,
                CAST(COALESCE(stock_qty, 0) AS VARCHAR),
                CAST(COALESCE(min_stock, 0) AS VARCHAR),
                bin_location,
                last_movement_at
         FROM products
         WHERE tenant_id = ? AND deleted_at IS NULL;",
    )?;
    let rows = stmt.query_map(params![tenant], |row| {
        let id: String = row.get(0)?;
        let stock_qty_str: String = row.get(1)?;
        let min_stock_str: String = row.get(2)?;
        let bin_location: Option<String> = row.get(3)?;
        let last_movement_at: Option<String> = row.get(4)?;
        Ok((
            id,
            stock_qty_str,
            min_stock_str,
            bin_location,
            last_movement_at,
        ))
    })?;
    let mut out = std::collections::HashMap::new();
    for r in rows {
        let (id, stock_qty_str, min_stock_str, bin_location, last_movement_at) = r?;
        let stock_qty = Decimal::from_str(&stock_qty_str)
            .with_context(|| format!("parse stock_qty {stock_qty_str:?}"))?;
        let min_stock = Decimal::from_str(&min_stock_str)
            .with_context(|| format!("parse min_stock {min_stock_str:?}"))?;
        let is_low_stock = stock_qty < min_stock;
        out.insert(
            id,
            InventoryFields {
                stock_qty,
                min_stock,
                bin_location,
                last_movement_at,
                is_low_stock,
            },
        );
    }
    Ok(out)
}

/// Read inventory cache fields for one product. Returns `None` for
/// missing / soft-deleted rows (matches `products::get_product`).
pub fn inventory_fields_for_product(
    conn: &Connection,
    tenant: &str,
    product_id: &str,
) -> anyhow::Result<Option<InventoryFields>> {
    let row = conn
        .query_row(
            "SELECT CAST(COALESCE(stock_qty, 0) AS VARCHAR),
                    CAST(COALESCE(min_stock, 0) AS VARCHAR),
                    bin_location,
                    last_movement_at
             FROM products
             WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL
             LIMIT 1;",
            params![tenant, product_id],
            |row| {
                let stock_qty_str: String = row.get(0)?;
                let min_stock_str: String = row.get(1)?;
                let bin_location: Option<String> = row.get(2)?;
                let last_movement_at: Option<String> = row.get(3)?;
                Ok((stock_qty_str, min_stock_str, bin_location, last_movement_at))
            },
        )
        .map(Some)
        .or_else(|e| match e {
            duckdb::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .context("SELECT products inventory fields")?;
    match row {
        None => Ok(None),
        Some((stock_qty_str, min_stock_str, bin_location, last_movement_at)) => {
            let stock_qty = Decimal::from_str(&stock_qty_str)
                .with_context(|| format!("parse stock_qty {stock_qty_str:?}"))?;
            let min_stock = Decimal::from_str(&min_stock_str)
                .with_context(|| format!("parse min_stock {min_stock_str:?}"))?;
            let is_low_stock = stock_qty < min_stock;
            Ok(Some(InventoryFields {
                stock_qty,
                min_stock,
                bin_location,
                last_movement_at,
                is_low_stock,
            }))
        }
    }
}

/// One row in the [`low_stock_products`] result. Carries enough
/// product context that the dashboard chip + the products-list
/// red-badge can render without re-joining `products`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LowStockRow {
    pub product_id: String,
    pub name: String,
    pub stock_qty: Decimal,
    pub min_stock: Decimal,
    pub bin_location: Option<String>,
}

/// Per ADR-0061 §3: products where the cached `stock_qty <
/// min_stock`. Excludes soft-deleted rows (a deleted product cannot
/// be "low-stock"); ordered by the deficit (lowest first) so the
/// dashboard surfaces the most-critical items at the top of the
/// click-through list.
///
/// `COALESCE(_, 0)` on both columns so a freshly-created product
/// (whose cache columns are NULL — `products::create_product`
/// pre-dates this module and does not populate the new columns) with
/// `min_stock` later UPDATEd via the form still surfaces here. The
/// invariant `low-stock-iff-cache-below-min` holds with NULLs treated
/// as 0 — there is no semantic difference between "stock is 0" and
/// "no movement has landed yet."
pub fn low_stock_products(conn: &Connection, tenant: &str) -> anyhow::Result<Vec<LowStockRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name,
                CAST(COALESCE(stock_qty, 0) AS VARCHAR),
                CAST(COALESCE(min_stock, 0) AS VARCHAR),
                bin_location
         FROM products
         WHERE tenant_id = ?
           AND deleted_at IS NULL
           AND COALESCE(stock_qty, 0) < COALESCE(min_stock, 0)
         ORDER BY (COALESCE(stock_qty, 0) - COALESCE(min_stock, 0)) ASC, name ASC;",
    )?;
    let rows = stmt.query_map(params![tenant], |row| {
        let id: String = row.get(0)?;
        let name: String = row.get(1)?;
        let stock_qty_str: String = row.get(2)?;
        let min_stock_str: String = row.get(3)?;
        let bin_location: Option<String> = row.get(4)?;
        Ok((id, name, stock_qty_str, min_stock_str, bin_location))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (product_id, name, stock_qty_str, min_stock_str, bin_location) = r?;
        out.push(LowStockRow {
            product_id,
            name,
            stock_qty: Decimal::from_str(&stock_qty_str)
                .with_context(|| format!("parse stock_qty {stock_qty_str:?}"))?,
            min_stock: Decimal::from_str(&min_stock_str)
                .with_context(|| format!("parse min_stock {min_stock_str:?}"))?,
            bin_location,
        });
    }
    Ok(out)
}

/// Walk every product in the tenant and re-compute its `stock_qty`
/// from `SUM(stock_movements.qty_delta)`. Returns the count of rows
/// touched (every product, even if its computed SUM already matched
/// the cache — the write is unconditional so an operator running this
/// after a suspected drift sees a definitive "X products were
/// reconciled" outcome).
///
/// Per ADR-0061 §3 this is the recovery path when the cache and
/// ledger diverge. Idempotent: re-running produces the same final
/// state. Runs in ONE transaction so partial-rebuild crashes leave
/// the cache untouched.
pub fn rebuild_stock_cache_for_tenant(conn: &mut Connection, tenant: &str) -> anyhow::Result<u64> {
    let tx = conn
        .transaction()
        .context("begin rebuild-stock-cache transaction")?;

    // For each (product_id) in this tenant, recompute SUM and MAX(at_iso8601).
    // We iterate the products list (not the ledger's distinct
    // product_ids) so a product with zero movements still gets its
    // cache stamped to 0 / NULL — a defence against partially-
    // recovered DBs where the cache columns might hold stale values.
    let product_ids: Vec<String> = {
        let mut stmt = tx
            .prepare("SELECT id FROM products WHERE tenant_id = ?;")
            .context("prepare products SELECT for rebuild")?;
        let rows = stmt.query_map(params![tenant], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for r in rows {
            ids.push(r?);
        }
        ids
    };

    let mut touched: u64 = 0;
    for pid in &product_ids {
        let sum_str: String = tx
            .query_row(
                "SELECT CAST(COALESCE(SUM(qty_delta), 0) AS VARCHAR)
                 FROM stock_movements
                 WHERE tenant_id = ? AND product_id = ?;",
                params![tenant, pid],
                |row| row.get(0),
            )
            .with_context(|| format!("SUM rebuild for {pid}"))?;
        let max_at: Option<String> = tx
            .query_row(
                "SELECT MAX(at_iso8601) FROM stock_movements
                 WHERE tenant_id = ? AND product_id = ?;",
                params![tenant, pid],
                |row| row.get::<_, Option<String>>(0),
            )
            .with_context(|| format!("MAX(at_iso8601) rebuild for {pid}"))?;
        tx.execute(
            "UPDATE products SET stock_qty = ?, last_movement_at = ?
             WHERE tenant_id = ? AND id = ?;",
            params![sum_str, max_at, tenant, pid],
        )
        .with_context(|| format!("UPDATE rebuild for {pid}"))?;
        touched += 1;
    }

    tx.commit()
        .context("commit rebuild-stock-cache transaction")?;
    Ok(touched)
}
