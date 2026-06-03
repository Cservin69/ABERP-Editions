//! Integration tests for the Work Orders repository — exercise the
//! full create + transition + inventory-coupling + audit-ledger write
//! paths against a fresh in-memory DuckDB so the ADR-0062 invariants
//! are pinned at the call-site shape.
//!
//! Same pattern `aberp-inventory/tests/repository_round_trip.rs`
//! uses; the DB is opened in-process, the products + audit + inventory
//! schemas are mirrored from production, and every write rides the
//! caller-owned transaction.

use rust_decimal::Decimal;
use std::str::FromStr;

use aberp_audit_ledger::{
    ensure_schema as ensure_audit_schema, Actor, BinaryHash, LedgerMeta, TenantId,
};
use aberp_inventory::{
    current_stock, ensure_schema as ensure_inventory_schema, record_movement, ActorKind,
    MovementReason, MovementRefKind, RecordMovementContext, RecordMovementInputs,
};
use aberp_work_orders::{
    create_work_order, ensure_schema as ensure_wo_schema, list_active_bom_for_product,
    list_routing_ops_for_wo, read_work_order, replace_bom_for_product, transition_routing_op,
    transition_work_order, BomLineInput, CreateWorkOrderInputs, RoutingOpAction, RoutingOpInput,
    RoutingOpTransitionInputs, TransitionInputs, WoAction, WoWriteContext, WorkOrderError,
    WorkOrderState,
};
use duckdb::Connection;

const TEST_TENANT: &str = "ten_test_work_orders";

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
    ensure_wo_schema(&conn).unwrap();
    // S233 / PR-229 — the WO Complete handler now gates on QA per
    // ADR-0063 §7 + invariant #6. The gate reads `qa_inspections`
    // directly so the schema must be ensured for any test that
    // exercises Complete.
    aberp_qa::ensure_schema(&conn).unwrap();
    conn
}

fn insert_product(conn: &Connection, id: &str, name: &str) {
    conn.execute(
        "INSERT INTO products (id, tenant_id, name, unit_kind, unit_value, currency,
                               unit_price_minor, created_at, updated_at, deleted_at,
                               stock_qty, min_stock)
         VALUES (?, ?, ?, 'Nav', 'PIECE', 'HUF', 0, '2026-01-01T00:00:00Z',
                 '2026-01-01T00:00:00Z', NULL, 0, 0);",
        duckdb::params![id, TEST_TENANT, name],
    )
    .unwrap();
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(
        TenantId::new(TEST_TENANT).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
}

fn ctx_for<'a>(meta: &'a LedgerMeta, login: &str) -> WoWriteContext<'a> {
    WoWriteContext {
        tenant: TEST_TENANT,
        actor: ActorKind::SpaOperator {
            operator_login: login.to_string(),
        },
        ledger_meta: meta,
        ledger_actor: Actor::from_local_cli("test-session".to_string(), login),
    }
}

fn seed_component_stock(conn: &mut Connection, meta: &LedgerMeta, product_id: &str, qty: &str) {
    let tx = conn.transaction().unwrap();
    let ctx = RecordMovementContext {
        tenant: TEST_TENANT,
        actor: ActorKind::SpaOperator {
            operator_login: "seed".to_string(),
        },
        ledger_meta: meta,
        ledger_actor: Actor::from_local_cli("seed-session".to_string(), "seed"),
    };
    record_movement(
        &tx,
        &ctx,
        RecordMovementInputs {
            product_id: product_id.to_string(),
            qty_delta: Decimal::from_str(qty).unwrap(),
            reason: MovementReason::Receipt,
            ref_kind: MovementRefKind::Manual,
            ref_id: None,
            notes: None,
            idempotency_key: format!("seed-{product_id}"),
        },
    )
    .unwrap();
    tx.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0062 §3 — happy path: Create → Release → Start → Complete
// ─────────────────────────────────────────────────────────────────────

#[test]
fn happy_path_create_release_start_complete_writes_all_expected_movements_and_audit() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar", "Raw bar");
    let meta = meta();
    // Stock the component (10 units of raw bar) so the Release leaves
    // the cache at 10 - (2*5) = 0 (boundary; not negative).
    seed_component_stock(&mut conn, &meta, "prd_bar", "10");

    // Author a BOM: 2 raw bar per widget.
    {
        let tx = conn.transaction().unwrap();
        replace_bom_for_product(
            &tx,
            TEST_TENANT,
            "prd_widget",
            &[BomLineInput {
                component_id: "prd_bar".to_string(),
                qty_per_unit: Decimal::from_str("2").unwrap(),
            }],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // Create the WO (qty=5).
    let tx = conn.transaction().unwrap();
    let ctx = ctx_for(&meta, "ervin");
    let (wo, ops) = create_work_order(
        &tx,
        &ctx,
        CreateWorkOrderInputs {
            wo_number: "WO-001".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("5").unwrap(),
            notes: Some("first WO".to_string()),
            routing_ops: vec![
                RoutingOpInput {
                    op_name: "CNC mill".to_string(),
                    est_time_min: Some(30),
                    est_cost_huf: None,
                },
                RoutingOpInput {
                    op_name: "Deburr".to_string(),
                    est_time_min: Some(10),
                    est_cost_huf: None,
                },
            ],
            idempotency_key: "create-1".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    assert_eq!(wo.state, WorkOrderState::Created);
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0].sequence, 1);
    assert_eq!(ops[1].sequence, 2);

    // Release.
    let tx = conn.transaction().unwrap();
    let outcome = transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Release,
            reason: None,
            source_event_id: None,
            idempotency_key: "transition-release".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(outcome.wo.state, WorkOrderState::Released);
    assert!(outcome.wo.released_at.is_some());

    // Component stock should now be 10 - (2*5) = 0.
    let component_after_release = current_stock(&conn, TEST_TENANT, "prd_bar")
        .unwrap()
        .unwrap();
    assert_eq!(component_after_release, Decimal::from_str("0").unwrap());

    // Start.
    let tx = conn.transaction().unwrap();
    let outcome = transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Start,
            reason: None,
            source_event_id: None,
            idempotency_key: "transition-start".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(outcome.wo.state, WorkOrderState::InProgress);
    assert!(outcome.wo.started_at.is_some());

    // S233 / PR-229 — per-op cascade + Pass each QA inspection before
    // WO Complete. The gate per ADR-0063 §7 + invariant #6 refuses
    // the Complete unless every routing-op has a live Passed
    // qa_inspections row.
    let qa_ctx = aberp_qa::QaWriteContext {
        tenant: TEST_TENANT,
        actor: aberp_inventory::ActorKind::SpaOperator {
            operator_login: "ervin".to_string(),
        },
        ledger_meta: &meta,
        ledger_actor: aberp_audit_ledger::Actor::from_local_cli(
            "test-session".to_string(),
            "ervin",
        ),
    };
    for (i, op) in ops.iter().enumerate() {
        let tx = conn.transaction().unwrap();
        let r = transition_routing_op(
            &tx,
            &ctx_for(&meta, "ervin"),
            &op.routing_op_id,
            RoutingOpTransitionInputs {
                action: RoutingOpAction::Complete,
                source_event_id: None,
                idempotency_key: format!("op-complete-{i}"),
            },
        )
        .unwrap();
        tx.commit().unwrap();
        let tx = conn.transaction().unwrap();
        aberp_qa::decide_qa(
            &tx,
            &qa_ctx,
            &r.qa_inspection_id,
            aberp_qa::DecideQaInputs {
                decision: aberp_qa::QaDecision::Pass,
                reason: None,
                measurement: None,
                source_event_id: None,
                idempotency_key: format!("qa-pass-{i}"),
            },
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // Complete.
    let tx = conn.transaction().unwrap();
    let outcome = transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Complete,
            reason: None,
            source_event_id: None,
            idempotency_key: "transition-complete".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(outcome.wo.state, WorkOrderState::Completed);
    assert!(outcome.wo.completed_at.is_some());

    // Finished good stock should now be +5 (the WoCompletion movement).
    let widget_after_complete = current_stock(&conn, TEST_TENANT, "prd_widget")
        .unwrap()
        .unwrap();
    assert_eq!(widget_after_complete, Decimal::from_str("5").unwrap());

    // Audit-ledger contents: 1 WorkOrderCreated + 1 BomConsumption + 1
    // WorkOrderStateChanged(release) + 1 WorkOrderStateChanged(start)
    // + 1 WoCompletion movement + 1 WorkOrderStateChanged(complete) +
    // (the seed Receipt). 7 mes.* entries + 1 stock_movement_recorded
    // for seed.
    let mut stmt = conn
        .prepare("SELECT kind FROM audit_ledger ORDER BY seq ASC")
        .unwrap();
    let kinds: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let wo_created = kinds
        .iter()
        .filter(|k| *k == "mes.work_order_created")
        .count();
    let wo_changed = kinds
        .iter()
        .filter(|k| *k == "mes.work_order_state_changed")
        .count();
    let stock_movements = kinds
        .iter()
        .filter(|k| *k == "mes.stock_movement_recorded")
        .count();
    assert_eq!(wo_created, 1, "expected 1 WorkOrderCreated entry");
    assert_eq!(
        wo_changed, 3,
        "expected 3 WorkOrderStateChanged entries (release/start/complete)"
    );
    // 1 seed Receipt + 1 Release BomConsumption + 1 Complete WoCompletion = 3.
    assert_eq!(
        stock_movements, 3,
        "expected 3 stock_movement_recorded entries"
    );
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0062 §5 — Cancel from Released does NOT auto-reverse
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cancel_from_released_does_not_auto_reverse_inventory() {
    // ADR-0062 §5 explicitly refuses to auto-reverse from Released
    // because components may have been physically picked. Operator
    // discipline + manual Adjustment movements carries the recovery.
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar", "Raw bar");
    let meta = meta();
    seed_component_stock(&mut conn, &meta, "prd_bar", "10");

    // BOM: 2 bar / widget.
    let tx = conn.transaction().unwrap();
    replace_bom_for_product(
        &tx,
        TEST_TENANT,
        "prd_widget",
        &[BomLineInput {
            component_id: "prd_bar".to_string(),
            qty_per_unit: Decimal::from_str("2").unwrap(),
        }],
    )
    .unwrap();
    tx.commit().unwrap();

    // Create + Release.
    let tx = conn.transaction().unwrap();
    let (wo, _ops) = create_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-002".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("3").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Cut".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-2".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Release,
            reason: None,
            source_event_id: None,
            idempotency_key: "release-2".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let bar_after_release = current_stock(&conn, TEST_TENANT, "prd_bar")
        .unwrap()
        .unwrap();
    // 10 - 6 = 4
    assert_eq!(bar_after_release, Decimal::from_str("4").unwrap());

    // Cancel from Released.
    let tx = conn.transaction().unwrap();
    let outcome = transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Cancel,
            reason: Some("customer pulled out".to_string()),
            source_event_id: None,
            idempotency_key: "cancel-2".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(outcome.wo.state, WorkOrderState::Cancelled);

    // ADR-0062 §5: stock is UNCHANGED — no auto-reverse.
    let bar_after_cancel = current_stock(&conn, TEST_TENANT, "prd_bar")
        .unwrap()
        .unwrap();
    assert_eq!(bar_after_cancel, Decimal::from_str("4").unwrap());
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0062 §5 — Cancel from Created emits no stock movements
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cancel_from_created_emits_no_stock_movements() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar", "Raw bar");
    let meta = meta();
    seed_component_stock(&mut conn, &meta, "prd_bar", "10");

    // BOM authored but never consumed.
    let tx = conn.transaction().unwrap();
    replace_bom_for_product(
        &tx,
        TEST_TENANT,
        "prd_widget",
        &[BomLineInput {
            component_id: "prd_bar".to_string(),
            qty_per_unit: Decimal::from_str("2").unwrap(),
        }],
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let (wo, _ops) = create_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-003".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("3").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Cut".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-3".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Count audit movements BEFORE the cancel.
    let count_before = count_kind(&conn, "mes.stock_movement_recorded");

    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Cancel,
            reason: Some("operator changed mind".to_string()),
            source_event_id: None,
            idempotency_key: "cancel-3".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let count_after = count_kind(&conn, "mes.stock_movement_recorded");
    assert_eq!(
        count_before, count_after,
        "Cancel from Created must NOT emit stock movements"
    );

    // Stock untouched.
    let bar = current_stock(&conn, TEST_TENANT, "prd_bar")
        .unwrap()
        .unwrap();
    assert_eq!(bar, Decimal::from_str("10").unwrap());
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0062 §5 — Release without an active BOM is loud-refused
// ─────────────────────────────────────────────────────────────────────

#[test]
fn release_refuses_loud_when_product_has_no_active_bom() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_no_bom", "No BOM");
    let meta = meta();

    let tx = conn.transaction().unwrap();
    let (wo, _ops) = create_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-004".to_string(),
            product_id: "prd_no_bom".to_string(),
            qty_target: Decimal::from_str("1").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Op".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-4".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let err = transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Release,
            reason: None,
            source_event_id: None,
            idempotency_key: "release-4".to_string(),
        },
    )
    .unwrap_err();
    drop(tx);
    assert!(
        matches!(&err, WorkOrderError::NoActiveBomForProduct(p) if p == "prd_no_bom"),
        "expected NoActiveBomForProduct, got {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0061 §"Adversarial review" #3 + ADR-0062 §5 — insufficient
// component stock allows release but surfaces a warning
// ─────────────────────────────────────────────────────────────────────

#[test]
fn release_with_insufficient_stock_succeeds_with_warning() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar", "Raw bar");
    let meta = meta();
    // Seed only 1 bar; BOM asks for 2 per widget × 3 widgets = 6.
    seed_component_stock(&mut conn, &meta, "prd_bar", "1");

    let tx = conn.transaction().unwrap();
    replace_bom_for_product(
        &tx,
        TEST_TENANT,
        "prd_widget",
        &[BomLineInput {
            component_id: "prd_bar".to_string(),
            qty_per_unit: Decimal::from_str("2").unwrap(),
        }],
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let (wo, _ops) = create_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-005".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("3").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Op".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-5".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let outcome = transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Release,
            reason: None,
            source_event_id: None,
            idempotency_key: "release-5".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Released despite insufficient stock per ADR-0061 negative-stock
    // policy.
    assert_eq!(outcome.wo.state, WorkOrderState::Released);
    // Warning surfaced.
    assert!(
        outcome.warnings.iter().any(|w| w.contains("prd_bar")),
        "expected a warning mentioning prd_bar, got {:?}",
        outcome.warnings
    );
    // Stock is -5 (1 - 6).
    let bar = current_stock(&conn, TEST_TENANT, "prd_bar")
        .unwrap()
        .unwrap();
    assert_eq!(bar, Decimal::from_str("-5").unwrap());
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0062 §3 + invariant 7 — adapter-driven transition preserves
// source_event_id on the audit entry (actor captured, not branched on)
// ─────────────────────────────────────────────────────────────────────

#[test]
fn adapter_driven_transition_preserves_source_event_id_on_audit_entry() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar", "Raw bar");
    let meta = meta();
    seed_component_stock(&mut conn, &meta, "prd_bar", "10");

    let tx = conn.transaction().unwrap();
    replace_bom_for_product(
        &tx,
        TEST_TENANT,
        "prd_widget",
        &[BomLineInput {
            component_id: "prd_bar".to_string(),
            qty_per_unit: Decimal::from_str("1").unwrap(),
        }],
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let (wo, _ops) = create_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-006".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("1").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Op".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-6".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Adapter-style ctx — Adapter actor instead of SpaOperator.
    let adapter_ctx = WoWriteContext {
        tenant: TEST_TENANT,
        actor: ActorKind::Adapter {
            adapter_name: "barcode-scanner-cell-A".to_string(),
        },
        ledger_meta: &meta,
        ledger_actor: Actor::from_local_cli(
            "adapter-session".to_string(),
            "adapter:barcode-scanner-cell-A",
        ),
    };

    // Transition WITH a source_event_id (simulating an upstream
    // adapter event whose ULID we want to record).
    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &adapter_ctx,
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Release,
            reason: None,
            source_event_id: Some("evt_01HADAPTER123".to_string()),
            idempotency_key: "release-6".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Find the WorkOrderStateChanged entry and assert it carries the
    // source_event_id verbatim.
    let mut stmt = conn
        .prepare(
            "SELECT payload FROM audit_ledger
             WHERE kind = 'mes.work_order_state_changed'
             ORDER BY seq ASC;",
        )
        .unwrap();
    let payloads: Vec<Vec<u8>> = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(payloads.len(), 1);
    let v: serde_json::Value = serde_json::from_slice(&payloads[0]).unwrap();
    assert_eq!(v["source_event_id"].as_str(), Some("evt_01HADAPTER123"));
    assert_eq!(v["actor"].as_str(), Some("adapter:barcode-scanner-cell-A"));
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0062 §2 — illegal transitions are refused loud
// ─────────────────────────────────────────────────────────────────────

#[test]
fn illegal_transition_created_to_complete_is_refused_loud() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget");
    let meta = meta();

    let tx = conn.transaction().unwrap();
    let (wo, _ops) = create_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-007".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("1").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Op".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-7".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let err = transition_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Complete,
            reason: None,
            source_event_id: None,
            idempotency_key: "bad-7".to_string(),
        },
    )
    .unwrap_err();
    drop(tx);
    assert!(
        matches!(err, WorkOrderError::IllegalTransition(_)),
        "expected IllegalTransition, got {err:?}"
    );

    // Row state unchanged.
    let wo_after = read_work_order(&conn, TEST_TENANT, &wo.wo_id)
        .unwrap()
        .unwrap();
    assert_eq!(wo_after.state, WorkOrderState::Created);
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0062 §6 — BOM rows soft-retired, never DELETEd
// ─────────────────────────────────────────────────────────────────────

#[test]
fn replace_bom_soft_retires_prior_rows() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar_v1", "Bar v1");
    insert_product(&conn, "prd_bar_v2", "Bar v2");

    let tx = conn.transaction().unwrap();
    replace_bom_for_product(
        &tx,
        TEST_TENANT,
        "prd_widget",
        &[BomLineInput {
            component_id: "prd_bar_v1".to_string(),
            qty_per_unit: Decimal::from_str("2").unwrap(),
        }],
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    replace_bom_for_product(
        &tx,
        TEST_TENANT,
        "prd_widget",
        &[BomLineInput {
            component_id: "prd_bar_v2".to_string(),
            qty_per_unit: Decimal::from_str("3").unwrap(),
        }],
    )
    .unwrap();
    tx.commit().unwrap();

    let active = list_active_bom_for_product(&conn, TEST_TENANT, "prd_widget").unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].component_id, "prd_bar_v2");

    // Total rows (including retired): 2 — none DELETEd per ADR-0062 §6.
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM boms WHERE tenant_id = ? AND product_id = ?;",
            duckdb::params![TEST_TENANT, "prd_widget"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total, 2);
}

// ─────────────────────────────────────────────────────────────────────
// Routing ops are listed in sequence order
// ─────────────────────────────────────────────────────────────────────

#[test]
fn list_routing_ops_returns_sequence_order() {
    let mut conn = setup_db();
    insert_product(&conn, "prd_widget", "Widget");
    let meta = meta();

    let tx = conn.transaction().unwrap();
    let (wo, _ops) = create_work_order(
        &tx,
        &ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-008".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("1").unwrap(),
            notes: None,
            routing_ops: vec![
                RoutingOpInput {
                    op_name: "first".to_string(),
                    est_time_min: None,
                    est_cost_huf: None,
                },
                RoutingOpInput {
                    op_name: "second".to_string(),
                    est_time_min: None,
                    est_cost_huf: None,
                },
                RoutingOpInput {
                    op_name: "third".to_string(),
                    est_time_min: None,
                    est_cost_huf: None,
                },
            ],
            idempotency_key: "create-8".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let ops = list_routing_ops_for_wo(&conn, TEST_TENANT, &wo.wo_id).unwrap();
    assert_eq!(ops.len(), 3);
    assert_eq!(ops[0].op_name, "first");
    assert_eq!(ops[0].sequence, 1);
    assert_eq!(ops[1].op_name, "second");
    assert_eq!(ops[1].sequence, 2);
    assert_eq!(ops[2].op_name, "third");
    assert_eq!(ops[2].sequence, 3);
}

fn count_kind(conn: &Connection, kind: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?;",
        duckdb::params![kind],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}
