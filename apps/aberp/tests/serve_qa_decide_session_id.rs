//! PR-242 / S250 finding 2 pin — `decide_qa_inspection_request` mints
//! ONE session ULID at route entry and reuses it across both the QA-
//! decide actor and the cascading WO auto-complete actor.
//!
//! Pre-fix behaviour: the QA-decide tx wrote `QaInspectionDecided` with
//! session_id A and `WorkOrderStateChanged` + `WoCompletion` with
//! session_id B. A forensic walker reconciling "what did one operator
//! do in one click" could not join those by `actor.session_id`.
//!
//! This test asserts: after a single Pass that satisfies the QA gate on
//! a one-op WO, every audit row appended by the route handler shares
//! the SAME `actor.session_id`. The WO/op/QA scaffolding mirrors
//! `crates/aberp-work-orders/tests/wo_auto_complete.rs` — single-op
//! variant for the slimmest possible end-to-end pin.

use std::path::PathBuf;
use std::sync::Arc;

use rust_decimal::Decimal;
use std::str::FromStr;

use aberp_audit_ledger::{
    ensure_schema as ensure_audit_schema, Actor, BinaryHash, EventKind, Ledger, LedgerMeta,
    TenantId,
};
use aberp_inventory::{
    ensure_schema as ensure_inventory_schema, record_movement, ActorKind, MovementReason,
    MovementRefKind, RecordMovementContext, RecordMovementInputs,
};
use aberp_qa::{ensure_schema as ensure_qa_schema, QaDecision};
use aberp_work_orders::{
    create_work_order, ensure_schema as ensure_wo_schema, replace_bom_for_product,
    transition_routing_op, transition_work_order, BomLineInput, CreateWorkOrderInputs,
    RoutingOpAction, RoutingOpInput, RoutingOpTransitionInputs, TransitionInputs, WoAction,
    WoWriteContext,
};
use duckdb::Connection;
use ulid::Ulid;

use aberp::serve::{self, AppState, DecideQaInspectionBody};

const TEST_TENANT: &str = "ten_test_qa_session_id";
const TEST_LOGIN: &str = "ervin";

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

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-qa-session-id")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn ensure_all_schemas(db_path: &PathBuf) {
    let conn = Connection::open(db_path).expect("open test DB");
    conn.execute_batch(PRODUCTS_SCHEMA_FOR_TESTS)
        .expect("create products test schema");
    ensure_inventory_schema(&conn).expect("inventory schema");
    ensure_audit_schema(&conn).expect("audit schema");
    ensure_wo_schema(&conn).expect("wo schema");
    ensure_qa_schema(&conn).expect("qa schema");
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
    .expect("insert product");
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(
        TenantId::new(TEST_TENANT).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
}

fn wo_ctx<'a>(meta: &'a LedgerMeta, login: &str) -> WoWriteContext<'a> {
    WoWriteContext {
        tenant: TEST_TENANT,
        actor: ActorKind::SpaOperator {
            operator_login: login.to_string(),
        },
        ledger_meta: meta,
        ledger_actor: Actor::from_local_cli("seed-session".to_string(), login),
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

/// Build a single-op WO, release it, start it, complete the op so a
/// Pending QA inspection exists. Returns the qa_id ready for decide.
fn seed_one_op_wo_with_pending_qa(db_path: &PathBuf) -> String {
    let mut conn = Connection::open(db_path).expect("reopen test DB");
    let m = meta();

    insert_product(&conn, "prd_widget", "Widget");
    insert_product(&conn, "prd_bar", "Raw bar");
    seed_component_stock(&mut conn, &m, "prd_bar", "10");

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
    let (wo, ops) = create_work_order(
        &tx,
        &wo_ctx(&m, TEST_LOGIN),
        CreateWorkOrderInputs {
            wo_number: "WO-SID-001".to_string(),
            product_id: "prd_widget".to_string(),
            qty_target: Decimal::from_str("1").unwrap(),
            notes: None,
            routing_ops: vec![RoutingOpInput {
                op_name: "Polish".to_string(),
                est_time_min: None,
                est_cost_huf: None,
            }],
            idempotency_key: "create-sid-1".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &wo_ctx(&m, TEST_LOGIN),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Release,
            reason: None,
            source_event_id: None,
            idempotency_key: "release-sid-1".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    transition_work_order(
        &tx,
        &wo_ctx(&m, TEST_LOGIN),
        &wo.wo_id,
        TransitionInputs {
            action: WoAction::Start,
            reason: None,
            source_event_id: None,
            idempotency_key: "start-sid-1".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let outcome = transition_routing_op(
        &tx,
        &wo_ctx(&m, TEST_LOGIN),
        &ops[0].routing_op_id,
        RoutingOpTransitionInputs {
            action: RoutingOpAction::Complete,
            source_event_id: None,
            idempotency_key: "op-complete-sid-1".to_string(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    outcome.qa_inspection_id
}

fn build_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    AppState {
        db_path: Arc::new(db_path),
        tenant,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        nav_poll_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
            aberp::serve::NAV_POLL_DAEMON_CONCURRENCY,
        )),
        boot_state: Arc::new(std::sync::RwLock::new(
            aberp::serve::ServeBootState::Ready {
                operator_login: TEST_LOGIN.to_string(),
            },
        )),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        adapter_registry: Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
        adapter_manager: Arc::new(aberp::mes_manager::AdapterManager::new(
            Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
            tokio_util::sync::CancellationToken::new(),
        )),
        adapter_health_baseline: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        restore_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        catalogue_push: aberp::catalogue_push::CataloguePushHandle::dormant(),
        email_relay_rate_limiter: std::sync::Arc::new(aberp::email_relay::RateLimiter::new()),
        pipeline_python_resolution: aberp::quote_pricing_pipeline::PythonResolutionHandle::dormant(
        ),
        storefront_credential: aberp::storefront_credential::StorefrontCredentialHandle::dormant(),
        email_outbox_daemon: aberp::email_outbox_poll_daemon::EmailOutboxDaemonHandle::dormant(),
        quote_pdf_rerender_queue: aberp::quote_pdf_rerender_queue::QuotePdfRerenderQueue::new(),
        digital_id: std::sync::Arc::new(aberp_digital_id::MockProvider::new()),
    }
}

#[test]
fn qa_decide_with_auto_complete_shares_session_id_across_audit_rows() {
    let dir = test_dir("share-session-id");
    let db_path = dir.join("test.duckdb");
    ensure_all_schemas(&db_path);

    let qa_id = seed_one_op_wo_with_pending_qa(&db_path);

    // Snapshot the ledger length BEFORE the decide so we can isolate
    // the rows produced by THIS tx.
    let tenant = TenantId::new(TEST_TENANT.to_string()).unwrap();
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    let pre_count = Ledger::open(&db_path, tenant.clone(), binary_hash)
        .expect("open ledger pre-decide")
        .entries()
        .expect("read pre-decide entries")
        .len();

    // Drive the route handler. The body asks for Pass with no measurement
    // — the one-op WO satisfies the gate so the auto-complete cascade
    // fires inside the same tx.
    let state = build_state(db_path.clone());
    let body = DecideQaInspectionBody {
        decision: "pass".to_string(),
        reason: None,
        measurement: None,
        source_event_id: None,
        idempotency_key: "qa-decide-sid-1".to_string(),
    };
    let resp =
        serve::decide_qa_inspection_request(&state, &qa_id, TEST_LOGIN, body).expect("decide_qa");
    assert!(
        resp.wo_auto_completed.is_some(),
        "one-op Pass must trigger the auto-complete cascade so we have BOTH actors in one tx"
    );
    let _ = QaDecision::Pass; // keep import live to ensure aberp_qa stays in dep graph for the test

    // Walk the audit rows added by this single route call. They MUST all
    // share `actor.session_id` — otherwise a forensic operator cannot
    // join the cascade by session column (S249 finding 2).
    let entries = Ledger::open(&db_path, tenant, binary_hash)
        .expect("open ledger post-decide")
        .entries()
        .expect("read post-decide entries");
    let new_entries = &entries[pre_count..];
    assert!(
        new_entries.len() >= 2,
        "expected at least QaInspectionDecided + WorkOrderStateChanged rows, got {}",
        new_entries.len()
    );
    let saw_qa = new_entries
        .iter()
        .any(|e| matches!(e.kind, EventKind::QaInspectionDecided));
    let saw_wo = new_entries
        .iter()
        .any(|e| matches!(e.kind, EventKind::WorkOrderStateChanged));
    assert!(
        saw_qa,
        "post-decide entries must include QaInspectionDecided"
    );
    assert!(
        saw_wo,
        "post-decide entries must include WorkOrderStateChanged"
    );
    let mut session_ids: Vec<String> = new_entries
        .iter()
        .map(|e| e.actor.session_id.clone())
        .collect();
    session_ids.sort();
    session_ids.dedup();
    assert_eq!(
        session_ids.len(),
        1,
        "every audit row from one QA-decide tx must share actor.session_id, got {session_ids:?}"
    );
}
