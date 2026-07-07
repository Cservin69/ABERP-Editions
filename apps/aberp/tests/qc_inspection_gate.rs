//! S443 (ADR-0092) — QC inspection → auto-NCR → refuse-Shipment gate e2e.
//!
//! Walks the defense quality journey the brief names
//! ([[customer-journey-e2e-gate]]): defense WO → part marked → inspection
//! plan created → operator records a manual inspection that measures
//! OUT of tolerance (Major) → the verdict is computed IN CODE → an S439
//! NCR is auto-spawned (Workmanship, Major, referencing the unit) → the
//! existing S438/S439 refuse-Shipment gate BLOCKS the defense dispatch.
//! A calibration-stale measurement, by contrast, records a row + warning
//! and spawns NO NCR, so the gate stays open.
//!
//! Quality + qc functions open their own connections (audit lives on the
//! same file), so this uses a file-backed DuckDB, not in-memory.

use duckdb::{params, Connection};

use aberp::part_marking::{
    data_matrix_payload, ensure_schema as ensure_part_schema, generate_part_uid, record_part_marks,
    PartMark,
};
use aberp::partners::{create_partner, CustomerType, PartnerInputs, PartnerKind};
use aberp::qc_inspection::{record_manual_inspection, ManualInspectionRequest};
use aberp::serve::{resolve_open_ncr_gate, OpenNcrGate};

use aberp_audit_ledger::{ensure_schema as audit_ensure_schema, BinaryHash, TenantId};
use aberp_qa::{
    create_inspection_plan, ensure_schema as ensure_qa_schema, NewInspectionPlan, QcSource, Verdict,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const T: &str = "qc_gate_test";

struct Fixture {
    db_path: std::path::PathBuf,
    // ADR-0099 — the shared Handle the inspection + auto-NCR audit appends
    // route through.
    handle: aberp_db::HandleArc,
    tenant: TenantId,
    hash: BinaryHash,
}

fn setup() -> Fixture {
    let dir = std::env::temp_dir()
        .join("aberp-qc-gate-test")
        .join(ulid::Ulid::new().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("aberp.duckdb");
    {
        let conn = Connection::open(&db_path).unwrap();
        audit_ensure_schema(&conn).unwrap();
        ensure_part_schema(&conn).unwrap();
        aberp::quality::ensure_schema(&conn).unwrap();
        ensure_qa_schema(&conn).unwrap(); // V001 (qa) + V002 (qc)
        aberp_work_orders::ensure_schema(&conn).unwrap();
        aberp_dispatch::ensure_schema(&conn).unwrap();
        aberp::quote_pricing_jobs::ensure_schema(&conn).unwrap();
    }
    let tenant = TenantId::new(T).unwrap();
    let handle = aberp::serve::open_tenant_handle(&db_path, tenant.clone())
        .expect("open shared test Handle");
    Fixture {
        db_path,
        handle,
        tenant,
        hash: BinaryHash::from_bytes([0u8; 32]),
    }
}

fn partner_inputs(name: &str, ct: CustomerType) -> PartnerInputs {
    PartnerInputs {
        display_name: name.to_string(),
        legal_name: name.to_string(),
        kind: PartnerKind::Customer,
        customer_vat_status: Default::default(),
        customer_type: ct,
        tax_number: None,
        eu_vat_number: None,
        address_street: None,
        address_postal_code: None,
        address_city: None,
        address_country: None,
        bank_account: None,
        contact_email: None,
        contact_phone: None,
    }
}

fn seed_wo(conn: &Connection, wo_id: &str) {
    conn.execute(
        "INSERT INTO work_orders (
            wo_id, tenant_id, wo_number, product_id, qty_target, state, created_at
         ) VALUES (?1, ?2, ?3, 'prd_1', '1', 'completed', '2026-06-06T00:00:00Z')",
        params![wo_id, T, wo_id],
    )
    .unwrap();
}

fn seed_dispatch(conn: &Connection, dsp_id: &str, wo_id: &str, partner_id: &str) {
    conn.execute(
        "INSERT INTO dispatches (dsp_id, tenant_id, wo_id, partner_id, state, created_at)
         VALUES (?1, ?2, ?3, ?4, 'drafted', '2026-06-06T00:00:00Z')",
        params![dsp_id, T, wo_id, partner_id],
    )
    .unwrap();
}

fn mark_one_unit(conn: &Connection, wo_id: &str) -> String {
    let part_uid = generate_part_uid();
    let serial = "SN-1".to_string();
    let payload = data_matrix_payload(&part_uid, &serial, None);
    let mark = PartMark {
        wo_id: wo_id.to_string(),
        unit_index: 1,
        part_uid: part_uid.clone(),
        serial_number: serial,
        data_matrix_payload: payload,
        heat_lot_reference: None,
        marked_at_utc: "2026-06-16T00:00:00Z".to_string(),
        marked_by_operator: "op".to_string(),
    };
    record_part_marks(conn, T, wo_id, &[mark]).unwrap();
    part_uid
}

fn seed_plan(fx: &Fixture) -> String {
    // ADR-0099 — seed the plan through the SHARED Handle (as production's
    // handle_create_inspection_plan does via state.db.write()), so
    // record_manual_inspection's Handle-routed read sees it. A fresh
    // Connection::open would be a separate DuckDB instance the persistent
    // Handle does not observe.
    let conn = fx.handle.write().unwrap();
    create_inspection_plan(
        &conn,
        T,
        NewInspectionPlan {
            product_id: "prd_1".into(),
            feature_name: "Bore Ø".into(),
            nominal_value: 10.0,
            upper_tol: 0.010,
            lower_tol: -0.010,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
        },
    )
    .unwrap()
    .plan_id
}

fn now() -> OffsetDateTime {
    OffsetDateTime::parse("2026-06-17T12:00:00Z", &Rfc3339).unwrap()
}

fn dispatch(conn: &Connection, dsp_id: &str) -> aberp_dispatch::Dispatch {
    aberp_dispatch::get_dispatch(conn, T, dsp_id)
        .unwrap()
        .expect("read seeded dispatch")
}

/// The headline journey: a Major manual inspection auto-creates an NCR that
/// engages the defense refuse-Shipment gate.
#[test]
fn major_inspection_auto_ncr_blocks_defense_shipment() {
    let fx = setup();
    let conn = Connection::open(&fx.db_path).unwrap();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_wo(&conn, "wo-def");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let part_uid = mark_one_unit(&conn, "wo-def");
    drop(conn);

    let plan_id = seed_plan(&fx);

    // Operator records 10.025 mm against nominal 10.0 ±0.010 → overage
    // 0.015, ratio 1.5× half-width → MAJOR (verdict computed in code).
    let result = record_manual_inspection(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "ervin",
        now(),
        86400,
        ManualInspectionRequest {
            plan_id,
            actual_value: 10.025,
            source: QcSource::Manual,
            units: None,
            source_event_id: None,
            probe_serial: None,
            last_calibration_at: None,
            wo_id: Some("wo-def".into()),
            part_uid: Some(part_uid.clone()),
            heat_lot: None,
        },
    )
    .unwrap();

    assert_eq!(result.inspection.verdict, Verdict::Major);
    let ncr = result.auto_ncr.expect("a Major verdict auto-spawns an NCR");
    assert_eq!(ncr.severity, aberp::quality::NcrSeverity::Major);
    assert_eq!(ncr.category, aberp::quality::NcrCategory::Workmanship);
    assert_eq!(ncr.affected_part_uids, vec![part_uid.clone()]);
    assert_eq!(
        result.inspection.auto_ncr_id.as_deref(),
        Some(ncr.ncr_id.as_str())
    );

    // The refuse-Shipment gate now BLOCKS the defense dispatch, naming the
    // auto-spawned NCR.
    let conn = Connection::open(&fx.db_path).unwrap();
    match resolve_open_ncr_gate(&conn, T, &dispatch(&conn, "dsp-def")).unwrap() {
        OpenNcrGate::Blocked {
            work_order_id,
            customer_type,
            blocking_ncr_ids,
        } => {
            assert_eq!(work_order_id, "wo-def");
            assert_eq!(customer_type, "defense");
            assert_eq!(blocking_ncr_ids, vec![ncr.ncr_id]);
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
}

/// A Pass measurement spawns no NCR; the gate stays open.
#[test]
fn pass_inspection_does_not_block_shipment() {
    let fx = setup();
    let conn = Connection::open(&fx.db_path).unwrap();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_wo(&conn, "wo-ok");
    seed_dispatch(&conn, "dsp-ok", "wo-ok", &buyer.id);
    let part_uid = mark_one_unit(&conn, "wo-ok");
    drop(conn);

    let plan_id = seed_plan(&fx);
    let result = record_manual_inspection(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "ervin",
        now(),
        86400,
        ManualInspectionRequest {
            plan_id,
            actual_value: 10.005, // within ±0.010 → Pass
            source: QcSource::Manual,
            units: None,
            source_event_id: None,
            probe_serial: None,
            last_calibration_at: None,
            wo_id: Some("wo-ok".into()),
            part_uid: Some(part_uid),
            heat_lot: None,
        },
    )
    .unwrap();
    assert_eq!(result.inspection.verdict, Verdict::Pass);
    assert!(result.auto_ncr.is_none());

    let conn = Connection::open(&fx.db_path).unwrap();
    assert_eq!(
        resolve_open_ncr_gate(&conn, T, &dispatch(&conn, "dsp-ok")).unwrap(),
        OpenNcrGate::Pass,
    );
}

/// A calibration-stale measurement records a row + warning but NO NCR, so
/// the gate stays open even though the raw value is wildly out of tolerance.
#[test]
fn calibration_stale_measurement_spawns_no_ncr() {
    let fx = setup();
    let conn = Connection::open(&fx.db_path).unwrap();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_wo(&conn, "wo-stale");
    seed_dispatch(&conn, "dsp-stale", "wo-stale", &buyer.id);
    let part_uid = mark_one_unit(&conn, "wo-stale");
    drop(conn);

    let plan_id = seed_plan(&fx);
    // Calibration 2 days old, window 1 day → stale; value 10.5 (way out).
    let stale_cal = "2026-06-15T12:00:00Z".to_string();
    let result = record_manual_inspection(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "ervin",
        now(),
        86400,
        ManualInspectionRequest {
            plan_id,
            actual_value: 10.500,
            source: QcSource::Probe,
            units: None,
            source_event_id: Some("evt-1".into()),
            probe_serial: Some("RMP600-007".into()),
            last_calibration_at: Some(stale_cal),
            wo_id: Some("wo-stale".into()),
            part_uid: Some(part_uid),
            heat_lot: None,
        },
    )
    .unwrap();
    assert_eq!(result.inspection.verdict, Verdict::CalibrationStale);
    assert!(
        result.auto_ncr.is_none(),
        "a stale-calibration measurement must NOT manufacture an NCR"
    );

    let conn = Connection::open(&fx.db_path).unwrap();
    assert_eq!(
        resolve_open_ncr_gate(&conn, T, &dispatch(&conn, "dsp-stale")).unwrap(),
        OpenNcrGate::Pass,
    );
}
