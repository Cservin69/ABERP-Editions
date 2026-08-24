//! ADR-0199 §D6 — the QC-report shipment gate, end to end.
//!
//! **This file pins the feature's single safety-critical behaviour:**
//!
//! > A missing REQUIRED characteristic ⇒ the report is `incomplete` ⇒
//! > `mark_shipped` is REFUSED (409 + one audit row).
//!
//! Ervin confirmed the blocking posture explicitly (ADR-0199 §Open Q3,
//! resolved 2026-08-23), so the refusal is a process commitment, not a
//! convenience. The tests below drive the real gate resolver against a
//! real DuckDB with real frozen reports, and check both directions — a
//! gate that always blocked would pass a negative-only test.
//!
//! The edition capability is passed in explicitly
//! (`resolve_qc_report_gate_with_capability`) so BOTH the Defense arm and
//! the Portable arm are exercised in ONE compile, on whichever edition the
//! gate run happens to build.

use duckdb::{params, Connection};

use aberp::part_marking::{
    data_matrix_payload, ensure_schema as ensure_part_schema, generate_part_uid, record_part_marks,
    PartMark,
};
use aberp::partners::{create_partner, CustomerType, PartnerInputs, PartnerKind};
use aberp::serve::{resolve_qc_report_gate_with_capability, QcReportBlockReason, QcReportGate};
use aberp_audit_ledger::{
    ensure_schema as audit_ensure_schema, Actor, BinaryHash, LedgerMeta, TenantId,
};
use aberp_inventory::ActorKind;
use aberp_qa::{
    create_inspection_plan, freeze_report, issue_report, list_inspection_plans,
    list_inspections_for_wo, record_inspection, CharacteristicType, Disposition,
    FreezeReportInputs, InspectionMethod, NewInspectionPlan, QcReportKind, QcReportTemplate,
    QcSource, QcWriteContext, RecordInspectionInputs, ReportCustomer, ReportTraceability,
    ReportUnit,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const T: &str = "qc_report_gate_test";
/// The Defense arm. Named so the assertions read as intent, not as a bool.
const QC_REPORTING_ON: bool = true;
/// The Portable arm.
const QC_REPORTING_OFF: bool = false;

fn setup() -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-qc-report-gate-test")
        .join(ulid::Ulid::new().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("aberp.duckdb");
    let conn = Connection::open(&db_path).unwrap();
    audit_ensure_schema(&conn).unwrap();
    ensure_part_schema(&conn).unwrap();
    aberp_qa::ensure_schema(&conn).unwrap();
    aberp_work_orders::ensure_schema(&conn).unwrap();
    aberp_dispatch::ensure_schema(&conn).unwrap();
    db_path
}

fn now() -> OffsetDateTime {
    OffsetDateTime::parse("2026-08-23T12:00:00Z", &Rfc3339).unwrap()
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(TenantId::new(T).unwrap(), BinaryHash::from_bytes([0u8; 32]))
}

fn ctx<'a>(m: &'a LedgerMeta) -> QcWriteContext<'a> {
    QcWriteContext {
        tenant: T,
        actor: ActorKind::SpaOperator {
            operator_login: "ervin".into(),
        },
        ledger_meta: m,
        ledger_actor: Actor::from_local_cli("qcr-gate-session".into(), "ervin"),
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

fn seed_wo(conn: &Connection, wo_id: &str, qty: &str) {
    conn.execute(
        "INSERT INTO work_orders (
            wo_id, tenant_id, wo_number, product_id, qty_target, state, created_at
         ) VALUES (?1, ?2, ?3, 'prd_bracket', ?4, 'completed', '2026-08-01T00:00:00Z')",
        params![wo_id, T, wo_id, qty],
    )
    .unwrap();
}

fn seed_dispatch(conn: &Connection, dsp_id: &str, wo_id: &str, partner_id: &str) {
    conn.execute(
        "INSERT INTO dispatches (dsp_id, tenant_id, wo_id, partner_id, state, created_at)
         VALUES (?1, ?2, ?3, ?4, 'drafted', '2026-08-02T00:00:00Z')",
        params![dsp_id, T, wo_id, partner_id],
    )
    .unwrap();
}

fn mark_units(conn: &Connection, wo_id: &str, n: u32) -> Vec<ReportUnit> {
    let mut marks = Vec::new();
    for i in 1..=n {
        let part_uid = generate_part_uid();
        let serial = format!("SN-{i:03}");
        let payload = data_matrix_payload(&part_uid, &serial, None);
        marks.push(PartMark {
            wo_id: wo_id.to_string(),
            unit_index: i,
            part_uid,
            serial_number: serial,
            data_matrix_payload: payload,
            heat_lot_reference: Some("HL-9911".into()),
            marked_at_utc: "2026-08-02T00:00:00Z".to_string(),
            marked_by_operator: "op".to_string(),
        });
    }
    record_part_marks(conn, T, wo_id, &marks).unwrap();
    marks
        .into_iter()
        .map(|m| ReportUnit {
            part_serial: m.serial_number,
            part_uid: m.part_uid,
        })
        .collect()
}

fn seed_plan(conn: &Connection, feature: &str, number: &str, required: bool) -> String {
    create_inspection_plan(
        conn,
        T,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: feature.into(),
            nominal_value: 25.0,
            upper_tol: 0.05,
            lower_tol: -0.05,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some(number.into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: None,
            is_required: Some(required),
        },
    )
    .unwrap()
    .plan_id
}

/// Promote an existing plan row from optional to required, leaving every
/// other field as `seed_plan` wrote it. This is what
/// `PUT /api/inspection-plans/:id` does.
fn promote_plan_to_required(conn: &Connection, plan_id: &str, feature: &str, number: &str) {
    aberp_qa::update_inspection_plan(
        conn,
        T,
        plan_id,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: feature.into(),
            nominal_value: 25.0,
            upper_tol: 0.05,
            lower_tol: -0.05,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some(number.into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: None,
            is_required: Some(true),
        },
    )
    .unwrap();
}

fn measure(conn: &mut Connection, plan_id: &str, part_uid: &str, actual: f64) {
    let m = meta();
    let plan = aberp_qa::get_inspection_plan(conn, T, plan_id)
        .unwrap()
        .unwrap();
    let tx = conn.transaction().unwrap();
    record_inspection(
        &tx,
        &ctx(&m),
        RecordInspectionInputs {
            plan: &plan,
            source: QcSource::Manual,
            source_event_id: None,
            actual_value: actual,
            units: "mm".into(),
            probe_serial: None,
            last_calibration_at: None,
            measured_at: now(),
            current_time: now(),
            stale_window_seconds: 86_400,
            linked_part_uid: Some(part_uid.into()),
            linked_heat_lot: Some("HL-9911".into()),
            linked_wo_id: Some("wo-def".into()),
            recorded_by: "ervin".into(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
}

/// `measure`, but at an explicit instant. `latest_measurement` breaks a
/// timestamp tie on the ULID `qci_id`, and two ULIDs minted in the same
/// millisecond order by their RANDOM suffix — so a re-measurement test
/// that relied on the fixed `now()` would be a coin flip.
fn measure_at(
    conn: &mut Connection,
    plan_id: &str,
    part_uid: &str,
    actual: f64,
    at: OffsetDateTime,
) {
    let m = meta();
    let plan = aberp_qa::get_inspection_plan(conn, T, plan_id)
        .unwrap()
        .unwrap();
    let tx = conn.transaction().unwrap();
    record_inspection(
        &tx,
        &ctx(&m),
        RecordInspectionInputs {
            plan: &plan,
            source: QcSource::Manual,
            source_event_id: None,
            actual_value: actual,
            units: "mm".into(),
            probe_serial: None,
            last_calibration_at: None,
            measured_at: at,
            current_time: at,
            stale_window_seconds: 86_400,
            linked_part_uid: Some(part_uid.into()),
            linked_heat_lot: Some("HL-9911".into()),
            linked_wo_id: Some("wo-def".into()),
            recorded_by: "ervin".into(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
}

/// A measurement attributed to the CHARACTERISTIC but to no part UID — the
/// first-article check taken before any unit carries a mark. With no marked
/// units, `build_report_lines` degrades every characteristic to one
/// lot-level line and `latest_measurement(.., lot_level = true)` accepts
/// this row, which is what makes the round-4 B-2 fixture a clean `accept`.
fn measure_lot_level(conn: &mut Connection, plan_id: &str, actual: f64) {
    let m = meta();
    let plan = aberp_qa::get_inspection_plan(conn, T, plan_id)
        .unwrap()
        .unwrap();
    let tx = conn.transaction().unwrap();
    record_inspection(
        &tx,
        &ctx(&m),
        RecordInspectionInputs {
            plan: &plan,
            source: QcSource::Manual,
            source_event_id: None,
            actual_value: actual,
            units: "mm".into(),
            probe_serial: None,
            last_calibration_at: None,
            measured_at: now(),
            current_time: now(),
            stale_window_seconds: 86_400,
            linked_part_uid: None,
            linked_heat_lot: Some("HL-9911".into()),
            linked_wo_id: Some("wo-def".into()),
            recorded_by: "ervin".into(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
}

/// Freeze + issue a report for `wo-def`, returning `(qcr_id, disposition)`.
fn issue_report_for(conn: &mut Connection, units: &[ReportUnit]) -> (String, Disposition) {
    issue_report_for_at(conn, units, now())
}

/// `issue_report_for`, but with the report's `created_at` under the test's
/// control. `freeze_report` formats the supplied instant through `time`'s
/// `Rfc3339`, so this is what lets a test produce the sub-second shapes the
/// recency ordering has to survive.
fn issue_report_for_at(
    conn: &mut Connection,
    units: &[ReportUnit],
    at: OffsetDateTime,
) -> (String, Disposition) {
    let m = meta();
    let plans = list_inspection_plans(conn, T, Some("prd_bracket"), false).unwrap();
    let inspections = list_inspections_for_wo(conn, T, "wo-def").unwrap();
    let tx = conn.transaction().unwrap();
    let (report, _) = freeze_report(
        &tx,
        &ctx(&m),
        FreezeReportInputs {
            report_kind: QcReportKind::DimensionalInspection,
            template: QcReportTemplate::AbenStandard,
            wo_id: "wo-def",
            product_id: "prd_bracket",
            partner_id: "ptr_x",
            plans: &plans,
            inspections: &inspections,
            units,
            open_ncr_against_reported_part: false,
            traceability: ReportTraceability::default(),
            customer: ReportCustomer::default(),
            created_by: "ervin",
        },
        at,
    )
    .unwrap();
    let issued = issue_report(
        &tx,
        &ctx(&m),
        &report.qcr_id,
        "deadbeef",
        "aberp-qc-pdf@0.0.0",
        "ervin",
        at,
    )
    .unwrap();
    tx.commit().unwrap();
    (issued.qcr_id, issued.disposition)
}

fn dispatch(conn: &Connection, dsp_id: &str) -> aberp_dispatch::Dispatch {
    aberp_dispatch::get_dispatch(conn, T, dsp_id)
        .unwrap()
        .unwrap()
}

// ═════════════════════════════════════════════════════════════════════
// THE safety-critical behaviour (ADR-0199 §AC2 + §D6).
// ═════════════════════════════════════════════════════════════════════

/// **A missing required characteristic REFUSES the Defense shipment.**
///
/// Four required characteristics across two units; three measured. The
/// report comes out `incomplete` and the gate blocks with reason
/// `Incomplete`, naming the offending report.
///
/// Then measure the missing one, issue a fresh report, and the gate
/// PASSES — the positive control that proves the block is caused by the
/// gap and not by the gate simply always refusing.
#[test]
fn a_missing_required_characteristic_blocks_the_defense_shipment_then_unblocks() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "2");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 2);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    let p2 = seed_plan(&conn, "Face Z", "2", true);

    // Measure 3 of the 4 (characteristic-unit pairs).
    measure(&mut conn, &p1, &units[0].part_uid, 25.0);
    measure(&mut conn, &p2, &units[0].part_uid, 25.0);
    measure(&mut conn, &p1, &units[1].part_uid, 25.0);

    let (qcr_id, disposition) = issue_report_for(&mut conn, &units);
    assert_eq!(
        disposition,
        Disposition::Incomplete,
        "3 of 4 required characteristics ⇒ incomplete"
    );

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked {
            work_order_id,
            customer_type,
            reason,
            qcr_id: blocked_id,
            disposition: disp,
        } => {
            assert_eq!(work_order_id, "wo-def");
            assert_eq!(customer_type, "defense");
            assert_eq!(reason, QcReportBlockReason::Incomplete);
            assert_eq!(blocked_id.as_deref(), Some(qcr_id.as_str()));
            assert_eq!(disp.as_deref(), Some("incomplete"));
        }
        other => panic!("expected Blocked(Incomplete), got {other:?}"),
    }

    // ── The positive control: close the gap, re-issue, gate passes. ──
    measure(&mut conn, &p2, &units[1].part_uid, 25.0);
    let (_, disposition2) = issue_report_for(&mut conn, &units);
    assert_eq!(disposition2, Disposition::Accept);

    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "a complete, accepted report must release the shipment"
    );
}

/// No report at all ⇒ blocked with `NoIssuedReport`. This is the arm that
/// stops a Defense shipment simply skipping the QC step.
#[test]
fn a_defense_shipment_with_no_issued_report_is_blocked() {
    let db = setup();
    let conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    mark_units(&conn, "wo-def", 1);
    seed_plan(&conn, "Bore D", "1", true);

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked { reason, qcr_id, .. } => {
            assert_eq!(reason, QcReportBlockReason::NoIssuedReport);
            assert_eq!(qcr_id, None);
        }
        other => panic!("expected Blocked(NoIssuedReport), got {other:?}"),
    }
}

/// A DRAFTED report does not release a shipment. A draft is not evidence.
#[test]
fn a_drafted_report_does_not_release_the_shipment() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure(&mut conn, &p1, &units[0].part_uid, 25.0);

    // Freeze only — do NOT issue.
    let m = meta();
    let plans = list_inspection_plans(&conn, T, Some("prd_bracket"), false).unwrap();
    let inspections = list_inspections_for_wo(&conn, T, "wo-def").unwrap();
    let tx = conn.transaction().unwrap();
    let (report, _) = freeze_report(
        &tx,
        &ctx(&m),
        FreezeReportInputs {
            report_kind: QcReportKind::DimensionalInspection,
            template: QcReportTemplate::AbenStandard,
            wo_id: "wo-def",
            product_id: "prd_bracket",
            partner_id: &buyer.id,
            plans: &plans,
            inspections: &inspections,
            units: &units,
            open_ncr_against_reported_part: false,
            traceability: ReportTraceability::default(),
            customer: ReportCustomer::default(),
            created_by: "ervin",
        },
        now(),
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        report.disposition,
        Disposition::Accept,
        "the content is fine…"
    );

    let d = dispatch(&conn, "dsp-def");
    assert!(
        matches!(
            resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
            QcReportGate::Blocked {
                reason: QcReportBlockReason::NoIssuedReport,
                ..
            }
        ),
        "…but an UNISSUED report is not evidence and must not release the shipment"
    );
}

/// A failing characteristic ⇒ `reject` ⇒ blocked with `Rejected`.
#[test]
fn a_rejected_report_blocks_the_shipment() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    // Way outside the ±0.05 band.
    measure(&mut conn, &p1, &units[0].part_uid, 26.5);

    let (_, disposition) = issue_report_for(&mut conn, &units);
    assert_eq!(disposition, Disposition::Reject);

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked { reason, .. } => {
            assert_eq!(reason, QcReportBlockReason::Rejected)
        }
        other => panic!("expected Blocked(Rejected), got {other:?}"),
    }
}

/// A stale-calibration measurement is not evidence of conformity, so it
/// blocks too — ADR-0199 §AC9 carried all the way to the shipment.
#[test]
fn a_stale_calibration_measurement_blocks_the_shipment() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);

    // Measured with a probe calibrated a year ago.
    let m = meta();
    let plan = aberp_qa::get_inspection_plan(&conn, T, &p1)
        .unwrap()
        .unwrap();
    let tx = conn.transaction().unwrap();
    record_inspection(
        &tx,
        &ctx(&m),
        RecordInspectionInputs {
            plan: &plan,
            source: QcSource::Probe,
            source_event_id: None,
            // An IN-TOLERANCE value: the block must come from the stale
            // calibration, not from the measurement being bad.
            actual_value: 25.0,
            units: "mm".into(),
            probe_serial: Some("RMP600-007".into()),
            last_calibration_at: Some(now() - time::Duration::days(365)),
            measured_at: now(),
            current_time: now(),
            stale_window_seconds: 86_400,
            linked_part_uid: Some(units[0].part_uid.clone()),
            linked_heat_lot: None,
            linked_wo_id: Some("wo-def".into()),
            recorded_by: "ervin".into(),
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let (_, disposition) = issue_report_for(&mut conn, &units);
    assert_eq!(
        disposition,
        Disposition::Incomplete,
        "an uncalibrated probe's reading is not evidence of conformity"
    );
    let d = dispatch(&conn, "dsp-def");
    assert!(matches!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Blocked {
            reason: QcReportBlockReason::Incomplete,
            ..
        }
    ));
}

// ═════════════════════════════════════════════════════════════════════
// The Pass exits — each one proven, so none of them is a silent hole.
// ═════════════════════════════════════════════════════════════════════

/// ADR-0199 §AC8 — **the commercial path is completely unaffected.** An
/// Industrial customer ships with no report and no 409, exactly as the two
/// sibling gates already assert.
#[test]
fn the_commercial_path_ships_with_no_report() {
    let db = setup();
    let conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Commercial Kft", CustomerType::Industrial),
    )
    .unwrap();
    seed_wo(&conn, "wo-com", "1");
    seed_dispatch(&conn, "dsp-com", "wo-com", &buyer.id);
    mark_units(&conn, "wo-com", 1);
    seed_plan(&conn, "Bore D", "1", true); // required, unmeasured — and still passes

    let d = dispatch(&conn, "dsp-com");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass
    );
}

/// Aerospace is gated alongside Defense — the two sibling gates use the
/// same pair and this one must not silently cover only half of it.
#[test]
fn aerospace_is_gated_like_defense() {
    let db = setup();
    let conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Aero Kft", CustomerType::Aerospace),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    mark_units(&conn, "wo-def", 1);
    seed_plan(&conn, "Bore D", "1", true);

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked { customer_type, .. } => assert_eq!(customer_type, "aerospace"),
        other => panic!("expected Blocked, got {other:?}"),
    }
}

/// **The Portable arm.** With the capability off the gate passes
/// unconditionally, on the exact fixture that blocks with it on. ADR-0199
/// §AC7 / §D9: a Portable build's shipment behaviour is byte-for-byte what
/// it was before this feature landed.
#[test]
fn the_portable_arm_passes_the_same_fixture_that_defense_blocks() {
    let db = setup();
    let conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    mark_units(&conn, "wo-def", 1);
    seed_plan(&conn, "Bore D", "1", true);

    let d = dispatch(&conn, "dsp-def");
    // Same fixture, both arms, one compile.
    assert!(matches!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Blocked { .. }
    ));
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_OFF).unwrap(),
        QcReportGate::Pass,
        "a Portable build must never refuse a shipment over QC reporting"
    );
}

/// A product with NO required characteristics passes: there is genuinely
/// no evidence to demand. This is the asymmetry that keeps the gate from
/// being a policy change disguised as a feature — but it must NOT extend
/// to a product that HAS characteristics (proven by the tests above).
#[test]
fn a_product_with_no_required_characteristics_passes() {
    let db = setup();
    let conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    mark_units(&conn, "wo-def", 1);
    // No plans at all.
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass
    );

    // …and an OPTIONAL-only product also passes.
    seed_plan(&conn, "Cosmetic", "9", false);
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass
    );
}

/// An unknown partner passes — nothing to gate on, matching both siblings.
#[test]
fn an_unknown_partner_passes() {
    let db = setup();
    let conn = Connection::open(&db).unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", "ptr_does_not_exist");
    seed_plan(&conn, "Bore D", "1", true);
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass
    );
}

/// A report bound to a DIFFERENT dispatch does not release this one. That
/// report released those parts, not these.
#[test]
fn a_report_bound_to_another_dispatch_does_not_release_this_one() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    seed_dispatch(&conn, "dsp-other", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure(&mut conn, &p1, &units[0].part_uid, 25.0);
    let (qcr_id, disposition) = issue_report_for(&mut conn, &units);
    assert_eq!(disposition, Disposition::Accept);

    // Bind it to the OTHER dispatch.
    let m = meta();
    let tx = conn.transaction().unwrap();
    aberp_qa::bind_reports_to_dispatch(&tx, &ctx(&m), "wo-def", "dsp-other").unwrap();
    tx.commit().unwrap();

    // dsp-other is released…
    let d_other = dispatch(&conn, "dsp-other");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d_other, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass
    );
    // …but dsp-def is not.
    let d_def = dispatch(&conn, "dsp-def");
    assert!(
        matches!(
            resolve_qc_report_gate_with_capability(&conn, T, &d_def, QC_REPORTING_ON).unwrap(),
            QcReportGate::Blocked {
                reason: QcReportBlockReason::NoIssuedReport,
                ..
            }
        ),
        "a report bound to another shipment released THOSE parts, not these: {qcr_id}"
    );
}

// ═════════════════════════════════════════════════════════════════════
// ADR-0199 §D7 / §AC3 — the SHA pin survives the shipment.
// ═════════════════════════════════════════════════════════════════════

/// **A report re-rendered AFTER it was bound to a dispatch still matches
/// the SHA pinned at issuance.**
///
/// This is the ordering trap the whole retention design has to survive:
/// the ADR-0199 §D6 gate requires the report to be ISSUED before the
/// shipment may proceed, so `qc_reports.dsp_id` is written strictly AFTER
/// the hash is taken. Any field that changes after issuance — `dsp_id`,
/// `state`, `superseded_by_qcr_id`, and the hash itself — must be
/// normalised out of the rendered form, or every correctly shipped report
/// reports itself as tampered the first time anyone downloads it.
///
/// `qc_report::canonical_for_render` is that normalisation, and this test
/// is what stops someone "restoring" one of those fields to the page.
#[test]
fn the_issued_sha_still_matches_after_the_report_is_bound_to_a_dispatch() {
    if !aberp::build_profile::qc_reporting_allowed() {
        return; // issuance + render are Defense-only; covered on that arm
    }
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure(&mut conn, &p1, &units[0].part_uid, 25.0);

    // Issue through the REAL app path, so the hash is minted the way
    // production mints it.
    let handle = aberp::serve::open_tenant_handle(&db, TenantId::new(T).unwrap()).unwrap();
    let hash = BinaryHash::from_bytes([0u8; 32]);
    let tenant = TenantId::new(T).unwrap();
    drop(conn);

    let drafted = aberp::qc_report::draft_report(
        &handle,
        tenant.clone(),
        hash,
        "ervin",
        now(),
        aberp::qc_report::DraftReportRequest {
            wo_id: "wo-def".into(),
            report_kind: QcReportKind::DimensionalInspection,
            template: None,
            notes: None,
        },
    )
    .expect("draft");
    assert_eq!(drafted.report.disposition, Disposition::Accept);

    let issued = aberp::qc_report::issue_report(
        &handle,
        tenant.clone(),
        hash,
        "ervin",
        now(),
        &drafted.report.qcr_id,
    )
    .expect("issue");
    let pinned = issued
        .report
        .rendered_sha256
        .clone()
        .expect("issuance pins a hash");

    // Re-render BEFORE binding — must already match.
    {
        let conn = handle.read().unwrap();
        let (_, _, sha, matches) =
            aberp::qc_report::render_report(&conn, T, &drafted.report.qcr_id).unwrap();
        assert_eq!(sha, pinned);
        assert_eq!(matches, Some(true), "a fresh re-render must match the pin");
    }

    // Bind it to the dispatch, exactly as `mark_shipped` does.
    {
        let mut guard = handle.write().unwrap();
        let m = meta();
        let tx = guard.transaction().unwrap();
        let bound = aberp_qa::bind_reports_to_dispatch(&tx, &ctx(&m), "wo-def", "dsp-def").unwrap();
        tx.commit().unwrap();
        assert_eq!(bound.len(), 1);
    }

    // …and re-render AFTER. The dsp_id is now set on the row.
    {
        let conn = handle.read().unwrap();
        let report = aberp_qa::get_report(&conn, T, &drafted.report.qcr_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            report.dsp_id.as_deref(),
            Some("dsp-def"),
            "precondition: the binding really did mutate the row"
        );
        let (_, _, sha, matches) =
            aberp::qc_report::render_report(&conn, T, &drafted.report.qcr_id).unwrap();
        assert_eq!(
            sha, pinned,
            "the post-shipment re-render must reproduce the ISSUED bytes — a \
             divergence here means a post-issuance field leaked into the hashed form"
        );
        assert_eq!(matches, Some(true));
    }
}

/// A DRAFT has no pinned hash, so its re-render reports `None` — not
/// `false`. Conflating "nothing to compare" with "did not match" would
/// train an operator to ignore the tamper signal.
#[test]
fn a_draft_render_reports_no_pin_rather_than_a_mismatch() {
    if !aberp::build_profile::qc_reporting_allowed() {
        return;
    }
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure(&mut conn, &p1, &units[0].part_uid, 25.0);
    drop(conn);

    let handle = aberp::serve::open_tenant_handle(&db, TenantId::new(T).unwrap()).unwrap();
    let drafted = aberp::qc_report::draft_report(
        &handle,
        TenantId::new(T).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
        "ervin",
        now(),
        aberp::qc_report::DraftReportRequest {
            wo_id: "wo-def".into(),
            report_kind: QcReportKind::DimensionalInspection,
            template: None,
            notes: None,
        },
    )
    .expect("draft");

    let conn = handle.read().unwrap();
    let (_, bytes, _, matches) =
        aberp::qc_report::render_report(&conn, T, &drafted.report.qcr_id).unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "a draft still previews");
    assert_eq!(matches, None, "a draft has no pin to match");
}

// ═════════════════════════════════════════════════════════════════════
// ROUND 2 — a REJECTED part must not ship (the reason the gate exists).
// ═════════════════════════════════════════════════════════════════════

/// **A stale `accept` must NOT outrank a current `reject`.**
///
/// The gate used to release the shipment if ANY issued unbound report
/// permitted it (`releasing.iter().any(..)`), written for the supersede
/// case — a corrected report must not stay blocked by the predecessor it
/// fixed. But `any` is symmetric, and this is the other direction: an
/// in-tolerance early measurement is issued as `accept`, the part then
/// FAILS final inspection and a second report is issued as `reject`, and
/// the shipment went out SHIPPED with both reports bound and no 409.
///
/// The fix is that the CURRENT report decides. Note what makes this test
/// sharp: both reports share a `created_at` (the fixture clock is fixed),
/// so "current" is decided by `report_number`, not by the timestamp and
/// not by ULID luck.
#[test]
fn a_stale_accept_does_not_outrank_the_current_reject() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);

    // In-process check: in tolerance ⇒ the first report accepts.
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    let (accept_id, d1) = issue_report_for(&mut conn, &units);
    assert_eq!(
        d1,
        Disposition::Accept,
        "precondition: the early report releases"
    );

    // Final inspection, an hour later: 99.0 against 25.0 ± 0.05.
    measure_at(
        &mut conn,
        &p1,
        &units[0].part_uid,
        99.0,
        now() + time::Duration::hours(1),
    );
    let (reject_id, d2) = issue_report_for(&mut conn, &units);
    assert_eq!(d2, Disposition::Reject, "precondition: the part failed");
    assert_ne!(accept_id, reject_id);

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked {
            reason,
            qcr_id,
            disposition,
            ..
        } => {
            assert_eq!(
                reason,
                QcReportBlockReason::Rejected,
                "a part that failed final inspection must not ship"
            );
            assert_eq!(
                qcr_id.as_deref(),
                Some(reject_id.as_str()),
                "the gate must name the CURRENT report, not the flattering one"
            );
            assert_eq!(disposition.as_deref(), Some("reject"));
        }
        other => panic!("a rejected part shipped: {other:?}"),
    }
}

/// The supersede case the `any()` was written for, kept explicit: a
/// rejected report that is SUPERSEDED by a corrected one leaves
/// `releasing`, so the corrected report is both the newest and the only
/// candidate — and the shipment is released.
///
/// Without this, "the current report decides" could be satisfied by a gate
/// that simply never releases once anything was ever rejected.
#[test]
fn a_superseded_rejection_does_not_block_the_corrected_report() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);

    measure_at(&mut conn, &p1, &units[0].part_uid, 99.0, now());
    let (bad_id, d1) = issue_report_for(&mut conn, &units);
    assert_eq!(d1, Disposition::Reject);

    // Rework, re-measure, re-issue.
    measure_at(
        &mut conn,
        &p1,
        &units[0].part_uid,
        25.0,
        now() + time::Duration::hours(1),
    );
    let (good_id, d2) = issue_report_for(&mut conn, &units);
    assert_eq!(d2, Disposition::Accept);

    // Supersede the rejection, as the correction workflow does.
    let m = meta();
    let tx = conn.transaction().unwrap();
    aberp_qa::void_report(&tx, &ctx(&m), &bad_id, "reworked", Some(&good_id)).unwrap();
    tx.commit().unwrap();

    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "a corrected report must release the shipment its predecessor blocked"
    );
}

// ═════════════════════════════════════════════════════════════════════
// ROUND 3 — the two bypasses that still let a bad part ship.
// ═════════════════════════════════════════════════════════════════════

/// **B1-a — an OPTIONAL characteristic PROMOTED to required after issuance
/// re-blocks the shipment.**
///
/// The bypass: `freeze_report` writes a line for EVERY enabled plan,
/// optional ones included. An unmeasured optional characteristic gets an
/// `Accountability::NotMeasured` accountability row — printed with a blank
/// actual, deliberately never omitted — and, being optional, it does not
/// count as `unaccounted`, so the report still issues as `accept`. The
/// drift check then compared a bare name-SET and saw that blank row as
/// coverage. Promote the characteristic to required via
/// `PUT /api/inspection-plans/:id` and the gate passed a shipment over a
/// required characteristic that was never measured at all.
///
/// The fix excludes `NotMeasured` lines from `covered`. Note what makes
/// this test sharp: the promoted characteristic is NEVER measured, so
/// nothing but the accountability row can make it look covered — the
/// assertion cannot pass because a measurement happened to exist.
///
/// Its sibling `a_required_characteristic_added_after_issuance_re_blocks_the_shipment`
/// covers the ADDED case; this one covers the PROMOTED case, which is the
/// one the round-2 review recorded as out of reach.
#[test]
fn an_optional_characteristic_promoted_to_required_re_blocks_the_shipment() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);

    // One required characteristic, measured in tolerance …
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    // … and one OPTIONAL characteristic, never measured.
    let p2 = seed_plan(&conn, "Face Z", "2", false);

    let (qcr_id, d1) = issue_report_for(&mut conn, &units);
    assert_eq!(
        d1,
        Disposition::Accept,
        "precondition: an unmeasured OPTIONAL characteristic does not make the \
         report incomplete — that is what makes this the promotion case and not \
         the incomplete case"
    );

    // The accountability row really is there and really is unmeasured —
    // otherwise the bypass this test pins would not exist to close.
    let lines = aberp_qa::list_report_lines(&conn, T, &qcr_id).unwrap();
    let face_z = lines
        .iter()
        .find(|l| l.characteristic_name == "Face Z")
        .expect("the optional characteristic got a frozen line");
    assert_eq!(
        face_z.accountability,
        aberp_qa::Accountability::NotMeasured,
        "precondition: the optional line is an unmeasured accountability row"
    );

    // Positive control: as things stand, it ships.
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "precondition: the report releases while Face Z is optional"
    );

    // Engineering PROMOTES the optional characteristic to required, after
    // the report froze. Nothing else about the plan changes.
    promote_plan_to_required(&conn, &p2, "Face Z", "2");
    let promoted = aberp_qa::get_inspection_plan(&conn, T, &p2)
        .unwrap()
        .unwrap();
    assert!(
        promoted.counts_toward_accountability(),
        "precondition: the promotion really did take"
    );

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked {
            reason, qcr_id: id, ..
        } => {
            assert_eq!(
                reason,
                QcReportBlockReason::PlanDrift,
                "a part with an unmeasured REQUIRED characteristic must not ship"
            );
            assert_eq!(id.as_deref(), Some(qcr_id.as_str()));
        }
        other => panic!("an unmeasured required characteristic shipped: {other:?}"),
    }

    // The counter-direction, so the fix is not just "block more": measure
    // the now-required characteristic, issue a fresh report, and the
    // shipment releases again. Without this, narrowing `covered` all the
    // way to the empty set would satisfy the assertion above.
    measure_at(&mut conn, &p2, &units[0].part_uid, 25.0, now());
    let (qcr2, d2) = issue_report_for(&mut conn, &units);
    assert_eq!(d2, Disposition::Accept, "both characteristics now measured");
    assert_ne!(qcr2, qcr_id);
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "a MEASURED characteristic stays covered — the block is caused by the \
         missing measurement, not by the gate simply refusing everything"
    );
}

/// **B1-a, per unit — a characteristic measured on SOME units is not
/// covered for the others.**
///
/// Found by the round-3 self-adversarial, on the fix as first written.
/// Excluding only the `NotMeasured` LINES is not enough, because lines are
/// PER UNIT: measure an optional characteristic on unit 1, leave unit 2
/// blank, and unit 1's surviving `Measured` line re-adds the name to
/// `covered`. Promote the characteristic and unit 2 ships with no
/// measurement for something now required — the same bypass, one unit over.
///
/// So a name is covered only when NO line for it is `NotMeasured`.
///
/// The sibling `an_optional_characteristic_promoted_to_required_re_blocks_the_shipment`
/// uses ONE unit, where the two rules coincide; only a second unit
/// separates them.
#[test]
fn a_characteristic_measured_on_only_some_units_is_not_covered_for_the_rest() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "2");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 2);

    // One required characteristic, measured on BOTH units.
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    measure_at(&mut conn, &p1, &units[1].part_uid, 25.0, now());

    // One OPTIONAL characteristic, measured on unit 1 only.
    let p2 = seed_plan(&conn, "Face Z", "2", false);
    measure_at(&mut conn, &p2, &units[0].part_uid, 25.0, now());

    let (qcr_id, d1) = issue_report_for(&mut conn, &units);
    assert_eq!(
        d1,
        Disposition::Accept,
        "precondition: an optional gap does not make the report incomplete"
    );

    // The shape that makes this test sharp: Face Z has BOTH a Measured and
    // a NotMeasured line. A rule that only drops the NotMeasured lines would
    // keep the name via the Measured one.
    let lines = aberp_qa::list_report_lines(&conn, T, &qcr_id).unwrap();
    let face_z: Vec<_> = lines
        .iter()
        .filter(|l| l.characteristic_name == "Face Z")
        .collect();
    assert_eq!(face_z.len(), 2, "one Face Z line per unit");
    assert!(
        face_z
            .iter()
            .any(|l| l.accountability == aberp_qa::Accountability::Measured)
            && face_z
                .iter()
                .any(|l| l.accountability == aberp_qa::Accountability::NotMeasured),
        "precondition: Face Z is measured on one unit and blank on the other"
    );

    // Positive control: it ships while Face Z is optional.
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "precondition: the report releases while Face Z is optional"
    );

    promote_plan_to_required(&conn, &p2, "Face Z", "2");

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked {
            reason, qcr_id: id, ..
        } => {
            assert_eq!(
                reason,
                QcReportBlockReason::PlanDrift,
                "unit 2 has no measurement for a now-required characteristic"
            );
            assert_eq!(id.as_deref(), Some(qcr_id.as_str()));
        }
        other => panic!("a partially measured characteristic released a shipment: {other:?}"),
    }

    // Counter-direction: measure the second unit, re-issue, and it releases.
    measure_at(&mut conn, &p2, &units[1].part_uid, 25.0, now());
    let (_, d2) = issue_report_for(&mut conn, &units);
    assert_eq!(d2, Disposition::Accept);
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "with every unit measured the characteristic is covered again — the block \\
         is caused by the missing unit, not by the gate refusing everything"
    );
}

/// **B1-b — a later `reject` outranks an earlier `accept` even when the
/// two timestamps differ only in a trimmed sub-second fraction.**
///
/// `created_at` is written by `time`'s `Rfc3339` formatter, which emits
/// sub-second digits only when non-zero and trims trailing zeros. So a
/// report frozen at `…12:00:00Z` and one frozen half a second LATER at
/// `…12:00:00.5Z` are stored with the first being a strict PREFIX of the
/// second, and a byte compare ranks `Z` (0x5A) above `.` (0x2E) — putting
/// the EARLIER report on top. The `report_number` tiebreak could not catch
/// it: that term is only consulted when the first terms compare EQUAL, and
/// these compare unequal-and-backwards.
///
/// The consequence is exactly what round 2 fixed and this re-opened: the
/// stale `accept` becomes `current`, and a part that FAILED final
/// inspection ships. The fix orders by the parsed instant.
///
/// `a_stale_accept_does_not_outrank_the_current_reject` is the same claim
/// with EQUAL timestamps (where `report_number` decides); this one is the
/// unequal-but-inverted case, which that test cannot reach.
#[test]
fn a_later_reject_outranks_an_earlier_accept_across_a_trimmed_subsecond() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);

    // Whole second — formats as `…12:00:00Z`, no fraction at all.
    let t_accept = now();
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, t_accept);
    let (accept_id, d1) = issue_report_for_at(&mut conn, &units, t_accept);
    assert_eq!(
        d1,
        Disposition::Accept,
        "precondition: the early report releases"
    );

    // Half a second LATER — formats as `…12:00:00.5Z`.
    let t_reject = t_accept + time::Duration::milliseconds(500);
    measure_at(&mut conn, &p1, &units[0].part_uid, 99.0, t_reject);
    let (reject_id, d2) = issue_report_for_at(&mut conn, &units, t_reject);
    assert_eq!(d2, Disposition::Reject, "precondition: the part failed");
    assert_ne!(accept_id, reject_id);

    // The trap, asserted directly: the stored strings really do invert
    // under a byte compare. If `time` ever stops trimming, this assertion
    // fails and tells the next reader the hazard is gone rather than
    // leaving a test that silently proves nothing.
    let a = aberp_qa::get_report(&conn, T, &accept_id).unwrap().unwrap();
    let r = aberp_qa::get_report(&conn, T, &reject_id).unwrap().unwrap();
    assert_eq!(a.created_at, "2026-08-23T12:00:00Z");
    assert_eq!(r.created_at, "2026-08-23T12:00:00.5Z");
    assert!(
        a.created_at.as_str() > r.created_at.as_str(),
        "precondition: as STRINGS the earlier accept sorts above the later reject \
         ({} vs {}) — that inversion is the bug",
        a.created_at,
        r.created_at
    );

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked {
            reason,
            qcr_id,
            disposition,
            ..
        } => {
            assert_eq!(
                reason,
                QcReportBlockReason::Rejected,
                "the part failed final inspection half a second after the accept"
            );
            assert_eq!(
                qcr_id.as_deref(),
                Some(reject_id.as_str()),
                "the gate must name the LATER report, not the flattering one"
            );
            assert_eq!(disposition.as_deref(), Some("reject"));
        }
        other => panic!("a rejected part shipped on a trimmed sub-second: {other:?}"),
    }
}

/// **The gate REFUSES rather than ranking a report it cannot date.**
///
/// The recency key parses `created_at` into an instant, and an unparseable
/// one yields `None` — which `Option`'s ordering sorts BELOW every `Some`.
/// Left alone that silently demotes the offending report, and demoting the
/// `reject` is exactly how a bad part ships. So the gate refuses the whole
/// decision instead.
///
/// The row is corrupted with direct SQL because nothing in the application
/// can produce it: every `created_at` is minted through `aberp_qa`'s one
/// `rfc3339` helper. That is the point — the refusal is a backstop against
/// a row written outside the application, and a backstop nobody tests is a
/// backstop that quietly stops working.
#[test]
fn the_gate_refuses_a_report_it_cannot_date_rather_than_demoting_it() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    let (qcr_id, d1) = issue_report_for(&mut conn, &units);
    assert_eq!(d1, Disposition::Accept);

    // Positive control: it ships while the timestamp is readable.
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "precondition: the report releases before the row is corrupted"
    );

    conn.execute(
        "UPDATE qc_reports SET created_at = 'not-a-timestamp'
         WHERE tenant_id = ?1 AND qcr_id = ?2",
        params![T, &qcr_id],
    )
    .unwrap();

    let d = dispatch(&conn, "dsp-def");
    let err = resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON)
        .expect_err("the gate must refuse, not decide, on an unreadable timestamp");
    let msg = err.to_string();
    assert!(
        msg.contains(&qcr_id) && msg.contains("created_at"),
        "the refusal names the report and the field: {msg}"
    );
}

/// **A required characteristic ADDED after an accept re-blocks the
/// shipment.** The releasing report enumerated two characteristics; the
/// plan now demands three, so its release covers evidence that was never
/// gathered.
///
/// `characteristics_required` on the header is a LINE count (one per
/// characteristic-unit pair), not a plan count, so the gate compares the
/// SET of characteristic names instead.
#[test]
fn a_required_characteristic_added_after_issuance_re_blocks_the_shipment() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    let (qcr_id, d1) = issue_report_for(&mut conn, &units);
    assert_eq!(d1, Disposition::Accept);

    // The positive control first: as things stand, it ships.
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "precondition: the report releases before the plan changes"
    );

    // Engineering adds a required characteristic AFTER the report froze.
    seed_plan(&conn, "Face Z", "2", true);

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked {
            reason, qcr_id: id, ..
        } => {
            assert_eq!(reason, QcReportBlockReason::PlanDrift);
            assert_eq!(id.as_deref(), Some(qcr_id.as_str()));
        }
        other => panic!("expected Blocked(PlanDrift), got {other:?}"),
    }

    // And an OPTIONAL addition does not: it never counted toward
    // accountability, so demanding it would block on nothing.
    let db2 = setup();
    let mut conn2 = Connection::open(&db2).unwrap();
    let buyer2 = create_partner(
        &conn2,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn2, "wo-def", "1");
    seed_dispatch(&conn2, "dsp-def", "wo-def", &buyer2.id);
    let units2 = mark_units(&conn2, "wo-def", 1);
    let q1 = seed_plan(&conn2, "Bore D", "1", true);
    measure_at(&mut conn2, &q1, &units2[0].part_uid, 25.0, now());
    issue_report_for(&mut conn2, &units2);
    seed_plan(&conn2, "Face Z", "2", false);
    let d2 = dispatch(&conn2, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn2, T, &d2, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "an OPTIONAL characteristic added later is not evidence anyone owes"
    );
}

// ═════════════════════════════════════════════════════════════════════
// ROUND 2 — the document must not make false statements about itself.
// ═════════════════════════════════════════════════════════════════════

/// Issue one accepted report through the REAL app path and hand back
/// `(handle, tenant, partner_id, qcr_id, pinned_sha)`.
fn issue_one_through_the_app(
    db: &std::path::Path,
) -> (aberp_db::HandleArc, TenantId, String, String, String) {
    let mut conn = Connection::open(db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    drop(conn);

    let tenant = TenantId::new(T).unwrap();
    let handle = aberp::serve::open_tenant_handle(db, tenant.clone()).unwrap();
    let hash = BinaryHash::from_bytes([0u8; 32]);
    let drafted = aberp::qc_report::draft_report(
        &handle,
        tenant.clone(),
        hash,
        "ervin",
        now(),
        aberp::qc_report::DraftReportRequest {
            wo_id: "wo-def".into(),
            report_kind: QcReportKind::DimensionalInspection,
            template: None,
            notes: None,
        },
    )
    .expect("draft");
    let issued = aberp::qc_report::issue_report(
        &handle,
        tenant.clone(),
        hash,
        "ervin",
        now(),
        &drafted.report.qcr_id,
    )
    .expect("issue");
    let sha = issued.report.rendered_sha256.clone().unwrap();
    (handle, tenant, buyer.id, issued.report.qcr_id, sha)
}

/// **Editing the customer AFTER issuance must not flip the report to
/// tampered.**
///
/// The identity block is printed inside the bytes whose SHA-256 is pinned
/// at issuance, and it used to be filled by joining `partners` LIVE at
/// both issuance and re-render. So a customer moving office — or
/// `soft_delete_partner` running — permanently flipped every one of their
/// issued reports to `matches_issued_sha256: false`: a tamper signal on an
/// untampered document, and unrecoverable, because the true bytes are
/// never stored. The identity is now a SNAPSHOT on `qc_reports`.
#[test]
fn a_partner_edit_after_issuance_leaves_the_issued_sha_intact() {
    if !aberp::build_profile::qc_reporting_allowed() {
        return; // issuance + render are Defense-only
    }
    let db = setup();
    let (handle, _tenant, partner_id, qcr_id, pinned) = issue_one_through_the_app(&db);

    // Precondition — the snapshot really carries the identity, so this
    // test cannot pass vacuously by rendering a blank customer block.
    {
        let conn = handle.read().unwrap();
        let report = aberp_qa::get_report(&conn, T, &qcr_id).unwrap().unwrap();
        assert_eq!(
            report.customer_name.as_deref(),
            Some("Prime Aero"),
            "the customer identity must be snapshotted at freeze"
        );
        let (_, bytes, sha, matches) = aberp::qc_report::render_report(&conn, T, &qcr_id).unwrap();
        assert_eq!(sha, pinned);
        assert_eq!(matches, Some(true));
        assert!(
            String::from_utf8_lossy(&bytes).contains("Prime Aero"),
            "the identity block must actually be on the page"
        );
    }

    // The customer moves, and is then removed from the address book.
    {
        let guard = handle.write().unwrap();
        // A name with NO substring overlap with the snapshot, so
        // "the old name is still on the page" cannot pass vacuously.
        let mut edited = partner_inputs("Nordwind Systems", CustomerType::Defense);
        edited.legal_name = "Nordwind Systems Zrt.".into();
        edited.address_city = Some("Debrecen".into());
        edited.address_street = Some("Uj utca 42.".into());
        aberp::partners::update_partner(&guard, T, &partner_id, &edited)
            .unwrap()
            .expect("the partner exists");
        assert!(aberp::partners::soft_delete_partner(&guard, T, &partner_id).unwrap());
    }

    let conn = handle.read().unwrap();
    let (_, bytes, sha, matches) = aberp::qc_report::render_report(&conn, T, &qcr_id).unwrap();
    assert_eq!(
        sha, pinned,
        "a customer-record edit must not change one byte of an issued document"
    );
    assert_eq!(matches, Some(true));
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("Prime Aero"),
        "the document keeps the identity it was ISSUED with, not today's"
    );
    assert!(
        !text.contains("Nordwind"),
        "today's partner row must not reach an already-issued document"
    );
}

/// **A VOIDED report is refused, not re-rendered as an ISSUED one.**
///
/// The page cannot carry `state` (it is post-issuance-mutable, and
/// printing it breaks the §D7 pin), and the old canonicalisation forced it
/// to `Issued` — so a voided report came out looking exactly like a valid
/// certificate of conformity, with no void marker anywhere and no route
/// refusing to serve it.
#[test]
fn a_voided_report_refuses_to_render() {
    if !aberp::build_profile::qc_reporting_allowed() {
        return;
    }
    let db = setup();
    let (handle, _tenant, _partner_id, qcr_id, _pinned) = issue_one_through_the_app(&db);

    // Positive control: it renders while it is valid.
    {
        let conn = handle.read().unwrap();
        assert!(aberp::qc_report::render_report(&conn, T, &qcr_id).is_ok());
    }

    {
        let mut guard = handle.write().unwrap();
        let m = meta();
        let tx = guard.transaction().unwrap();
        aberp_qa::void_report(&tx, &ctx(&m), &qcr_id, "issued in error", None).unwrap();
        tx.commit().unwrap();
    }

    let conn = handle.read().unwrap();
    match aberp::qc_report::render_report(&conn, T, &qcr_id) {
        Err(aberp::qc_report::QcReportError::NotCurrent(msg)) => {
            assert!(msg.contains(&qcr_id), "the refusal names the report: {msg}");
            assert!(
                msg.contains("voided"),
                "the refusal says WHICH way it stopped being current: {msg}"
            );
        }
        Ok((_, bytes, _, _)) => panic!(
            "a voided report rendered {} bytes of valid-looking certificate",
            bytes.len()
        ),
        Err(other) => panic!("expected NotCurrent, got {other:?}"),
    }
}

/// **A SUPERSEDED report is refused too, on the same footing as a void.**
///
/// Round 3's residual, decided conservatively. `void_report` picks
/// `Superseded` over `Voided` purely from whether the caller supplied a
/// `superseded_by_qcr_id` — same route, same meaning to a reader: this is
/// not the document that stands. And because the page cannot carry `state`,
/// the superseded report re-rendered as a clean, unmarked certificate.
///
/// It is the sharper hazard of the two. The report a supersede replaces is
/// typically the FLATTERING one — the early `accept` that a later `reject`
/// corrected — which is exactly the pair
/// `a_stale_accept_does_not_outrank_the_current_reject` stops the gate from
/// shipping on. Serving its PDF handed an auditor the very document the
/// gate refused to ship on.
///
/// The §D7 byte-form invariant this test used to also carry — that a state
/// transition perturbs no byte — is NOT dropped; it moved to
/// `qc_report::tests::render_canonical_is_byte_identical_across_a_supersede`,
/// which asserts it at the layer it is about instead of through the route
/// that now refuses.
#[test]
fn a_superseded_report_refuses_to_render() {
    if !aberp::build_profile::qc_reporting_allowed() {
        return;
    }
    let db = setup();
    let (handle, _tenant, _partner_id, qcr_id, _pinned) = issue_one_through_the_app(&db);

    // Positive control: it renders while it is the current document.
    {
        let conn = handle.read().unwrap();
        assert!(aberp::qc_report::render_report(&conn, T, &qcr_id).is_ok());
    }

    {
        let mut guard = handle.write().unwrap();
        let m = meta();
        let tx = guard.transaction().unwrap();
        aberp_qa::void_report(&tx, &ctx(&m), &qcr_id, "reworked", Some("qcr_later")).unwrap();
        tx.commit().unwrap();
    }

    let conn = handle.read().unwrap();
    // Precondition: the row really moved to `Superseded`, not `Voided` —
    // otherwise this test would be re-asserting the void case.
    assert_eq!(
        aberp_qa::get_report(&conn, T, &qcr_id)
            .unwrap()
            .unwrap()
            .state,
        aberp_qa::QcReportState::Superseded,
        "precondition: the supersede really did move the state"
    );
    match aberp::qc_report::render_report(&conn, T, &qcr_id) {
        Err(aberp::qc_report::QcReportError::NotCurrent(msg)) => {
            assert!(msg.contains(&qcr_id), "the refusal names the report: {msg}");
            assert!(
                msg.contains("superseded") && msg.contains("qcr_later"),
                "the refusal says it was superseded and by which report: {msg}"
            );
        }
        Ok((_, bytes, _, _)) => panic!(
            "a superseded report rendered {} bytes of clean, unmarked certificate",
            bytes.len()
        ),
        Err(other) => panic!("expected NotCurrent, got {other:?}"),
    }

    // The belt on the braces: `state` and `superseded_by_qcr_id` are written
    // by ONE statement, so a row where `state` says `issued` while a
    // replacement id is present cannot come from the application. If one ever
    // does, the pointer to the replacement is the honest signal — the refusal
    // must still fire on it rather than trust the stale `state`.
    drop(conn);
    {
        let guard = handle.write().unwrap();
        guard
            .execute(
                "UPDATE qc_reports SET state = 'issued'
                 WHERE tenant_id = ?1 AND qcr_id = ?2",
                params![T, &qcr_id],
            )
            .unwrap();
    }
    let conn = handle.read().unwrap();
    let row = aberp_qa::get_report(&conn, T, &qcr_id).unwrap().unwrap();
    assert_eq!(
        row.state,
        aberp_qa::QcReportState::Issued,
        "precondition: the row now disagrees with itself"
    );
    assert_eq!(row.superseded_by_qcr_id.as_deref(), Some("qcr_later"));
    match aberp::qc_report::render_report(&conn, T, &qcr_id) {
        Err(aberp::qc_report::QcReportError::NotCurrent(msg)) => {
            assert!(msg.contains("superseded"), "{msg}");
        }
        Ok((_, bytes, _, _)) => panic!(
            "a row naming its own replacement rendered {} bytes as a current document",
            bytes.len()
        ),
        Err(other) => panic!("expected NotCurrent, got {other:?}"),
    }
}

/// **An issued PDF does not stamp itself `(draft — not issued)`, and does
/// cite its audit-chain entry.**
///
/// Both were consequences of the canonical render form: `rendered_sha256`
/// is normalised to `None` (a document cannot contain its own hash), so
/// the footer's "Issued SHA-256" line printed the draft placeholder on
/// EVERY issued document; and both call sites passed `chain_reference:
/// ""`, so the CoC's fixed statement — "the tamper-evident audit chain
/// entry cited below" — sat directly above `Audit chain ref: —`.
#[test]
fn an_issued_pdf_cites_its_chain_entry_and_is_not_stamped_draft() {
    if !aberp::build_profile::qc_reporting_allowed() {
        return;
    }
    let db = setup();
    let (handle, _tenant, _partner_id, qcr_id, _pinned) = issue_one_through_the_app(&db);
    let conn = handle.read().unwrap();
    let (_, bytes, _, _) = aberp::qc_report::render_report(&conn, T, &qcr_id).unwrap();
    let text = String::from_utf8_lossy(&bytes);

    assert!(
        !text.contains("draft — not issued") && !text.contains("DRAFT — NOT ISSUED"),
        "an ISSUED document must not describe itself as a draft"
    );
    assert!(
        !text.contains("Issued SHA-256"),
        "a document cannot contain its own hash; the pin is served out-of-band"
    );
    let expected_ref = aberp_qa::issuance_chain_ref(&qcr_id);
    assert!(
        text.contains(&expected_ref),
        "the page must cite the real chain entry ({expected_ref}), not a dash"
    );
    assert!(
        !text.contains("SUPERSEDED") && !text.contains("VOIDED"),
        "`state` is post-issuance-mutable and must not be on the page"
    );

    // The `state`-off-the-page claim above is what makes the round-3
    // refusal necessary rather than optional: with no state on the page,
    // a superseded report is indistinguishable from a current one, so the
    // route has to refuse it. That refusal is pinned by
    // `a_superseded_report_refuses_to_render`; the §D7 byte-form invariant
    // it displaces — a state transition perturbs no byte — is pinned by
    // `qc_report::tests::render_canonical_is_byte_identical_across_a_supersede`.
    // Neither claim is dropped; they are asserted at the layer each is about.
    drop(conn);
    let _ = bytes;
}

/// The other direction: a DRAFT preview says so. Without this, dropping
/// `state` from the header could be satisfied by a renderer that never
/// distinguishes a preview from a certificate.
#[test]
fn a_draft_pdf_says_it_is_a_draft() {
    if !aberp::build_profile::qc_reporting_allowed() {
        return;
    }
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    drop(conn);

    let tenant = TenantId::new(T).unwrap();
    let handle = aberp::serve::open_tenant_handle(&db, tenant.clone()).unwrap();
    let drafted = aberp::qc_report::draft_report(
        &handle,
        tenant,
        BinaryHash::from_bytes([0u8; 32]),
        "ervin",
        now(),
        aberp::qc_report::DraftReportRequest {
            wo_id: "wo-def".into(),
            report_kind: QcReportKind::DimensionalInspection,
            template: None,
            notes: None,
        },
    )
    .expect("draft");

    let conn = handle.read().unwrap();
    let (_, bytes, _, _) =
        aberp::qc_report::render_report(&conn, T, &drafted.report.qcr_id).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("DRAFT"),
        "a preview must be visibly a preview"
    );
    assert!(
        !text.contains(&aberp_qa::issuance_chain_ref(&drafted.report.qcr_id)),
        "a draft has no issuance entry to cite"
    );
}

// ═════════════════════════════════════════════════════════════════════
// Round 4 — the two ways a bad part still shipped through a gate that
// had already stopped every characteristic-level bypass.
// ═════════════════════════════════════════════════════════════════════

/// **B-1 — a second active plan under the same STORED name collapses the
/// gate's name-keyed join, and an unmeasured REQUIRED characteristic
/// ships.**
///
/// `create_plan` / `update_plan` persist `feature_name.trim()`, but
/// `ensure_unique` used to query on the RAW operator input. So `" Bore D "`
/// matched no existing row (the stored value is `"Bore D"`), passed the
/// uniqueness check, and was then written as `"Bore D"` — two ACTIVE plans,
/// one stored key.
///
/// That is a shipment bug, not a tidiness bug. The drift check builds
/// `required_now` as a `BTreeSet` of trimmed plan names and asks whether
/// every one is covered by the report's frozen lines. Two plans sharing a
/// stored name collapse to ONE element, so plan 1's measurement covers plan
/// 2's name — and plan 2, a required characteristic nobody ever measured,
/// rides out on it.
///
/// **The mutation:** revert either `.trim()` in `ensure_unique`. The
/// duplicate is then accepted, the `Err` arm below is never taken, and the
/// `Ok` arm asserts the escape it buys before failing.
#[test]
fn a_padded_duplicate_plan_name_cannot_collapse_the_gates_join() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 1);

    // One required characteristic, measured in tolerance. This is the plan
    // whose measurement the duplicate would hide behind.
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());

    let (qcr_id, d1) = issue_report_for(&mut conn, &units);
    assert_eq!(d1, Disposition::Accept, "precondition: the report releases");
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "precondition: the shipment is released before the duplicate exists"
    );

    // Engineering adds a SECOND characteristic — balloon 7, required, never
    // measured — and types the name with the surrounding whitespace a
    // copy-paste from a drawing carries.
    let dup = create_inspection_plan(
        &conn,
        T,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "  Bore D  ".into(),
            nominal_value: 25.0,
            upper_tol: 0.05,
            lower_tol: -0.05,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("7".into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: None,
            is_required: Some(true),
        },
    );

    match dup {
        Err(aberp_qa::QcError::Validation(msg)) => {
            assert!(
                msg.contains("Bore D"),
                "the refusal must name the colliding feature: {msg}"
            );
        }
        Ok(second) => {
            // The fix is reverted. Show exactly what that buys before
            // failing, so the mutation report reads as a shipment escape
            // rather than as a uniqueness nit.
            let stored: Vec<String> = list_inspection_plans(&conn, T, Some("prd_bracket"), false)
                .unwrap()
                .into_iter()
                .map(|p| p.feature_name)
                .collect();
            assert_eq!(
                stored,
                vec!["Bore D".to_string(), "Bore D".to_string()],
                "the WRITE trims, so both plans landed under one stored name"
            );
            let d = dispatch(&conn, "dsp-def");
            let gate =
                resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap();
            assert_eq!(
                gate,
                QcReportGate::Pass,
                "…and the collapsed join released the shipment"
            );
            panic!(
                "ensure_unique compared the RAW feature_name: plan {} was accepted under \
                 the same STORED name as {p1}, collapsing the gate's name-keyed join — a \
                 required characteristic that was never measured shipped on report {qcr_id}",
                second.plan_id
            );
        }
        Err(other) => panic!("expected a Validation refusal, got {other:?}"),
    }

    // The PRODUCT id is the other half of the same key and is trimmed by the
    // same writes, so it carries the same collapse: a plan filed under
    // `" prd_bracket "` is stored as `prd_bracket` and lands inside the very
    // set `required_now` is built from (`list_inspection_plans` filters on
    // the trimmed, stored value).
    let dup_product = create_inspection_plan(
        &conn,
        T,
        NewInspectionPlan {
            product_id: "  prd_bracket  ".into(),
            feature_name: "Bore D".into(),
            nominal_value: 25.0,
            upper_tol: 0.05,
            lower_tol: -0.05,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("8".into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: None,
            is_required: Some(true),
        },
    );
    assert!(
        matches!(dup_product, Err(aberp_qa::QcError::Validation(_))),
        "a padded PRODUCT id collapses onto the same stored key and must be          refused too, got {dup_product:?}"
    );

    // ── Counter-direction 1: the fix must not make a plan collide with
    // ITSELF. Editing plan 1 and re-typing its own name with padding goes
    // through the same check with `exclude_plan_id = Some(p1)`, so it must
    // still succeed — and must leave the release untouched, because the
    // stored name did not change.
    promote_plan_to_required(&conn, &p1, "  Bore D  ", "1");
    let after = aberp_qa::get_inspection_plan(&conn, T, &p1)
        .unwrap()
        .unwrap();
    assert_eq!(
        after.feature_name, "Bore D",
        "the edit stored the trimmed name, as it always did"
    );
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "a plan re-saved under its own name must not block its own shipment"
    );

    // ── Counter-direction 2: a genuinely DIFFERENT required characteristic
    // is still accepted, and still blocks. Without this, refusing every
    // second plan outright would satisfy the assertions above.
    let p2 = seed_plan(&conn, "Face Z", "7", true);
    assert_ne!(p2, p1);
    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked { reason, .. } => assert_eq!(
            reason,
            QcReportBlockReason::PlanDrift,
            "a distinct, unmeasured required characteristic still blocks"
        ),
        other => panic!("a new required characteristic did not block: {other:?}"),
    }
}

/// **B-2 — a report frozen BEFORE the parts were marked releases units it
/// enumerates none of.**
///
/// `resolve_context` resolves the report's units from `wo_part_marks` once,
/// at draft time. With no marks yet, `build_report_lines` degrades every
/// characteristic to ONE lot-level line — matched against ANY measurement of
/// that characteristic — so the report comes out a clean `accept` with
/// `serial_range = None` and `qty_reported = 1`. Nothing re-checked that
/// snapshot afterwards, and the characteristic-coverage join is name-keyed,
/// so marking N parts and shipping passed the gate on a document that names
/// none of them.
///
/// **The mutation:** delete the `serial_range` comparison in
/// `resolve_qc_report_gate_with_capability`. The gate then returns `Pass`
/// after the marking and the `Blocked` assertion goes red.
#[test]
fn a_report_frozen_before_part_marking_does_not_release_the_marked_units() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "2");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    // Deliberately NO marks yet — the parts are still on the machine.
    ensure_part_schema(&conn).unwrap();

    let p1 = seed_plan(&conn, "Bore D", "1", true);
    // A first-article measurement, taken before any part carries a UID.
    measure_lot_level(&mut conn, &p1, 25.0);

    let (qcr_id, d1) = issue_report_for(&mut conn, &[]);
    assert_eq!(
        d1,
        Disposition::Accept,
        "precondition: with no units, every characteristic reports once at lot \
         level and the report is a clean accept"
    );
    let frozen = aberp_qa::get_report(&conn, T, &qcr_id).unwrap().unwrap();
    assert_eq!(
        frozen.serial_range, None,
        "precondition: the report enumerates NO serials"
    );
    assert_eq!(
        frozen.qty_reported, 1,
        "precondition: and accounts for one lot-level document"
    );

    // Positive control: while nothing is marked, this report legitimately
    // releases — an unserialised lot is exactly what it documents.
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "precondition: the block below must be caused by the MARKING, not by \
         the report being unserialised"
    );

    // The parts come off the machine and get marked: two serialised units
    // the issued report has never heard of.
    let units = mark_units(&conn, "wo-def", 2);
    assert_eq!(units.len(), 2);

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked {
            reason,
            qcr_id: id,
            disposition,
            ..
        } => {
            assert_eq!(
                reason,
                QcReportBlockReason::UnitDrift,
                "two serialised units must not ship on a report that enumerates none"
            );
            assert_eq!(id.as_deref(), Some(qcr_id.as_str()));
            assert_eq!(
                disposition.as_deref(),
                Some("accept"),
                "the audit payload records that the refusal overrode a RELEASING \
                 report — that is the whole point of the finding"
            );
        }
        other => panic!("two unenumerated units shipped: {other:?}"),
    }

    // Counter-direction: measure both marked units, issue a fresh report
    // that DOES enumerate them, and the shipment releases again. Without
    // this, hard-coding the block would satisfy the assertion above.
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    measure_at(&mut conn, &p1, &units[1].part_uid, 25.0, now());
    let (qcr2, d2) = issue_report_for(&mut conn, &units);
    assert_ne!(qcr2, qcr_id);
    assert_eq!(d2, Disposition::Accept, "both units measured");
    let reissued = aberp_qa::get_report(&conn, T, &qcr2).unwrap().unwrap();
    assert_eq!(
        reissued.serial_range.as_deref(),
        Some("SN-001 … SN-002 (2 units)"),
        "the fresh report enumerates the units it releases"
    );
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "a report whose unit scope matches the marks releases — the block is \
         caused by the drift, not by the gate refusing everything"
    );
}

/// **B-2, the backstop arm — a report that enumerates SOME of the marked
/// units does not release the rest.**
///
/// The same defect one unit over, and the reason the check is written as
/// scope EQUALITY rather than as "marks exist and the report enumerates
/// none": the characteristic-coverage join is name-keyed, so a report frozen
/// over SN-001…SN-002 covers `Bore D` for a later SN-003 that nobody
/// measured.
///
/// This arm is NOT reachable through the application: `record_part_marks`
/// refuses once a WO has any mark (`PartMarkError::AlreadyMarked`), so the
/// extra mark has to be written directly — the same posture as
/// `refuse_unparseable_report_timestamps`, which also only an out-of-band
/// write can reach. It is pinned anyway so the equality form is not a
/// silently untested widening.
///
/// **The mutation:** narrow the check back to
/// `current.serial_range.is_none() && !marks_now.is_empty()`. The extra unit
/// then ships.
#[test]
fn a_report_covering_only_some_marked_units_does_not_release_the_rest() {
    let db = setup();
    let mut conn = Connection::open(&db).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Prime Aero", CustomerType::Defense),
    )
    .unwrap();
    seed_wo(&conn, "wo-def", "3");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id);
    let units = mark_units(&conn, "wo-def", 2);

    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure_at(&mut conn, &p1, &units[0].part_uid, 25.0, now());
    measure_at(&mut conn, &p1, &units[1].part_uid, 25.0, now());

    let (qcr_id, d1) = issue_report_for(&mut conn, &units);
    assert_eq!(d1, Disposition::Accept);
    let d = dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap(),
        QcReportGate::Pass,
        "precondition: the report covers exactly the two marked units"
    );

    // A third unit appears in `wo_part_marks`. The marking ROUTE refuses
    // this (`AlreadyMarked`), so it is written directly — which is the only
    // way this arm is reachable at all.
    let extra_uid = generate_part_uid();
    conn.execute(
        "INSERT INTO wo_part_marks (
            tenant_id, wo_id, unit_index, part_uid, serial_number,
            data_matrix_payload, heat_lot_reference, marked_at_utc, marked_by_operator
         ) VALUES (?1, 'wo-def', 3, ?2, 'SN-003', ?3, 'HL-9911', '2026-08-02T00:00:00Z', 'op')",
        params![
            T,
            &extra_uid,
            data_matrix_payload(&extra_uid, "SN-003", None)
        ],
    )
    .unwrap();

    let d = dispatch(&conn, "dsp-def");
    match resolve_qc_report_gate_with_capability(&conn, T, &d, QC_REPORTING_ON).unwrap() {
        QcReportGate::Blocked {
            reason, qcr_id: id, ..
        } => {
            assert_eq!(
                reason,
                QcReportBlockReason::UnitDrift,
                "a third unit the report never enumerated must not ship on it"
            );
            assert_eq!(id.as_deref(), Some(qcr_id.as_str()));
        }
        other => panic!("an unenumerated third unit shipped: {other:?}"),
    }
}
