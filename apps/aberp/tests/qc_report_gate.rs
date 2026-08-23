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
    QcSource, QcWriteContext, RecordInspectionInputs, ReportTraceability, ReportUnit,
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

/// Freeze + issue a report for `wo-def`, returning `(qcr_id, disposition)`.
fn issue_report_for(conn: &mut Connection, units: &[ReportUnit]) -> (String, Disposition) {
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
            created_by: "ervin",
        },
        now(),
    )
    .unwrap();
    let issued = issue_report(
        &tx,
        &ctx(&m),
        &report.qcr_id,
        "deadbeef",
        "aberp-qc-pdf@0.0.0",
        "ervin",
        now(),
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
