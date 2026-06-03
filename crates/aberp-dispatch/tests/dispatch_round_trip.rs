//! Integration tests for the Dispatch-board repository — exercise the
//! full create_dispatch + mark_shipped + cancel + invoice-spawn paths
//! against a fresh in-memory DuckDB so ADR-0064's invariants are pinned
//! at the call-site shape. Mirrors the
//! `aberp-qa/tests/qa_round_trip.rs` / `aberp-work-orders/tests/work_order_round_trip.rs`
//! posture.
//!
//! The tests use a [`MockInvoiceSpawner`] (declared inline at the
//! bottom of this file) to exercise the three failure modes of the
//! injected spawner per ADR-0064 §5 invariant #6:
//!   - returns `Ok(None)` — v1 production noop posture (default)
//!   - returns `Ok(Some(invoice_id))` — pins invariants #4 + #5
//!   - returns `Err(_)` — pins invariant #6 (failed spawn rolls back
//!     the entire `mark_shipped` transaction)

use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Mutex;

use aberp_audit_ledger::{
    ensure_schema as ensure_audit_schema, Actor, BinaryHash, LedgerMeta, TenantId,
};
use aberp_dispatch::{
    cancel_dispatch, create_dispatch, ensure_schema as ensure_dispatch_schema, get_dispatch,
    list_dispatches, list_eligible_work_orders, mark_shipped, CarrierKind, CreateDispatchInputs,
    Dispatch, DispatchError, DispatchState, DispatchWriteContext, InvoiceSpawner,
    MarkShippedInputs, NoopInvoiceSpawner,
};
use aberp_inventory::{
    current_stock, ensure_schema as ensure_inventory_schema, record_movement, ActorKind,
    MovementReason, MovementRefKind, RecordMovementContext, RecordMovementInputs,
};
use aberp_work_orders::{
    create_work_order, ensure_schema as ensure_wo_schema, replace_bom_for_product,
    transition_routing_op, transition_work_order, BomLineInput, CreateWorkOrderInputs,
    RoutingOpAction, RoutingOpInput, RoutingOpTransitionInputs, TransitionInputs, WoAction,
    WoWriteContext, WorkOrderState,
};
use duckdb::{Connection, Transaction};

const TEST_TENANT: &str = "ten_test_dispatch";

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

const PARTNERS_SCHEMA_FOR_TESTS: &str = "
CREATE TABLE IF NOT EXISTS partners (
    id                    VARCHAR NOT NULL PRIMARY KEY,
    tenant_id             VARCHAR NOT NULL,
    display_name          VARCHAR NOT NULL,
    legal_name            VARCHAR NOT NULL,
    kind                  VARCHAR NOT NULL,
    tax_number            VARCHAR,
    eu_vat_number         VARCHAR,
    address_street        VARCHAR,
    address_postal_code   VARCHAR,
    address_city          VARCHAR,
    address_country       VARCHAR,
    bank_account          VARCHAR,
    contact_email         VARCHAR,
    contact_phone         VARCHAR,
    customer_vat_status   VARCHAR NOT NULL DEFAULT 'Domestic',
    issued_invoice_count  BIGINT  NOT NULL DEFAULT 0,
    created_at            VARCHAR NOT NULL,
    updated_at            VARCHAR NOT NULL,
    deleted_at            VARCHAR
);
";

const TEST_INVOICE_DRAFTS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS test_invoice_drafts (
    invoice_id   VARCHAR NOT NULL PRIMARY KEY,
    tenant_id    VARCHAR NOT NULL,
    partner_id   VARCHAR NOT NULL,
    product_id   VARCHAR NOT NULL,
    qty          VARCHAR NOT NULL,
    is_issued    BOOLEAN NOT NULL,
    nav_status   VARCHAR NOT NULL,
    created_at   VARCHAR NOT NULL
);
";

fn setup_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(PRODUCTS_SCHEMA_FOR_TESTS).unwrap();
    conn.execute_batch(PARTNERS_SCHEMA_FOR_TESTS).unwrap();
    conn.execute_batch(TEST_INVOICE_DRAFTS_SCHEMA).unwrap();
    ensure_inventory_schema(&conn).unwrap();
    ensure_audit_schema(&conn).unwrap();
    ensure_wo_schema(&conn).unwrap();
    aberp_qa::ensure_schema(&conn).unwrap();
    ensure_dispatch_schema(&conn).unwrap();
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

fn insert_partner(conn: &Connection, id: &str, name: &str) {
    conn.execute(
        "INSERT INTO partners (id, tenant_id, display_name, legal_name, kind, tax_number,
                               created_at, updated_at, deleted_at)
         VALUES (?, ?, ?, ?, 'Customer', '12345678-2-13',
                 '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL);",
        duckdb::params![id, TEST_TENANT, name, name],
    )
    .unwrap();
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(
        TenantId::new(TEST_TENANT).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
}

fn wo_ctx_for<'a>(meta: &'a LedgerMeta, login: &str) -> WoWriteContext<'a> {
    WoWriteContext {
        tenant: TEST_TENANT,
        actor: ActorKind::SpaOperator {
            operator_login: login.to_string(),
        },
        ledger_meta: meta,
        ledger_actor: Actor::from_local_cli("test-session".to_string(), login),
    }
}

fn dispatch_ctx_for<'a>(meta: &'a LedgerMeta, login: &str) -> DispatchWriteContext<'a> {
    DispatchWriteContext {
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

/// Common setup: create products + 1 partner + BOM + a 1-op WO + drive
/// it all the way to Completed (Release → complete op → Complete). The
/// dispatch tests start from this "Completed WO" state.
fn create_completed_wo(conn: &mut Connection, meta: &LedgerMeta, wo_number: &str) -> String {
    // First-time setup only (products + partner). Re-runs skip via
    // primary-key conflict — the test that calls us twice can pre-seed
    // a different WO number; the products + partner stay shared.
    let already_set_up: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM products WHERE id = 'prd_widget'",
            duckdb::params![],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if already_set_up == 0 {
        insert_product(conn, "prd_widget", "Widget");
        insert_product(conn, "prd_bar", "Raw bar");
        insert_partner(conn, "ptr_acme", "ACME Kft.");
        seed_component_stock(conn, meta, "prd_bar", "100");

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
    }

    // Create the WO.
    let tx = conn.transaction().unwrap();
    let (wo, ops) = create_work_order(
        &tx,
        &wo_ctx_for(meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: wo_number.to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("5").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Assemble".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: format!("create-{wo_number}"),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let op_id = ops[0].routing_op_id.clone();

    // Release (cascades op#1 to Active + consumes BOM).
    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &wo_ctx_for(meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Release,
            reason: None,
            source_event_id: None,
            idempotency_key: format!("release-{wo_number}"),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Start the WO (Released → InProgress).
    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &wo_ctx_for(meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Start,
            reason: None,
            source_event_id: None,
            idempotency_key: format!("start-{wo_number}"),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Complete op#1 → QA auto-created.
    let tx = conn.transaction().unwrap();
    let op_outcome = transition_routing_op(
        &tx,
        &wo_ctx_for(meta, "ervin"),
        &op_id,
        RoutingOpTransitionInputs {
            action: RoutingOpAction::Complete,
            source_event_id: None,
            idempotency_key: format!("op1-complete-{wo_number}"),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Decide Pass on the QA inspection.
    let tx = conn.transaction().unwrap();
    aberp_qa::decide_qa(
        &tx,
        &aberp_qa::QaWriteContext {
            tenant: TEST_TENANT,
            actor: ActorKind::SpaOperator {
                operator_login: "ervin".to_string(),
            },
            ledger_meta: meta,
            ledger_actor: Actor::from_local_cli("test-session".to_string(), "ervin"),
        },
        &op_outcome.qa_inspection_id,
        aberp_qa::DecideQaInputs {
            decision: aberp_qa::QaDecision::Pass,
            reason: None,
            measurement: None,
            source_event_id: None,
            idempotency_key: format!("decide-pass-{wo_number}"),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Complete the WO (gates on all-QA-pass).
    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &wo_ctx_for(meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Complete,
            reason: None,
            source_event_id: None,
            idempotency_key: format!("complete-{wo_number}"),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Sanity: WO is Completed.
    let wo_state: String = conn
        .query_row(
            "SELECT state FROM work_orders WHERE wo_id = ?",
            duckdb::params![wo.wo_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(wo_state, "completed");
    wo.wo_id
}

fn count_kind(conn: &Connection, kind: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?;",
        duckdb::params![kind],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

// ── MockInvoiceSpawner ─────────────────────────────────────────────

/// Test-only [`InvoiceSpawner`] with configurable return:
///   - `Behaviour::ReturnNone` → returns `Ok(None)` (mirrors
///     [`NoopInvoiceSpawner`] but counts calls for assertions)
///   - `Behaviour::ReturnSome(invoice_id)` → returns `Ok(Some(_))` AND
///     writes a row into the test_invoice_drafts table inside the
///     supplied tx (pins invariants #4 + #5)
///   - `Behaviour::ReturnErr(msg)` → returns `Err(_)` (pins invariant
///     #6 — failed spawn rolls back the entire mark_shipped tx)
enum Behaviour {
    YieldsSome(String),
    YieldsErr(String),
}

struct MockInvoiceSpawner {
    behaviour: Behaviour,
    call_count: Mutex<u32>,
}

impl MockInvoiceSpawner {
    fn return_some(invoice_id: &str) -> Self {
        Self {
            behaviour: Behaviour::YieldsSome(invoice_id.to_string()),
            call_count: Mutex::new(0),
        }
    }
    fn return_err(msg: &str) -> Self {
        Self {
            behaviour: Behaviour::YieldsErr(msg.to_string()),
            call_count: Mutex::new(0),
        }
    }
    fn calls(&self) -> u32 {
        *self.call_count.lock().unwrap()
    }
}

impl InvoiceSpawner for MockInvoiceSpawner {
    fn spawn(
        &self,
        tx: &Transaction<'_>,
        dispatch: &Dispatch,
        wo_product_id: &str,
        wo_qty_target: Decimal,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<String>> {
        *self.call_count.lock().unwrap() += 1;
        match &self.behaviour {
            Behaviour::YieldsSome(invoice_id) => {
                // Pin invariant #4: the spawned invoice is marked DRAFTED
                // (`is_issued = false` + `nav_status = NONE`). Pin
                // invariant #5: the spawned_invoice_id matches what we
                // return.
                tx.execute(
                    "INSERT INTO test_invoice_drafts (
                        invoice_id, tenant_id, partner_id, product_id, qty,
                        is_issued, nav_status, created_at
                     ) VALUES (?, ?, ?, ?, ?, FALSE, 'NONE', '2026-06-03T10:00:00Z');",
                    duckdb::params![
                        invoice_id,
                        TEST_TENANT,
                        &dispatch.partner_id,
                        wo_product_id,
                        wo_qty_target.to_string(),
                    ],
                )?;
                // Also persist a tiny note tying the idempotency_key to
                // the call so the post-tx assertion can verify the
                // key shape per the F8 contract (`<root>:spawn_invoice`).
                assert!(idempotency_key.ends_with(":spawn_invoice"));
                Ok(Some(invoice_id.clone()))
            }
            Behaviour::YieldsErr(msg) => Err(anyhow::anyhow!("{msg}")),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Invariant: create_dispatch refuses ineligible WO (not Completed)
// ─────────────────────────────────────────────────────────────────────

#[test]
fn create_dispatch_refuses_ineligible_wo() {
    let mut conn = setup_db();
    let meta = meta();
    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar", "Raw bar");
    insert_partner(&conn, "ptr_acme", "ACME Kft.");
    seed_component_stock(&mut conn, &meta, "prd_bar", "100");

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
    let (wo, _) = create_work_order(
        &tx,
        &wo_ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-INELIG-001".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("5").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Assemble".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-inelig".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Try to create a dispatch — WO is in `Created`, not `Completed`.
    let tx = conn.transaction().unwrap();
    let result = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id: wo.wo_id.clone(),
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-inelig".to_string(),
        },
    );
    match result {
        Err(DispatchError::WorkOrderNotEligible { state, .. }) => {
            assert_eq!(state, "created");
        }
        other => panic!("expected WorkOrderNotEligible, got {other:?}"),
    }
    // Tx never committed; no audit entry.
    drop(tx);
    assert_eq!(count_kind(&conn, "mes.dispatch_created"), 0);
}

// ─────────────────────────────────────────────────────────────────────
// Invariant: create_dispatch refuses duplicate dispatch for same WO
// ─────────────────────────────────────────────────────────────────────

#[test]
fn create_dispatch_refuses_duplicate_for_wo() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-DUP-001");

    // First create succeeds.
    let tx = conn.transaction().unwrap();
    let dsp = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id: wo_id.clone(),
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-dup-1".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let first_dsp_id = dsp.dsp_id.clone();

    // Second create against the same WO → refused with the existing id.
    let tx = conn.transaction().unwrap();
    let result = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id: wo_id.clone(),
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-dup-2".to_string(),
        },
    );
    match result {
        Err(DispatchError::WorkOrderAlreadyDispatched { dsp_id, .. }) => {
            assert_eq!(dsp_id, first_dsp_id);
        }
        other => panic!("expected WorkOrderAlreadyDispatched, got {other:?}"),
    }
    drop(tx);
    // Only one DispatchCreated audit entry.
    assert_eq!(count_kind(&conn, "mes.dispatch_created"), 1);
}

// ─────────────────────────────────────────────────────────────────────
// Invariant #1: mark_shipped writes the state flip + the Dispatch
// stock_movement + the DispatchShipped audit entry in the SAME tx.
// (With NoopInvoiceSpawner; spawned_invoice_id stays None.)
// ─────────────────────────────────────────────────────────────────────

#[test]
fn mark_shipped_writes_movement_and_audit_in_same_tx_with_noop_spawner() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-SHIP-001");

    // Stock before ship: BOM consume on Release was -5 (qty_target=5,
    // BOM 1 bar/unit), WoCompletion on Complete was +5 widget. So bar
    // ends at 95, widget at 5.
    let bar_before = current_stock(&conn, TEST_TENANT, "prd_bar")
        .unwrap()
        .unwrap();
    let widget_before = current_stock(&conn, TEST_TENANT, "prd_widget")
        .unwrap()
        .unwrap();
    assert_eq!(bar_before, Decimal::from_str("95").unwrap());
    assert_eq!(widget_before, Decimal::from_str("5").unwrap());

    // Create + ship.
    let tx = conn.transaction().unwrap();
    let dsp = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id,
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-ship".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let outcome = mark_shipped(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        &dsp.dsp_id,
        MarkShippedInputs {
            carrier_kind: CarrierKind::MagyarPosta,
            tracking_number: Some("MPL-XYZ-999".to_string()),
            shipped_at: None,
            idempotency_key: "ship-dsp-1".to_string(),
        },
        &NoopInvoiceSpawner,
    )
    .unwrap();
    tx.commit().unwrap();

    // Dispatch state flipped to Shipped.
    assert_eq!(outcome.dispatch.state, DispatchState::Shipped);
    assert_eq!(
        outcome.dispatch.carrier_kind,
        Some(CarrierKind::MagyarPosta)
    );
    assert_eq!(
        outcome.dispatch.tracking_number,
        Some("MPL-XYZ-999".to_string())
    );
    // Noop spawner — no invoice id.
    assert!(outcome.spawned_invoice_id.is_none());
    assert!(outcome.dispatch.spawned_invoice_id.is_none());
    assert!(outcome.stock_movement_id.starts_with("mvt_"));

    // Stock decremented by 5.
    let widget_after = current_stock(&conn, TEST_TENANT, "prd_widget")
        .unwrap()
        .unwrap();
    assert_eq!(widget_after, Decimal::from_str("0").unwrap());

    // Audit ledger: one DispatchCreated + one DispatchShipped.
    assert_eq!(count_kind(&conn, "mes.dispatch_created"), 1);
    assert_eq!(count_kind(&conn, "mes.dispatch_shipped"), 1);
}

// ─────────────────────────────────────────────────────────────────────
// Invariants #4 + #5: with a real spawner, the spawned invoice draft
// rides the same tx and dispatches.spawned_invoice_id matches the
// returned id.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn mark_shipped_writes_movement_and_spawns_draft_in_same_tx() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-SHIP-SPAWN-001");

    let tx = conn.transaction().unwrap();
    let dsp = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id,
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-spawn".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let spawner = MockInvoiceSpawner::return_some("inv_test_001");
    let tx = conn.transaction().unwrap();
    let outcome = mark_shipped(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        &dsp.dsp_id,
        MarkShippedInputs {
            carrier_kind: CarrierKind::Gls,
            tracking_number: Some("GLS-1234".to_string()),
            shipped_at: None,
            idempotency_key: "ship-dsp-spawn".to_string(),
        },
        &spawner,
    )
    .unwrap();
    tx.commit().unwrap();

    assert_eq!(spawner.calls(), 1);
    assert_eq!(outcome.spawned_invoice_id.as_deref(), Some("inv_test_001"));
    assert_eq!(
        outcome.dispatch.spawned_invoice_id.as_deref(),
        Some("inv_test_001")
    );

    // Invariant #4: the invoice draft exists and is NOT issued + has
    // nav_status = NONE.
    let (is_issued, nav_status, partner_id, qty): (bool, String, String, String) = conn
        .query_row(
            "SELECT is_issued, nav_status, partner_id, qty
             FROM test_invoice_drafts WHERE invoice_id = ?",
            duckdb::params!["inv_test_001"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert!(!is_issued, "spawn_invoice_creates_drafted_not_issued");
    assert_eq!(nav_status, "NONE");
    assert_eq!(partner_id, "ptr_acme");
    // wo.qty_target propagated to the draft. DECIMAL(18,6) renders as
    // "5.000000" — parse both sides as Decimal for a value comparison.
    assert_eq!(
        Decimal::from_str(&qty).unwrap(),
        Decimal::from_str("5").unwrap()
    );
}

// ─────────────────────────────────────────────────────────────────────
// Invariant #6: a failed spawner rolls back the ENTIRE mark_shipped
// tx — no state flip, no stock movement, no audit entry.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn mark_shipped_rolls_back_on_draft_failure() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-SHIP-ROLLBACK-001");

    let tx = conn.transaction().unwrap();
    let dsp = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id,
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-rollback".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let widget_before = current_stock(&conn, TEST_TENANT, "prd_widget")
        .unwrap()
        .unwrap();
    let created_count_before = count_kind(&conn, "mes.dispatch_created");
    let shipped_count_before = count_kind(&conn, "mes.dispatch_shipped");

    let spawner = MockInvoiceSpawner::return_err("partner master-data missing");
    let tx = conn.transaction().unwrap();
    let result = mark_shipped(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        &dsp.dsp_id,
        MarkShippedInputs {
            carrier_kind: CarrierKind::Dpd,
            tracking_number: Some("DPD-666".to_string()),
            shipped_at: None,
            idempotency_key: "ship-dsp-rollback".to_string(),
        },
        &spawner,
    );
    // Tx is dropped without commit — DuckDB rolls back automatically.
    drop(tx);

    assert!(matches!(result, Err(DispatchError::InvoiceSpawnFailed(_))));
    assert_eq!(spawner.calls(), 1);

    // Dispatch row is STILL Drafted — no state flip persisted.
    let dsp_after = get_dispatch(&conn, TEST_TENANT, &dsp.dsp_id)
        .unwrap()
        .unwrap();
    assert_eq!(dsp_after.state, DispatchState::Drafted);
    assert!(dsp_after.shipped_at.is_none());
    assert!(dsp_after.carrier_kind.is_none());
    assert!(dsp_after.tracking_number.is_none());
    assert!(dsp_after.spawned_invoice_id.is_none());

    // Stock is unchanged.
    let widget_after = current_stock(&conn, TEST_TENANT, "prd_widget")
        .unwrap()
        .unwrap();
    assert_eq!(widget_after, widget_before);

    // No DispatchShipped audit entry written; DispatchCreated count
    // unchanged from before the ship attempt.
    assert_eq!(
        count_kind(&conn, "mes.dispatch_created"),
        created_count_before
    );
    assert_eq!(
        count_kind(&conn, "mes.dispatch_shipped"),
        shipped_count_before
    );

    // The orphan invoice-draft row was also rolled back.
    let invoice_drafts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM test_invoice_drafts",
            duckdb::params![],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(invoice_drafts, 0);
}

// ─────────────────────────────────────────────────────────────────────
// Invariant #2: mark_shipped is idempotent on an already-Shipped row
// (operator's double-click against a stale UI is a 200 OK noop).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn mark_shipped_idempotent_on_already_shipped() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-IDEM-001");

    let tx = conn.transaction().unwrap();
    let dsp = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id,
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-idem".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // First ship.
    let tx = conn.transaction().unwrap();
    mark_shipped(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        &dsp.dsp_id,
        MarkShippedInputs {
            carrier_kind: CarrierKind::SelfDelivery,
            tracking_number: None,
            shipped_at: None,
            idempotency_key: "ship-dsp-idem-1".to_string(),
        },
        &NoopInvoiceSpawner,
    )
    .unwrap();
    tx.commit().unwrap();

    let movement_count_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM stock_movements WHERE reason = 'dispatch'",
            duckdb::params![],
            |row| row.get(0),
        )
        .unwrap();
    let shipped_audit_before = count_kind(&conn, "mes.dispatch_shipped");

    // Second ship — should noop.
    let tx = conn.transaction().unwrap();
    let outcome = mark_shipped(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        &dsp.dsp_id,
        MarkShippedInputs {
            carrier_kind: CarrierKind::Gls, // operator picks a different carrier on the stale UI
            tracking_number: Some("DIFFERENT".to_string()),
            shipped_at: None,
            idempotency_key: "ship-dsp-idem-2".to_string(),
        },
        &NoopInvoiceSpawner,
    )
    .unwrap();
    tx.commit().unwrap();

    // The original carrier sticks (the second call was a noop).
    assert_eq!(outcome.dispatch.state, DispatchState::Shipped);
    assert_eq!(
        outcome.dispatch.carrier_kind,
        Some(CarrierKind::SelfDelivery)
    );
    assert_eq!(outcome.stock_movement_id, "<idempotent-noop>");

    // No new stock_movement and no new audit row.
    let movement_count_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM stock_movements WHERE reason = 'dispatch'",
            duckdb::params![],
            |row| row.get(0),
        )
        .unwrap();
    let shipped_audit_after = count_kind(&conn, "mes.dispatch_shipped");
    assert_eq!(movement_count_after, movement_count_before);
    assert_eq!(shipped_audit_after, shipped_audit_before);
}

// ─────────────────────────────────────────────────────────────────────
// cancel_dispatch: Drafted → Cancelled with NO inventory side-effect.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cancel_drafted_dispatch_has_no_inventory_impact() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-CANCEL-001");

    let tx = conn.transaction().unwrap();
    let dsp = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id,
            partner_id: "ptr_acme".to_string(),
            notes: Some("operator changed their mind".to_string()),
            idempotency_key: "create-dsp-cancel".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let widget_before = current_stock(&conn, TEST_TENANT, "prd_widget")
        .unwrap()
        .unwrap();

    let tx = conn.transaction().unwrap();
    let cancelled = cancel_dispatch(&tx, &dispatch_ctx_for(&meta, "ervin"), &dsp.dsp_id).unwrap();
    tx.commit().unwrap();

    assert_eq!(cancelled.state, DispatchState::Cancelled);
    assert!(cancelled.cancelled_at.is_some());

    // Stock unchanged.
    let widget_after = current_stock(&conn, TEST_TENANT, "prd_widget")
        .unwrap()
        .unwrap();
    assert_eq!(widget_after, widget_before);

    // No DispatchShipped audit; DispatchCreated count is 1 (no
    // dedicated DispatchCancelled kind per ADR-0064 §6).
    assert_eq!(count_kind(&conn, "mes.dispatch_created"), 1);
    assert_eq!(count_kind(&conn, "mes.dispatch_shipped"), 0);
}

// ─────────────────────────────────────────────────────────────────────
// Cancel on a Shipped dispatch is refused (Shipped is terminal).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cancel_shipped_dispatch_is_refused() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-CANCEL-SHIPPED-001");

    let tx = conn.transaction().unwrap();
    let dsp = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id,
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-cs".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    mark_shipped(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        &dsp.dsp_id,
        MarkShippedInputs {
            carrier_kind: CarrierKind::CustomerPickup,
            tracking_number: None,
            shipped_at: None,
            idempotency_key: "ship-cs".to_string(),
        },
        &NoopInvoiceSpawner,
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let result = cancel_dispatch(&tx, &dispatch_ctx_for(&meta, "ervin"), &dsp.dsp_id);
    assert!(matches!(result, Err(DispatchError::IllegalTransition(_))));
}

// ─────────────────────────────────────────────────────────────────────
// list_eligible_work_orders excludes dispatched WOs.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn list_eligible_work_orders_excludes_already_dispatched() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id_a = create_completed_wo(&mut conn, &meta, "WO-ELIG-A");
    let wo_id_b = create_completed_wo(&mut conn, &meta, "WO-ELIG-B");

    // Before any dispatch: both eligible.
    let eligible = list_eligible_work_orders(&conn, TEST_TENANT, 50).unwrap();
    let eligible_ids: Vec<String> = eligible.iter().map(|e| e.wo_id.clone()).collect();
    assert!(eligible_ids.contains(&wo_id_a));
    assert!(eligible_ids.contains(&wo_id_b));

    // Dispatch WO-A → it leaves the eligible list.
    let tx = conn.transaction().unwrap();
    create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id: wo_id_a.clone(),
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-elig-A".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let eligible_after = list_eligible_work_orders(&conn, TEST_TENANT, 50).unwrap();
    let eligible_after_ids: Vec<String> = eligible_after.iter().map(|e| e.wo_id.clone()).collect();
    assert!(!eligible_after_ids.contains(&wo_id_a));
    assert!(eligible_after_ids.contains(&wo_id_b));
}

// ─────────────────────────────────────────────────────────────────────
// create_dispatch refuses unknown partner_id.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn create_dispatch_refuses_unknown_partner() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-PARTNER-001");

    let tx = conn.transaction().unwrap();
    let result = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id,
            partner_id: "ptr_does_not_exist".to_string(),
            notes: None,
            idempotency_key: "create-dsp-partner-x".to_string(),
        },
    );
    assert!(matches!(result, Err(DispatchError::PartnerNotFound(_))));
}

// ─────────────────────────────────────────────────────────────────────
// DispatchShipped audit payload is parseable + structurally correct
// (pins the ADR-0064 §6 wire shape).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn dispatch_shipped_audit_payload_parses_with_expected_fields() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_id = create_completed_wo(&mut conn, &meta, "WO-PAYLOAD-001");

    let tx = conn.transaction().unwrap();
    let dsp = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id: wo_id.clone(),
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-payload".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    mark_shipped(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        &dsp.dsp_id,
        MarkShippedInputs {
            carrier_kind: CarrierKind::Foxpost,
            tracking_number: Some("FOX-007".to_string()),
            shipped_at: Some("2026-06-03T12:00:00Z".to_string()),
            idempotency_key: "ship-payload".to_string(),
        },
        &MockInvoiceSpawner::return_some("inv_payload_test"),
    )
    .unwrap();
    tx.commit().unwrap();

    // Pull the audit payload off the ledger + parse it.
    let payload_bytes: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM audit_ledger WHERE kind = ? LIMIT 1",
            duckdb::params!["mes.dispatch_shipped"],
            |row| row.get(0),
        )
        .unwrap();
    let payload: aberp_dispatch::DispatchShippedPayload =
        serde_json::from_slice(&payload_bytes).unwrap();
    assert_eq!(payload.dsp_id, dsp.dsp_id);
    assert_eq!(payload.wo_id, wo_id);
    assert_eq!(payload.partner_id, "ptr_acme");
    assert_eq!(payload.carrier_kind, CarrierKind::Foxpost);
    assert_eq!(payload.tracking_number.as_deref(), Some("FOX-007"));
    assert_eq!(
        payload.spawned_invoice_id.as_deref(),
        Some("inv_payload_test")
    );
    assert_eq!(payload.shipped_at, "2026-06-03T12:00:00Z");
    assert_eq!(payload.actor, "ervin");
}

// ─────────────────────────────────────────────────────────────────────
// list_dispatches state filter + newest-first sort.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn list_dispatches_filters_and_sorts_newest_first() {
    let mut conn = setup_db();
    let meta = meta();
    let wo_a = create_completed_wo(&mut conn, &meta, "WO-LIST-A");
    let wo_b = create_completed_wo(&mut conn, &meta, "WO-LIST-B");

    let tx = conn.transaction().unwrap();
    let dsp_a = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id: wo_a,
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "list-a".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Sleep is not portable across CI; create_at ordering by ULID is
    // good enough since dsp_id contains a monotonic ULID. The repo
    // sorts by (created_at DESC, dsp_id DESC) so ULID tiebreaks if
    // two rows land in the same second.
    let tx = conn.transaction().unwrap();
    let dsp_b = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id: wo_b,
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "list-b".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Full list: B first (newest), A second.
    let all = list_dispatches(&conn, TEST_TENANT, None, 50, 0).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].dsp_id, dsp_b.dsp_id);
    assert_eq!(all[1].dsp_id, dsp_a.dsp_id);

    // Filter on Drafted: both.
    let drafted = list_dispatches(&conn, TEST_TENANT, Some(DispatchState::Drafted), 50, 0).unwrap();
    assert_eq!(drafted.len(), 2);

    // Filter on Shipped: none.
    let shipped = list_dispatches(&conn, TEST_TENANT, Some(DispatchState::Shipped), 50, 0).unwrap();
    assert_eq!(shipped.len(), 0);
}

// ─────────────────────────────────────────────────────────────────────
// WO that has been Completed but whose only QA inspection was Disposed
// → WO never reaches Completed state → not eligible. (Pinned via the
// eligibility-checks-WO-state path; the WO-Complete gate is itself
// aberp-qa's invariant, not ours, but we re-check the contract.)
// ─────────────────────────────────────────────────────────────────────

#[test]
fn wo_in_active_state_is_not_eligible() {
    let mut conn = setup_db();
    let meta = meta();
    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar", "Raw bar");
    insert_partner(&conn, "ptr_acme", "ACME Kft.");
    seed_component_stock(&mut conn, &meta, "prd_bar", "100");

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
    let (wo, _) = create_work_order(
        &tx,
        &wo_ctx_for(&meta, "ervin"),
        CreateWorkOrderInputs {
            wo_number: "WO-ACTIVE-001".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("1").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Assemble".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-active".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Release only — WO is in Active (Released) state, not Completed.
    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &wo_ctx_for(&meta, "ervin"),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Release,
            reason: None,
            source_event_id: None,
            idempotency_key: "release-active".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    // Not in the eligible list.
    let eligible = list_eligible_work_orders(&conn, TEST_TENANT, 50).unwrap();
    let eligible_ids: Vec<String> = eligible.iter().map(|e| e.wo_id.clone()).collect();
    assert!(!eligible_ids.contains(&wo.wo_id));

    // create_dispatch refuses with the actual state name.
    let tx = conn.transaction().unwrap();
    let result = create_dispatch(
        &tx,
        &dispatch_ctx_for(&meta, "ervin"),
        CreateDispatchInputs {
            wo_id: wo.wo_id.clone(),
            partner_id: "ptr_acme".to_string(),
            notes: None,
            idempotency_key: "create-dsp-active".to_string(),
        },
    );
    match result {
        Err(DispatchError::WorkOrderNotEligible { state, .. }) => {
            // The state could be "released" or "started" depending on
            // the WO state machine vocabulary; the precise check is
            // "not completed".
            assert_ne!(state, "completed");
        }
        other => panic!("expected WorkOrderNotEligible, got {other:?}"),
    }
}

// Required for the WorkOrderState import to not be unused.
#[allow(dead_code)]
fn _force_wo_state_use() -> WorkOrderState {
    WorkOrderState::Completed
}
