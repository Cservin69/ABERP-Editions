//! ADR-0199 §D6 — the ROUTE-LEVEL half of the QC-report shipment gate.
//!
//! `qc_report_gate.rs` pins the pure resolver's verdicts. This file pins
//! what the route does with a `Blocked` verdict, which is the part an
//! operator actually experiences:
//!
//! > **409 Conflict + exactly one `qcr.report_shipment_blocked` audit row,
//! > and NOTHING of the shipment persisted.**
//!
//! The gate runs before `mark_shipped` opens its transaction, so a refusal
//! costs nothing and leaves the dispatch Drafted.
//!
//! ## Both editions, one file
//!
//! `mark_dispatch_shipped_request` reads the compile-time capability, so
//! this file branches on `qc_reporting_allowed()` at runtime rather than
//! `#[cfg]`-ing itself out: on a Defense build it asserts the refusal, on a
//! Portable build it asserts the SAME fixture is not refused. Each gate run
//! therefore proves its own edition's behaviour, and neither arm is a
//! silently-skipped test.

use std::path::PathBuf;
use std::sync::Arc;

use aberp::build_profile::qc_reporting_allowed;
use aberp::part_marking::{
    data_matrix_payload, ensure_schema as ensure_part_schema, generate_part_uid, record_part_marks,
    PartMark,
};
use aberp::serve::{self, AppState, MarkDispatchShippedBody, WorkOrderRouteError};
use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use aberp_qa::NewInspectionPlan;
use ulid::Ulid;

const TEST_TENANT: &str = "qc_report_refusal_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xC9; 32]);

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-qc-report-refusal")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn build_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    AppState {
        db: serve::open_tenant_handle(&db_path, tenant.clone()).expect("open shared handle"),
        db_path: Arc::new(db_path),
        tenant,
        nav_enabled: true,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(TEST_HASH),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        nav_poll_semaphore: Arc::new(tokio::sync::Semaphore::new(
            serve::NAV_POLL_DAEMON_CONCURRENCY,
        )),
        boot_state: Arc::new(std::sync::RwLock::new(serve::ServeBootState::Ready {
            operator_login: "test-operator".to_string(),
        })),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        adapter_registry: Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
        adapter_manager: Arc::new(aberp::mes_manager::AdapterManager::new(
            Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
            tokio_util::sync::CancellationToken::new(),
        )),
        adapter_health_baseline: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        restore_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        catalogue_push: aberp::catalogue_push::CataloguePushHandle::dormant(),
        email_relay_rate_limiter: Arc::new(aberp::email_relay::RateLimiter::new()),
        pipeline_python_resolution: aberp::quote_pricing_pipeline::PythonResolutionHandle::dormant(
        ),
        storefront_credential: aberp::storefront_credential::StorefrontCredentialHandle::dormant(),
        email_outbox_daemon: aberp::email_outbox_poll_daemon::EmailOutboxDaemonHandle::dormant(),
        quote_pdf_rerender_queue: aberp::quote_pdf_rerender_queue::QuotePdfRerenderQueue::new(),
        digital_id: Arc::new(aberp_digital_id::MockProvider::new()),
    }
}

/// Seed a defence dispatch whose WO has ONE required, UNMEASURED
/// characteristic — the minimum fixture that reaches the QC gate.
///
/// Every unit is marked (so the ADR-0089 part-UID gate passes) and no NCR
/// exists (so the ADR-0090 open-NCR gate passes). The QC gate is therefore
/// the FIRST one that can refuse, which is what makes the assertion below
/// unambiguous about which gate fired.
fn seed(state: &AppState) -> String {
    let dsp_id = format!("dsp_{}", Ulid::new());
    let conn = state.db.write().expect("writer");
    aberp::partners::ensure_schema(&conn).expect("partners schema");
    aberp_dispatch::ensure_schema(&conn).expect("dispatch schema");
    aberp_work_orders::ensure_schema(&conn).expect("wo schema");
    aberp_qa::ensure_schema(&conn).expect("qa/qc schema");
    ensure_part_schema(&conn).expect("part schema");
    aberp::quality::ensure_schema(&conn).expect("quality schema");

    conn.execute(
        "INSERT INTO partners (id, tenant_id, display_name, legal_name, kind, \
         customer_vat_status, customer_type, issued_invoice_count, created_at, updated_at) \
         VALUES ('ptr_prime', ?, 'Prime Aero', 'Prime Aero Kft', 'Customer', \
         'Domestic', 'defense', 0, ?, ?);",
        duckdb::params![TEST_TENANT, "2026-08-23T00:00:00Z", "2026-08-23T00:00:00Z"],
    )
    .expect("insert partner");
    conn.execute(
        "INSERT INTO work_orders (wo_id, tenant_id, wo_number, product_id, qty_target, \
         state, created_at) VALUES ('wo_qcr', ?, 'WO-QCR-1', 'prd_bracket', '1', \
         'completed', '2026-08-23T00:00:00Z');",
        duckdb::params![TEST_TENANT],
    )
    .expect("insert wo");
    conn.execute(
        "INSERT INTO dispatches (dsp_id, tenant_id, wo_id, partner_id, state, created_at) \
         VALUES (?, ?, 'wo_qcr', 'ptr_prime', 'drafted', '2026-08-23T00:00:00Z');",
        duckdb::params![&dsp_id, TEST_TENANT],
    )
    .expect("insert dispatch");

    // Mark the single unit so the part-UID gate passes.
    let part_uid = generate_part_uid();
    let serial = "SN-001".to_string();
    let payload = data_matrix_payload(&part_uid, &serial, None);
    record_part_marks(
        &conn,
        TEST_TENANT,
        "wo_qcr",
        &[PartMark {
            wo_id: "wo_qcr".into(),
            unit_index: 1,
            part_uid,
            serial_number: serial,
            data_matrix_payload: payload,
            heat_lot_reference: Some("HL-1".into()),
            marked_at_utc: "2026-08-23T00:00:00Z".into(),
            marked_by_operator: "op".into(),
        }],
    )
    .expect("record part marks");

    // ONE required characteristic, never measured ⇒ nothing can release.
    aberp_qa::create_inspection_plan(
        &conn,
        TEST_TENANT,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "Bore D".into(),
            nominal_value: 25.0,
            upper_tol: 0.05,
            lower_tol: -0.05,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("1".into()),
            characteristic_designator: None,
            characteristic_type: None,
            inspection_method: None,
            sheet_zone: None,
            is_required: Some(true),
        },
    )
    .expect("create plan");
    dsp_id
}

fn ship_body() -> MarkDispatchShippedBody {
    MarkDispatchShippedBody {
        carrier_kind: "gls".to_string(),
        tracking_number: Some("GLS-QCR-1".to_string()),
        shipped_at: None,
        idempotency_key: "ship-qcr-refusal".to_string(),
    }
}

fn count_kind(db_path: &PathBuf, kind: EventKind) -> usize {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let ledger = Ledger::open(db_path, tenant, TEST_HASH).expect("open ledger");
    ledger
        .entries()
        .expect("read entries")
        .into_iter()
        .filter(|e| e.kind == kind)
        .count()
}

/// **The refusal, end to end at the route.**
///
/// Defense build: 409 + exactly one `qcr.report_shipment_blocked` row, the
/// dispatch still Drafted, and NO `mes.dispatch_shipped`.
///
/// Portable build: the same fixture is NOT refused by this gate — proving
/// the feature is genuinely compiled out rather than merely quiet.
#[test]
fn an_incomplete_report_refuses_the_shipment_with_409_and_one_audit_row() {
    let dir = test_dir("refusal");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    let dsp_id = seed(&state);

    let before = count_kind(&db, EventKind::QcReportShipmentBlocked);
    assert_eq!(before, 0);

    let result = serve::mark_dispatch_shipped_request(&state, &dsp_id, "ervin", ship_body());

    if qc_reporting_allowed() {
        // ── Defense ──
        match result {
            Err(WorkOrderRouteError::Conflict(msg)) => {
                assert!(
                    msg.contains("Shipment blocked"),
                    "the 409 must name the refusal: {msg}"
                );
                assert!(
                    msg.contains("QC") || msg.contains("QC inspection report"),
                    "the 409 must say WHICH gate refused: {msg}"
                );
            }
            other => panic!("expected a 409 Conflict from the QC-report gate, got {other:?}"),
        }
        assert_eq!(
            count_kind(&db, EventKind::QcReportShipmentBlocked),
            1,
            "exactly ONE denial row — not zero (silent refusal) and not two"
        );
        assert_eq!(
            count_kind(&db, EventKind::DispatchShipped),
            0,
            "nothing of the shipment may persist behind a refusal"
        );
        let conn = state.db.read().expect("read");
        let dsp = aberp_dispatch::get_dispatch(&conn, TEST_TENANT, &dsp_id)
            .unwrap()
            .unwrap();
        assert_eq!(dsp.state, aberp_dispatch::DispatchState::Drafted);
        assert!(dsp.shipped_at.is_none());
    } else {
        // ── Portable ──
        //
        // The QC gate must not be what stops this shipment. It may still
        // fail further down (this fixture has no `products` row for the
        // stock movement), so the assertion is specifically that no QC
        // refusal was recorded — not that the ship succeeded.
        if let Err(WorkOrderRouteError::Conflict(msg)) = &result {
            assert!(
                !msg.contains("Shipment blocked:"),
                "a Portable build must never refuse a shipment over QC reporting: {msg}"
            );
        }
        assert_eq!(
            count_kind(&db, EventKind::QcReportShipmentBlocked),
            0,
            "a Portable build must not write a QC-report denial row"
        );
    }
}

/// A second refused click writes a SECOND denial row. Denials are events,
/// not state: an operator who tried to ship twice against an incomplete
/// report attempted it twice, and the audit trail must say so.
#[test]
fn each_refused_attempt_is_audited_separately() {
    if !qc_reporting_allowed() {
        return; // no denial path on Portable — covered by the test above
    }
    let dir = test_dir("refusal-twice");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    let dsp_id = seed(&state);

    for _ in 0..2 {
        let r = serve::mark_dispatch_shipped_request(&state, &dsp_id, "ervin", ship_body());
        assert!(matches!(r, Err(WorkOrderRouteError::Conflict(_))));
    }
    assert_eq!(count_kind(&db, EventKind::QcReportShipmentBlocked), 2);
}
