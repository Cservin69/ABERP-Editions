//! Integration tests for the inventory repository — exercise the
//! full ledger + cache + audit-ledger write path against a fresh
//! in-memory DuckDB so the invariants ADR-0061 names are pinned at
//! the call-site shape, not just at the unit-test level inside the
//! repository module.
//!
//! Same pattern `aberp-mes/src/audit.rs::ledger_append_end_to_end`
//! uses; the DB is opened in-process, the products schema is hand-
//! created (mirroring what `apps/aberp/src/products.rs::ensure_schema`
//! emits in production), and every write rides the caller-owned
//! transaction.

use rust_decimal::Decimal;
use std::str::FromStr;

use aberp_audit_ledger::{
    ensure_schema as ensure_audit_schema, Actor, BinaryHash, LedgerMeta, TenantId,
};
use aberp_inventory::{
    current_stock, ensure_schema as ensure_inventory_schema, list_movements_for_product,
    low_stock_products, rebuild_stock_cache_for_tenant, record_movement, validate_reason_sign,
    ActorKind, InventoryError, MovementReason, MovementRefKind, RecordMovementContext,
    RecordMovementInputs, RequiredSign,
};
use duckdb::Connection;

const TEST_TENANT: &str = "ten_test_inventory";

// Mirror of `apps/aberp/src/products.rs::PRODUCTS_SCHEMA_SQL`. Keeping
// the body byte-identical means the tests are exercising the same
// products row shape production carries; if the production schema
// evolves, a future PR updates both halves together.
const PRODUCTS_SCHEMA_FOR_TESTS: &str = "
CREATE TABLE IF NOT EXISTS products (
    id               VARCHAR NOT NULL PRIMARY KEY,
    tenant_id        VARCHAR NOT NULL,
    name             VARCHAR NOT NULL,
    unit_kind        VARCHAR NOT NULL CHECK (unit_kind IN ('Nav','Own')),
    unit_value       VARCHAR NOT NULL,
    currency         VARCHAR NOT NULL CHECK (currency IN ('HUF','EUR')),
    unit_price_minor BIGINT  NOT NULL,
    created_at       VARCHAR NOT NULL,
    updated_at       VARCHAR NOT NULL,
    deleted_at       VARCHAR
);
";

fn setup_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(PRODUCTS_SCHEMA_FOR_TESTS).unwrap();
    ensure_inventory_schema(&conn).unwrap();
    ensure_audit_schema(&conn).unwrap();
    conn
}

fn insert_product(conn: &Connection, id: &str, name: &str, min_stock: &str) {
    conn.execute(
        "INSERT INTO products (id, tenant_id, name, unit_kind, unit_value, currency,
                               unit_price_minor, created_at, updated_at, deleted_at,
                               stock_qty, min_stock)
         VALUES (?, ?, ?, 'Nav', 'PIECE', 'HUF', 0, '2026-01-01T00:00:00Z',
                 '2026-01-01T00:00:00Z', NULL, 0, ?);",
        duckdb::params![id, TEST_TENANT, name, min_stock],
    )
    .unwrap();
}

fn ctx_for<'a>(meta: &'a LedgerMeta, login: &str) -> RecordMovementContext<'a> {
    RecordMovementContext {
        tenant: TEST_TENANT,
        actor: ActorKind::SpaOperator {
            operator_login: login.to_string(),
        },
        ledger_meta: meta,
        ledger_actor: Actor::from_local_cli("test-session".to_string(), login),
    }
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(
        TenantId::new(TEST_TENANT).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
}

#[allow(clippy::too_many_arguments)]
fn record(
    conn: &mut Connection,
    meta: &LedgerMeta,
    product_id: &str,
    qty: &str,
    reason: MovementReason,
    ref_kind: MovementRefKind,
    ref_id: Option<&str>,
    idem: &str,
) -> Result<(), InventoryError> {
    let tx = conn.transaction().unwrap();
    let ctx = ctx_for(meta, "ervin");
    record_movement(
        &tx,
        &ctx,
        RecordMovementInputs {
            product_id: product_id.to_string(),
            qty_delta: Decimal::from_str(qty).unwrap(),
            reason,
            ref_kind,
            ref_id: ref_id.map(|s| s.to_string()),
            notes: None,
            idempotency_key: idem.to_string(),
        },
    )?;
    tx.commit().unwrap();
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0061 §3 — round-trip a movement + cache invariant
// ──────────────────────────────────────────────────────────────────────

#[test]
fn record_movement_writes_one_ledger_row_and_updates_cache_in_tx() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget", "0");
    let meta = meta();

    record(
        &mut conn,
        &meta,
        "prd_widget",
        "10.5",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "idem-1",
    )
    .unwrap();

    // Cache reads back the SUM.
    let cached = current_stock(&conn, TEST_TENANT, "prd_widget")
        .unwrap()
        .unwrap();
    assert_eq!(cached, Decimal::from_str("10.500000").unwrap());

    // The ledger has the one row.
    let rows = list_movements_for_product(&conn, TEST_TENANT, "prd_widget", 10, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].qty_delta, Decimal::from_str("10.5").unwrap());
    assert_eq!(rows[0].reason, MovementReason::Receipt);
    assert_eq!(rows[0].ref_kind, Some(MovementRefKind::Manual));
    assert_eq!(rows[0].ref_id, None);

    // The audit-ledger has one mes.stock_movement_recorded entry.
    let mut stmt = conn
        .prepare("SELECT kind, payload FROM audit_ledger ORDER BY seq ASC")
        .unwrap();
    let entries: Vec<(String, Vec<u8>)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "mes.stock_movement_recorded");
    let payload: serde_json::Value = serde_json::from_slice(&entries[0].1).unwrap();
    assert_eq!(payload["product_id"], "prd_widget");
    assert_eq!(payload["reason"], "receipt");
}

#[test]
fn cache_equals_sum_after_many_movements() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_part", "Part", "0");
    let meta = meta();
    // Receipt + WO consume + adjustment = +10 -3 +0.5 = 7.5
    record(
        &mut conn,
        &meta,
        "prd_part",
        "10",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "idem-a",
    )
    .unwrap();
    record(
        &mut conn,
        &meta,
        "prd_part",
        "-3",
        MovementReason::BomConsumption,
        MovementRefKind::WorkOrder,
        Some("wo_test"),
        "idem-b",
    )
    .unwrap();
    record(
        &mut conn,
        &meta,
        "prd_part",
        "0.5",
        MovementReason::Adjustment,
        MovementRefKind::Manual,
        None,
        "idem-c",
    )
    .unwrap();

    let cached = current_stock(&conn, TEST_TENANT, "prd_part")
        .unwrap()
        .unwrap();
    assert_eq!(cached, Decimal::from_str("7.500000").unwrap());

    let rows = list_movements_for_product(&conn, TEST_TENANT, "prd_part", 10, 0).unwrap();
    let sum: Decimal = rows.iter().map(|r| r.qty_delta).sum();
    assert_eq!(sum, Decimal::from_str("7.5").unwrap());
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0061 §5 — reason-sign matrix
// ──────────────────────────────────────────────────────────────────────

#[test]
fn record_movement_refuses_wrong_sign_per_reason() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_bad", "Bad", "0");
    let meta = meta();

    // Receipt requires positive — negative qty is refused.
    let err = record(
        &mut conn,
        &meta,
        "prd_bad",
        "-1",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "idem-1",
    )
    .unwrap_err();
    assert!(matches!(err, InventoryError::WrongSignForReason { .. }));

    // BomConsumption requires negative — positive qty is refused.
    let err = record(
        &mut conn,
        &meta,
        "prd_bad",
        "1",
        MovementReason::BomConsumption,
        MovementRefKind::WorkOrder,
        Some("wo_x"),
        "idem-2",
    )
    .unwrap_err();
    assert!(matches!(err, InventoryError::WrongSignForReason { .. }));

    // Zero qty is refused for every reason — structurally meaningless.
    for r in [
        MovementReason::Receipt,
        MovementReason::Adjustment,
        MovementReason::Scrap,
    ] {
        assert!(validate_reason_sign(r, Decimal::ZERO).is_err());
    }

    // Adjustment accepts any non-zero sign — operator typed intent.
    assert!(
        validate_reason_sign(MovementReason::Adjustment, Decimal::from_str("-5").unwrap()).is_ok()
    );
    assert!(
        validate_reason_sign(MovementReason::Adjustment, Decimal::from_str("5").unwrap()).is_ok()
    );
    assert_eq!(
        MovementReason::Adjustment.required_sign(),
        RequiredSign::Any
    );

    // After all those refused writes, the ledger is empty and the
    // cache is still 0 — the boundary check fires BEFORE any INSERT.
    let rows = list_movements_for_product(&conn, TEST_TENANT, "prd_bad", 10, 0).unwrap();
    assert!(rows.is_empty());
    let cached = current_stock(&conn, TEST_TENANT, "prd_bad")
        .unwrap()
        .unwrap();
    assert_eq!(cached, Decimal::ZERO);
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0061 §3 — virtual low-stock view
// ──────────────────────────────────────────────────────────────────────

#[test]
fn low_stock_products_surfaces_only_below_min() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_a", "Below A", "10");
    insert_product(&conn, "prd_b", "Below B", "20");
    insert_product(&conn, "prd_c", "Not low", "5");
    insert_product(&conn, "prd_d", "Equal to min", "10");
    let meta = meta();

    // a: stock 0, min 10 → below by 10
    // b: stock 5, min 20 → below by 15 (most critical, sorted first)
    // c: stock 7, min 5 → above (not in result)
    // d: stock 10, min 10 → equal (not below; not in result)
    record(
        &mut conn,
        &meta,
        "prd_b",
        "5",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "i-b",
    )
    .unwrap();
    record(
        &mut conn,
        &meta,
        "prd_c",
        "7",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "i-c",
    )
    .unwrap();
    record(
        &mut conn,
        &meta,
        "prd_d",
        "10",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "i-d",
    )
    .unwrap();

    let low = low_stock_products(&conn, TEST_TENANT).unwrap();
    let names: Vec<&str> = low.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["Below B", "Below A"]); // by deficit ASC
    assert_eq!(low[0].stock_qty, Decimal::from_str("5.000000").unwrap());
    assert_eq!(low[0].min_stock, Decimal::from_str("20.000000").unwrap());
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0061 §3 — rebuild-stock-cache recovery path
// ──────────────────────────────────────────────────────────────────────

#[test]
fn rebuild_stock_cache_reproduces_ledger_totals() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_x", "X", "0");
    insert_product(&conn, "prd_y", "Y", "0");
    insert_product(&conn, "prd_empty", "Empty", "0");
    let meta = meta();

    record(
        &mut conn,
        &meta,
        "prd_x",
        "10",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "i-x1",
    )
    .unwrap();
    record(
        &mut conn,
        &meta,
        "prd_x",
        "-3",
        MovementReason::BomConsumption,
        MovementRefKind::WorkOrder,
        Some("wo_test"),
        "i-x2",
    )
    .unwrap();
    record(
        &mut conn,
        &meta,
        "prd_y",
        "100.25",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "i-y1",
    )
    .unwrap();

    // Corrupt the cache so we can verify the rebuild fixes it. Direct
    // UPDATE — bypasses record_movement deliberately to simulate the
    // "operator edit-by-mistake" / "schema migration bug" failure mode
    // ADR-0061 §3 names.
    conn.execute(
        "UPDATE products SET stock_qty = 999999.999999 WHERE id = 'prd_x';",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE products SET stock_qty = -100.0 WHERE id = 'prd_y';",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE products SET stock_qty = 42.0 WHERE id = 'prd_empty';",
        [],
    )
    .unwrap();

    let touched = rebuild_stock_cache_for_tenant(&mut conn, TEST_TENANT).unwrap();
    assert_eq!(touched, 3);

    assert_eq!(
        current_stock(&conn, TEST_TENANT, "prd_x").unwrap().unwrap(),
        Decimal::from_str("7.000000").unwrap()
    );
    assert_eq!(
        current_stock(&conn, TEST_TENANT, "prd_y").unwrap().unwrap(),
        Decimal::from_str("100.250000").unwrap()
    );
    // Product with zero movements: rebuild stamps it to 0, fixing the
    // bogus 42 we wrote above. This is the load-bearing "products
    // without movements still get reconciled" property.
    assert_eq!(
        current_stock(&conn, TEST_TENANT, "prd_empty")
            .unwrap()
            .unwrap(),
        Decimal::ZERO
    );
}

#[test]
fn rebuild_is_idempotent() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_id", "Idempotent", "0");
    let meta = meta();
    record(
        &mut conn,
        &meta,
        "prd_id",
        "5",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "i-1",
    )
    .unwrap();

    let first = rebuild_stock_cache_for_tenant(&mut conn, TEST_TENANT).unwrap();
    let after_first = current_stock(&conn, TEST_TENANT, "prd_id")
        .unwrap()
        .unwrap();
    let second = rebuild_stock_cache_for_tenant(&mut conn, TEST_TENANT).unwrap();
    let after_second = current_stock(&conn, TEST_TENANT, "prd_id")
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(after_first, after_second);
    assert_eq!(after_second, Decimal::from_str("5.000000").unwrap());
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0061 §1 — append-only (no UPDATE/DELETE surface)
// ──────────────────────────────────────────────────────────────────────

#[test]
fn record_movement_refuses_duplicate_idempotency_key() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_dup", "Dup", "0");
    let meta = meta();
    record(
        &mut conn,
        &meta,
        "prd_dup",
        "1",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "shared-key",
    )
    .unwrap();
    let err = record(
        &mut conn,
        &meta,
        "prd_dup",
        "1",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "shared-key",
    )
    .unwrap_err();
    assert!(matches!(err, InventoryError::DuplicateIdempotencyKey(_)));

    // The retry did NOT add a second row.
    let rows = list_movements_for_product(&conn, TEST_TENANT, "prd_dup", 10, 0).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn record_movement_refuses_unknown_product() {
    let mut conn = setup_db();
    let meta = meta();
    let err = record(
        &mut conn,
        &meta,
        "prd_does_not_exist",
        "1",
        MovementReason::Receipt,
        MovementRefKind::Manual,
        None,
        "i-1",
    )
    .unwrap_err();
    assert!(matches!(err, InventoryError::ProductNotFound(_)));
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0061 §"Adversarial review" #3 — Adjustment can drive negative
// ──────────────────────────────────────────────────────────────────────

#[test]
fn adjustment_can_drive_stock_negative_with_explicit_operator_intent() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_neg", "Negative-able", "0");
    let meta = meta();

    // Stock starts at 0. Adjustment writes -5; cache should reflect
    // -5. This is the ADR-0061 §5 "the only path that can drive
    // stock_qty negative" pin — the chip surfaces it in the SPA.
    record(
        &mut conn,
        &meta,
        "prd_neg",
        "-5",
        MovementReason::Adjustment,
        MovementRefKind::Manual,
        None,
        "i-neg",
    )
    .unwrap();
    let cached = current_stock(&conn, TEST_TENANT, "prd_neg")
        .unwrap()
        .unwrap();
    assert_eq!(cached, Decimal::from_str("-5.000000").unwrap());
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0061 §"Cross-cutting decisions" #1 — actor-agnostic handler
// ──────────────────────────────────────────────────────────────────────

#[test]
fn record_movement_records_actor_attribution() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_actor", "Actor", "0");
    let meta = meta();
    let tx = conn.transaction().unwrap();
    // Pretend an adapter (future barcode scanner) triggered the
    // Receipt rather than the SPA. Same record_movement signature
    // either way — the audit-ledger entry just records a different
    // operator string.
    let ctx = RecordMovementContext {
        tenant: TEST_TENANT,
        actor: ActorKind::Adapter {
            adapter_name: "barcode-scanner-cell-A".to_string(),
        },
        ledger_meta: &meta,
        ledger_actor: Actor::from_local_cli(
            "test-session".to_string(),
            "adapter:barcode-scanner-cell-A",
        ),
    };
    record_movement(
        &tx,
        &ctx,
        RecordMovementInputs {
            product_id: "prd_actor".to_string(),
            qty_delta: Decimal::from_str("3").unwrap(),
            reason: MovementReason::Receipt,
            ref_kind: MovementRefKind::Manual,
            ref_id: None,
            notes: None,
            idempotency_key: "i-actor".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let rows = list_movements_for_product(&conn, TEST_TENANT, "prd_actor", 10, 0).unwrap();
    assert_eq!(rows[0].operator, "adapter:barcode-scanner-cell-A");
}
