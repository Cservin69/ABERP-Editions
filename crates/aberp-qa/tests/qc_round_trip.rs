//! S443 / ADR-0092 — integration tests for the QC inspection write path
//! against a fresh in-memory DuckDB. Pins: record_inspection writes a row
//! + emits the right events per verdict tier; auto-NCR-recommended tracks
//! the failing tiers; calibration-stale records-but-warns (never
//! recommends an NCR); units mismatch fails loud; plan CRUD enforces
//! unique (product, feature) + archive-not-delete.

use aberp_audit_ledger::{
    ensure_schema as ensure_audit_schema, Actor, BinaryHash, EventKind, LedgerMeta, TenantId,
};
use aberp_inventory::ActorKind;
use aberp_qa::{
    archive_inspection_plan, create_inspection_plan, ensure_schema as ensure_qa_schema,
    get_inspection_plan, link_auto_ncr, list_inspection_plans, list_inspections_for_part,
    list_inspections_for_wo, list_recent_stale_calibration, record_inspection, NewInspectionPlan,
    QcError, QcSource, QcWriteContext, RecordInspectionInputs, Verdict,
};
use duckdb::Connection;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

const TEST_TENANT: &str = "ten_test_qc";

fn setup_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    ensure_audit_schema(&conn).unwrap();
    ensure_qa_schema(&conn).unwrap();
    conn
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(
        TenantId::new(TEST_TENANT).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
}

fn qc_ctx<'a>(meta: &'a LedgerMeta, login: &str) -> QcWriteContext<'a> {
    QcWriteContext {
        tenant: TEST_TENANT,
        actor: ActorKind::SpaOperator {
            operator_login: login.to_string(),
        },
        ledger_meta: meta,
        ledger_actor: Actor::from_local_cli("qc-test-session".to_string(), login),
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::parse("2026-06-17T12:00:00Z", &Rfc3339).unwrap()
}

fn seed_plan(conn: &Connection, feature: &str, nominal: f64, upper: f64, lower: f64) -> String {
    create_inspection_plan(
        conn,
        TEST_TENANT,
        NewInspectionPlan {
            product_id: "prod_1".into(),
            feature_name: feature.into(),
            nominal_value: nominal,
            upper_tol: upper,
            lower_tol: lower,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            // ADR-0199 §D3(a) — this fixture predates the identity columns
            // and deliberately leaves them unset, which pins that the
            // ADR-0092 write path is unchanged by their addition.
            characteristic_number: None,
            characteristic_designator: None,
            characteristic_type: None,
            inspection_method: None,
            sheet_zone: None,
            is_required: None,
        },
    )
    .unwrap()
    .plan_id
}

fn count_kind(conn: &Connection, kind: EventKind) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?;",
        duckdb::params![kind.as_str()],
        |r| r.get(0),
    )
    .unwrap()
}

/// Record one manual inspection in its own tx (mirrors the app orchestrator).
fn record_one(
    conn: &mut Connection,
    meta: &LedgerMeta,
    plan_id: &str,
    actual: f64,
    last_cal: Option<OffsetDateTime>,
    wo_id: Option<&str>,
    part_uid: Option<&str>,
) -> aberp_qa::RecordedInspection {
    let plan = get_inspection_plan(conn, TEST_TENANT, plan_id)
        .unwrap()
        .unwrap();
    let ctx = qc_ctx(meta, "ervin");
    let tx = conn.transaction().unwrap();
    let recorded = record_inspection(
        &tx,
        &ctx,
        RecordInspectionInputs {
            plan: &plan,
            source: QcSource::Manual,
            source_event_id: None,
            actual_value: actual,
            units: "mm".into(),
            probe_serial: None,
            last_calibration_at: last_cal,
            measured_at: now(),
            current_time: now(),
            stale_window_seconds: 86400,
            linked_part_uid: part_uid.map(str::to_string),
            linked_heat_lot: None,
            linked_wo_id: wo_id.map(str::to_string),
            recorded_by: "ervin".into(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
    recorded
}

#[test]
fn pass_records_row_emits_passed_no_ncr() {
    let mut conn = setup_db();
    let meta = meta();
    let plan = seed_plan(&conn, "Bore Ø", 10.0, 0.010, -0.010);
    let r = record_one(&mut conn, &meta, &plan, 10.0, None, Some("WO-1"), None);
    assert_eq!(r.verdict, Verdict::Pass);
    assert!(!r.auto_ncr_recommended);
    assert_eq!(count_kind(&conn, EventKind::QcInspectionRecorded), 1);
    assert_eq!(count_kind(&conn, EventKind::QcInspectionPassed), 1);
    assert_eq!(count_kind(&conn, EventKind::QcInspectionFailed), 0);
    let rows = list_inspections_for_wo(&conn, TEST_TENANT, "WO-1").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].verdict, Verdict::Pass);
    assert_eq!(rows[0].deviation, 0.0);
}

#[test]
fn each_failing_tier_recommends_ncr_and_emits_failed() {
    let mut conn = setup_db();
    let meta = meta();
    let plan = seed_plan(&conn, "Bore Ø", 10.0, 0.010, -0.010); // half_width 0.010

    // Minor: overage 0.005 → ratio 0.5.
    let minor = record_one(&mut conn, &meta, &plan, 10.015, None, Some("WO-1"), None);
    assert_eq!(minor.verdict, Verdict::Minor);
    assert!(minor.auto_ncr_recommended);

    // Major: overage 0.015 → ratio 1.5.
    let major = record_one(&mut conn, &meta, &plan, 10.025, None, Some("WO-1"), None);
    assert_eq!(major.verdict, Verdict::Major);
    assert!(major.auto_ncr_recommended);

    // Critical: overage 0.025 → ratio 2.5.
    let crit = record_one(&mut conn, &meta, &plan, 10.035, None, Some("WO-1"), None);
    assert_eq!(crit.verdict, Verdict::Critical);
    assert!(crit.auto_ncr_recommended);

    assert_eq!(count_kind(&conn, EventKind::QcInspectionFailed), 3);
    assert_eq!(count_kind(&conn, EventKind::QcInspectionPassed), 0);
    assert_eq!(count_kind(&conn, EventKind::QcInspectionRecorded), 3);
}

#[test]
fn calibration_stale_records_warning_not_failure() {
    let mut conn = setup_db();
    let meta = meta();
    let plan = seed_plan(&conn, "Bore Ø", 10.0, 0.010, -0.010);
    // Wildly out of tolerance, BUT stale calibration (2 days > 1 day window).
    let stale_cal = now() - Duration::days(2);
    let r = record_one(
        &mut conn,
        &meta,
        &plan,
        10.500,
        Some(stale_cal),
        Some("WO-1"),
        None,
    );
    assert_eq!(r.verdict, Verdict::CalibrationStale);
    assert!(
        !r.auto_ncr_recommended,
        "a stale-calibration measurement must NOT recommend an NCR"
    );
    assert_eq!(
        count_kind(&conn, EventKind::QcProbeCalibrationStaleWarning),
        1
    );
    assert_eq!(count_kind(&conn, EventKind::QcInspectionFailed), 0);
    assert_eq!(count_kind(&conn, EventKind::QcInspectionRecorded), 1);

    // The row is on the dashboard stale feed.
    let stale = list_recent_stale_calibration(&conn, TEST_TENANT, now(), 30 * 24 * 3600).unwrap();
    assert_eq!(stale.len(), 1);
    assert!(stale[0].calibration_stale_at_event);
}

#[test]
fn link_auto_ncr_sets_id_and_emits_cross_link() {
    let mut conn = setup_db();
    let meta = meta();
    let plan = seed_plan(&conn, "Bore Ø", 10.0, 0.010, -0.010);
    let r = record_one(
        &mut conn,
        &meta,
        &plan,
        10.025,
        None,
        Some("WO-1"),
        Some("dp-X"),
    );
    assert_eq!(r.verdict, Verdict::Major);

    let ctx = qc_ctx(&meta, "ervin");
    let tx = conn.transaction().unwrap();
    link_auto_ncr(&tx, &ctx, &r.inspection.qci_id, "ncr_TEST", r.verdict).unwrap();
    tx.commit().unwrap();
    assert_eq!(count_kind(&conn, EventKind::QcAutoNcrCreated), 1);

    let rows = list_inspections_for_part(&conn, TEST_TENANT, "dp-X").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].auto_ncr_id.as_deref(), Some("ncr_TEST"));
}

#[test]
fn units_mismatch_fails_loud() {
    let mut conn = setup_db();
    let meta = meta();
    let plan_id = seed_plan(&conn, "Bore Ø", 10.0, 0.010, -0.010);
    let plan = get_inspection_plan(&conn, TEST_TENANT, &plan_id)
        .unwrap()
        .unwrap();
    let ctx = qc_ctx(&meta, "ervin");
    let tx = conn.transaction().unwrap();
    let err = record_inspection(
        &tx,
        &ctx,
        RecordInspectionInputs {
            plan: &plan,
            source: QcSource::Probe,
            source_event_id: Some("evt-1".into()),
            actual_value: 10.0,
            units: "µm".into(), // plan is "mm"
            probe_serial: Some("RMP600".into()),
            last_calibration_at: None,
            measured_at: now(),
            current_time: now(),
            stale_window_seconds: 86400,
            linked_part_uid: None,
            linked_heat_lot: None,
            linked_wo_id: None,
            recorded_by: "ervin".into(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, QcError::UnitsMismatch { .. }));
}

#[test]
fn plan_unique_product_feature_enforced() {
    let conn = setup_db();
    seed_plan(&conn, "Bore Ø", 10.0, 0.010, -0.010);
    // Same (product, feature) → rejected.
    let dup = create_inspection_plan(
        &conn,
        TEST_TENANT,
        NewInspectionPlan {
            product_id: "prod_1".into(),
            feature_name: "Bore Ø".into(),
            nominal_value: 9.0,
            upper_tol: 0.02,
            lower_tol: -0.02,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: None,
            characteristic_designator: None,
            characteristic_type: None,
            inspection_method: None,
            sheet_zone: None,
            is_required: None,
        },
    );
    assert!(matches!(dup, Err(QcError::Validation(_))));
    // A different feature on the same product is fine.
    seed_plan(&conn, "Length", 50.0, 0.05, -0.05);
    assert_eq!(
        list_inspection_plans(&conn, TEST_TENANT, Some("prod_1"), false)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn plan_archive_not_delete_frees_the_unique_slot() {
    let conn = setup_db();
    let plan_id = seed_plan(&conn, "Bore Ø", 10.0, 0.010, -0.010);
    archive_inspection_plan(&conn, TEST_TENANT, &plan_id).unwrap();
    // Archived row is hidden by default but still present with include_archived.
    assert_eq!(
        list_inspection_plans(&conn, TEST_TENANT, None, false)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        list_inspection_plans(&conn, TEST_TENANT, None, true)
            .unwrap()
            .len(),
        1
    );
    // Archiving freed the unique (product, feature) slot → re-create succeeds.
    seed_plan(&conn, "Bore Ø", 10.0, 0.012, -0.012);
}

#[test]
fn plan_rejects_degenerate_tolerance_band() {
    let conn = setup_db();
    let bad = create_inspection_plan(
        &conn,
        TEST_TENANT,
        NewInspectionPlan {
            product_id: "prod_1".into(),
            feature_name: "Bad".into(),
            nominal_value: 10.0,
            upper_tol: -0.010, // upper <= lower → degenerate
            lower_tol: 0.010,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: None,
            characteristic_designator: None,
            characteristic_type: None,
            inspection_method: None,
            sheet_zone: None,
            is_required: None,
        },
    );
    assert!(matches!(bad, Err(QcError::Validation(_))));
}
