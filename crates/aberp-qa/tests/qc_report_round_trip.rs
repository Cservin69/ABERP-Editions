//! ADR-0199 — integration tests for the QC-report write path against a
//! fresh in-memory DuckDB.
//!
//! Pins the things the pure unit tests in `qc::reports` cannot reach:
//! that the computed lines actually survive a write/read cycle, that
//! issuance pins the SHA and emits the chain entries, that binding to a
//! dispatch is one transaction, and — the load-bearing one — that a
//! frozen report is IMMUNE to later edits of the plan it was computed
//! from (ADR-0199 §AC4).

use aberp_audit_ledger::{
    ensure_schema as ensure_audit_schema, Actor, BinaryHash, EventKind, LedgerMeta, TenantId,
};
use aberp_inventory::ActorKind;
use aberp_qa::{
    archive_inspection_plan, bind_reports_to_dispatch, create_inspection_plan, current_drawing_ref,
    ensure_schema as ensure_qa_schema, freeze_report, get_report, issue_report, list_drawing_refs,
    list_inspection_plans, list_report_lines, list_reports_for_dispatch, record_drawing_ref,
    record_inspection, update_inspection_plan, Accountability, CharacteristicType, Disposition,
    FreezeReportInputs, InspectionMethod, NewInspectionPlan, NewPartDrawingRef, QcReportKind,
    QcReportState, QcReportTemplate, QcSource, QcWriteContext, RecordInspectionInputs,
    ReportCustomer, ReportTraceability, ReportUnit,
};
use duckdb::Connection;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const TEST_TENANT: &str = "ten_test_qcr";

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

fn ctx<'a>(meta: &'a LedgerMeta, login: &str) -> QcWriteContext<'a> {
    QcWriteContext {
        tenant: TEST_TENANT,
        actor: ActorKind::SpaOperator {
            operator_login: login.to_string(),
        },
        ledger_meta: meta,
        ledger_actor: Actor::from_local_cli("qcr-test-session".to_string(), login),
    }
}

fn t(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap()
}

fn now() -> OffsetDateTime {
    t("2026-08-23T12:00:00Z")
}

fn count_kind(conn: &Connection, kind: EventKind) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?;",
        duckdb::params![kind.as_str()],
        |r| r.get(0),
    )
    .unwrap()
}

fn seed_plan(conn: &Connection, feature: &str, number: &str, required: bool) -> String {
    create_inspection_plan(
        conn,
        TEST_TENANT,
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
            characteristic_designator: Some(aberp_qa::CharacteristicDesignator::Key),
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: Some("1/B4".into()),
            is_required: Some(required),
        },
    )
    .unwrap()
    .plan_id
}

/// Record one measurement through the EXISTING ADR-0092 chokepoint — the
/// report layer never invents a second measurement path.
fn measure(conn: &mut Connection, plan_id: &str, part_uid: &str, actual: f64) {
    let m = meta();
    let plan = aberp_qa::get_inspection_plan(conn, TEST_TENANT, plan_id)
        .unwrap()
        .unwrap();
    let tx = conn.transaction().unwrap();
    record_inspection(
        &tx,
        &ctx(&m, "ervin"),
        RecordInspectionInputs {
            plan: &plan,
            source: QcSource::Manual,
            source_event_id: None,
            actual_value: actual,
            units: "mm".into(),
            probe_serial: Some("RMP600-007".into()),
            last_calibration_at: None,
            measured_at: now(),
            current_time: now(),
            stale_window_seconds: 86_400,
            linked_part_uid: Some(part_uid.into()),
            linked_heat_lot: Some("HL-9911".into()),
            linked_wo_id: Some("wo_1".into()),
            recorded_by: "ervin".into(),
        },
    )
    .unwrap();
    tx.commit().unwrap();
}

fn freeze(
    conn: &mut Connection,
    units: &[ReportUnit],
    kind: QcReportKind,
    template: QcReportTemplate,
) -> String {
    let m = meta();
    let plans = list_inspection_plans(conn, TEST_TENANT, Some("prd_bracket"), false).unwrap();
    let inspections = aberp_qa::list_inspections_for_wo(conn, TEST_TENANT, "wo_1").unwrap();
    let tx = conn.transaction().unwrap();
    let (report, _lines) = freeze_report(
        &tx,
        &ctx(&m, "ervin"),
        FreezeReportInputs {
            report_kind: kind,
            template,
            wo_id: "wo_1",
            product_id: "prd_bracket",
            partner_id: "ptr_prime",
            plans: &plans,
            inspections: &inspections,
            units,
            open_ncr_against_reported_part: false,
            traceability: ReportTraceability {
                drawing_number: Some("DWG-4471".into()),
                drawing_rev: Some("C".into()),
                heat_lot_reference: Some("HL-9911".into()),
                ..Default::default()
            },
            customer: ReportCustomer {
                name: Some("Prime Aerospace Kft.".into()),
                address_line: Some("1117 Budapest, Fo utca 1., HU".into()),
                purchase_order: None,
            },
            created_by: "ervin",
        },
        now(),
    )
    .unwrap();
    tx.commit().unwrap();
    report.qcr_id
}

fn units() -> Vec<ReportUnit> {
    vec![
        ReportUnit {
            part_serial: "SN-001".into(),
            part_uid: "uid1".into(),
        },
        ReportUnit {
            part_serial: "SN-002".into(),
            part_uid: "uid2".into(),
        },
    ]
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0199 §AC1 — the end-to-end happy path.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn full_accountability_freezes_issues_and_binds() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    let p2 = seed_plan(&conn, "Face Z", "2", true);
    for uid in ["uid1", "uid2"] {
        measure(&mut conn, &p1, uid, 25.0);
        measure(&mut conn, &p2, uid, 25.01);
    }

    let qcr_id = freeze(
        &mut conn,
        &units(),
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );

    let report = get_report(&conn, TEST_TENANT, &qcr_id).unwrap().unwrap();
    assert_eq!(report.state, QcReportState::Drafted);
    assert_eq!(report.disposition, Disposition::Accept);
    // 2 characteristics × 2 units = 4/4 accounted for.
    assert_eq!(report.characteristics_required, 4);
    assert_eq!(report.characteristics_measured, 4);
    assert_eq!(report.characteristics_passed, 4);
    assert_eq!(report.characteristics_unaccounted, 0);
    assert_eq!(report.qty_reported, 2);
    assert_eq!(
        report.serial_range.as_deref(),
        Some("SN-001 … SN-002 (2 units)")
    );
    assert_eq!(report.drawing_number.as_deref(), Some("DWG-4471"));
    assert_eq!(report.drawing_rev.as_deref(), Some("C"));
    assert!(report.report_number.starts_with("QCR-2026-"));
    assert_eq!(count_kind(&conn, EventKind::QcReportDrafted), 1);

    let lines = list_report_lines(&conn, TEST_TENANT, &qcr_id).unwrap();
    assert_eq!(lines.len(), 4);
    assert!(lines
        .iter()
        .all(|l| l.accountability == Accountability::Measured));
    // line_no is 1-based, dense, and stable.
    assert_eq!(
        lines.iter().map(|l| l.line_no).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    // Identity metadata survived the freeze.
    assert_eq!(lines[0].characteristic_number.as_deref(), Some("1"));
    assert_eq!(lines[0].sheet_zone.as_deref(), Some("1/B4"));
    assert_eq!(
        lines[0].inspection_method,
        Some(InspectionMethod::OnMachineProbe)
    );

    // Issue.
    let m = meta();
    let tx = conn.transaction().unwrap();
    let issued = issue_report(
        &tx,
        &ctx(&m, "ervin"),
        &qcr_id,
        "abc123",
        "aberp-qc-pdf@0.0.0",
        "ervin",
        now(),
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(issued.state, QcReportState::Issued);
    assert_eq!(issued.rendered_sha256.as_deref(), Some("abc123"));
    assert_eq!(issued.issued_by.as_deref(), Some("ervin"));
    assert_eq!(count_kind(&conn, EventKind::QcReportIssued), 1);

    // Bind to a dispatch.
    let tx = conn.transaction().unwrap();
    let bound = bind_reports_to_dispatch(&tx, &ctx(&m, "ervin"), "wo_1", "dsp_77").unwrap();
    tx.commit().unwrap();
    assert_eq!(bound, vec![qcr_id.clone()]);
    assert_eq!(count_kind(&conn, EventKind::QcReportAttachedToShipment), 1);

    let bound_reports = list_reports_for_dispatch(&conn, TEST_TENANT, "dsp_77").unwrap();
    assert_eq!(bound_reports.len(), 1);
    assert_eq!(bound_reports[0].dsp_id.as_deref(), Some("dsp_77"));
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0199 §AC2 — the accountability gap survives the write.
// ─────────────────────────────────────────────────────────────────────

/// Measure 3 of 4 → FOUR rows persist, the fourth marked `not_measured`
/// with a NULL actual, `unaccounted == 1`, disposition `incomplete`.
#[test]
fn a_missing_required_characteristic_persists_as_a_row_and_makes_the_report_incomplete() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    let p2 = seed_plan(&conn, "Face Z", "2", true);
    measure(&mut conn, &p1, "uid1", 25.0);
    measure(&mut conn, &p2, "uid1", 25.0);
    measure(&mut conn, &p1, "uid2", 25.0);
    // p2 on uid2 is NEVER measured.

    let qcr_id = freeze(
        &mut conn,
        &units(),
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    let report = get_report(&conn, TEST_TENANT, &qcr_id).unwrap().unwrap();
    assert_eq!(report.characteristics_required, 4);
    assert_eq!(report.characteristics_measured, 3);
    assert_eq!(report.characteristics_unaccounted, 1);
    assert_eq!(report.disposition, Disposition::Incomplete);
    assert!(
        !report.disposition.permits_shipment(),
        "an incomplete report must not release a shipment"
    );

    let lines = list_report_lines(&conn, TEST_TENANT, &qcr_id).unwrap();
    assert_eq!(lines.len(), 4, "the gap is a ROW, not an omission");
    let gap: Vec<_> = lines
        .iter()
        .filter(|l| l.accountability == Accountability::NotMeasured)
        .collect();
    assert_eq!(gap.len(), 1);
    assert_eq!(gap[0].part_serial.as_deref(), Some("SN-002"));
    assert_eq!(gap[0].characteristic_name, "Face Z");
    assert_eq!(gap[0].actual_value, None);
    assert_eq!(gap[0].verdict, None);
    assert_eq!(gap[0].qci_id, None);
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0199 §AC4 — snapshot immunity.
// ─────────────────────────────────────────────────────────────────────

/// **The frozen report does not change when the plan changes.**
///
/// Issue a report, then edit the tolerance and archive a characteristic.
/// The stored lines are byte-for-byte what they were — which is what makes
/// the SHA-256 pin in §D7 mean anything, and what stops an operator from
/// retroactively widening the band a shipped part was judged against.
#[test]
fn a_frozen_report_is_immune_to_later_plan_edits() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    let p2 = seed_plan(&conn, "Face Z", "2", true);
    measure(&mut conn, &p1, "uid1", 25.0);
    measure(&mut conn, &p2, "uid1", 25.0);

    let unit_one = vec![ReportUnit {
        part_serial: "SN-001".into(),
        part_uid: "uid1".into(),
    }];
    let qcr_id = freeze(
        &mut conn,
        &unit_one,
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    let before = list_report_lines(&conn, TEST_TENANT, &qcr_id).unwrap();
    let report_before = get_report(&conn, TEST_TENANT, &qcr_id).unwrap().unwrap();

    // Now mutate the master data underneath it. Everything an operator can
    // edit on a measured plan moves: nominal, both tolerances, balloon
    // number, method, zone, and the required flag.
    //
    // `characteristic_type` deliberately does NOT move — round 6 made it
    // un-editable once a plan has measurements (it decides whether the
    // characteristic reports per-serial or once for the lot, so re-reading
    // existing evidence under a new classification is a rewritten record).
    // The refusal is asserted below, against these same frozen lines.
    update_inspection_plan(
        &conn,
        TEST_TENANT,
        &p1,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "Bore D".into(),
            nominal_value: 99.0,
            upper_tol: 5.0,
            lower_tol: -5.0,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("999".into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::Visual),
            sheet_zone: Some("9/Z9".into()),
            is_required: Some(false),
        },
    )
    .unwrap();
    archive_inspection_plan(&conn, TEST_TENANT, &p2).unwrap();

    let after = list_report_lines(&conn, TEST_TENANT, &qcr_id).unwrap();
    let report_after = get_report(&conn, TEST_TENANT, &qcr_id).unwrap().unwrap();
    assert_eq!(
        before, after,
        "the frozen lines must be identical after a tolerance edit + an archive"
    );
    assert_eq!(report_before, report_after);
    assert_eq!(after.len(), 2, "the archived characteristic's row remains");

    // ── Round 6, B-1 — and the ONE edit that is now refused outright. ──
    //
    // `characteristic_type` is not a label; `build_report_lines` partitions
    // on it, so `Dimensional` → `Process` re-reads the SAME plan row from N
    // per-serial lines to ONE lot-level line. On a partially-measured WO
    // that collapse dropped `unaccounted` to 0 and turned the computed
    // disposition from `incomplete` into `accept` — the release happened
    // below the shipment gate, which only ever reads the disposition.
    let refused = update_inspection_plan(
        &conn,
        TEST_TENANT,
        &p1,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "Bore D".into(),
            nominal_value: 99.0,
            upper_tol: 5.0,
            lower_tol: -5.0,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("999".into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Process),
            inspection_method: Some(InspectionMethod::Visual),
            sheet_zone: Some("9/Z9".into()),
            is_required: Some(false),
        },
    );
    match refused {
        Err(aberp_qa::QcError::Validation(m)) => {
            assert!(
                m.contains("characteristic_type"),
                "the refusal must name the field it refused: {m}"
            );
        }
        other => panic!("expected a Validation refusal, got {other:?}"),
    }
    // The refused edit persisted nothing.
    let plan_now = aberp_qa::get_inspection_plan(&conn, TEST_TENANT, &p1)
        .unwrap()
        .unwrap();
    assert_eq!(
        plan_now.characteristic_type,
        Some(CharacteristicType::Dimensional)
    );
}

/// The type IS editable while the characteristic has no evidence — the
/// guard keys on measurements, not on the plan's age. Without this the
/// refusal could be "always refuse", which would break plan setup.
#[test]
fn characteristic_type_is_editable_until_the_first_measurement() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Coating", "7", true);

    // No measurement yet → the re-classification goes through.
    let edited = update_inspection_plan(
        &conn,
        TEST_TENANT,
        &p1,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "Coating".into(),
            nominal_value: 25.0,
            upper_tol: 0.05,
            lower_tol: -0.05,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("7".into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Process),
            inspection_method: Some(InspectionMethod::Visual),
            sheet_zone: None,
            is_required: Some(true),
        },
    )
    .unwrap();
    assert_eq!(
        edited.characteristic_type,
        Some(CharacteristicType::Process)
    );

    // After a measurement, an ordinary edit that leaves the type alone
    // still works — only the re-classification is refused.
    measure(&mut conn, &p1, "uid1", 25.0);
    let ordinary = update_inspection_plan(
        &conn,
        TEST_TENANT,
        &p1,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "Coating".into(),
            nominal_value: 25.0,
            upper_tol: 0.10,
            lower_tol: -0.10,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("7".into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Process),
            inspection_method: Some(InspectionMethod::Visual),
            sheet_zone: None,
            is_required: Some(true),
        },
    )
    .unwrap();
    assert_eq!(ordinary.upper_tol, 0.10);
}

/// A pre-ADR-0199 client omits `characteristic_type` entirely, and
/// `None` READS as `Dimensional`. The guard compares EFFECTIVE types, so
/// a legacy body editing a measured dimensional plan is not a
/// re-classification and must not 400 — the commonest edit there is.
#[test]
fn a_legacy_body_omitting_the_type_is_not_a_reclassification() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure(&mut conn, &p1, "uid1", 25.0);
    let edited = update_inspection_plan(
        &conn,
        TEST_TENANT,
        &p1,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "Bore D".into(),
            nominal_value: 25.0,
            upper_tol: 0.08,
            lower_tol: -0.08,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: None,
            characteristic_designator: None,
            characteristic_type: None, // the legacy body
            inspection_method: None,
            sheet_zone: None,
            is_required: None,
        },
    )
    .unwrap();
    assert_eq!(edited.upper_tol, 0.08);
    assert_eq!(edited.characteristic_type, None);
}

// ─────────────────────────────────────────────────────────────────────
// Issuance discipline + binding scope.
// ─────────────────────────────────────────────────────────────────────

/// A report can be issued exactly once. Re-issuing would mint a second
/// SHA for a document that already has a chain-pinned identity.
#[test]
fn a_report_cannot_be_issued_twice() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure(&mut conn, &p1, "uid1", 25.0);
    let qcr_id = freeze(
        &mut conn,
        &[ReportUnit {
            part_serial: "SN-001".into(),
            part_uid: "uid1".into(),
        }],
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    let m = meta();
    let tx = conn.transaction().unwrap();
    issue_report(
        &tx,
        &ctx(&m, "ervin"),
        &qcr_id,
        "sha1",
        "v0",
        "ervin",
        now(),
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let second = issue_report(
        &tx,
        &ctx(&m, "ervin"),
        &qcr_id,
        "sha2",
        "v0",
        "ervin",
        now(),
    );
    assert!(second.is_err(), "a second issuance must be refused");
    drop(tx);

    let report = get_report(&conn, TEST_TENANT, &qcr_id).unwrap().unwrap();
    assert_eq!(
        report.rendered_sha256.as_deref(),
        Some("sha1"),
        "the original pin survives the refused re-issue"
    );
    assert_eq!(count_kind(&conn, EventKind::QcReportIssued), 1);
}

/// Binding only picks up ISSUED, unbound reports. A draft is not evidence
/// and must never reach a shipment.
#[test]
fn binding_ignores_drafts_and_already_bound_reports() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    measure(&mut conn, &p1, "uid1", 25.0);
    let unit_one = vec![ReportUnit {
        part_serial: "SN-001".into(),
        part_uid: "uid1".into(),
    }];
    let issued_id = freeze(
        &mut conn,
        &unit_one,
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    let draft_id = freeze(
        &mut conn,
        &unit_one,
        QcReportKind::CertificateOfConformance,
        QcReportTemplate::AbenStandard,
    );
    let m = meta();
    let tx = conn.transaction().unwrap();
    issue_report(
        &tx,
        &ctx(&m, "ervin"),
        &issued_id,
        "sha1",
        "v0",
        "ervin",
        now(),
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    let bound = bind_reports_to_dispatch(&tx, &ctx(&m, "ervin"), "wo_1", "dsp_77").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        bound,
        vec![issued_id.clone()],
        "only the issued report binds"
    );
    assert!(
        get_report(&conn, TEST_TENANT, &draft_id)
            .unwrap()
            .unwrap()
            .dsp_id
            .is_none(),
        "the draft must stay unbound"
    );

    // A second bind is a no-op — the report is no longer unbound.
    let tx = conn.transaction().unwrap();
    let again = bind_reports_to_dispatch(&tx, &ctx(&m, "ervin"), "wo_1", "dsp_88").unwrap();
    tx.commit().unwrap();
    assert!(again.is_empty());
    assert_eq!(count_kind(&conn, EventKind::QcReportAttachedToShipment), 1);
}

/// A template that cannot produce the requested kind is refused at
/// freeze time, before anything is written.
#[test]
fn a_coc_only_customer_cannot_be_handed_a_characteristic_table() {
    let mut conn = setup_db();
    seed_plan(&conn, "Bore D", "1", true);
    let m = meta();
    let plans = list_inspection_plans(&conn, TEST_TENANT, Some("prd_bracket"), false).unwrap();
    let tx = conn.transaction().unwrap();
    let result = freeze_report(
        &tx,
        &ctx(&m, "ervin"),
        FreezeReportInputs {
            report_kind: QcReportKind::DimensionalInspection,
            template: QcReportTemplate::CocOnly,
            wo_id: "wo_1",
            product_id: "prd_bracket",
            partner_id: "ptr_prime",
            plans: &plans,
            inspections: &[],
            units: &[],
            open_ncr_against_reported_part: false,
            traceability: ReportTraceability::default(),
            customer: ReportCustomer::default(),
            created_by: "ervin",
        },
        now(),
    );
    assert!(result.is_err());
    drop(tx);
    assert_eq!(count_kind(&conn, EventKind::QcReportDrafted), 0);
}

/// **A measurement nothing can date REFUSES the freeze** (round 3).
///
/// `latest_measurement` now orders by the parsed instant, and an
/// unparseable `measured_at_utc` yields `None` — which `Option`'s ordering
/// sorts LOWEST. Left alone that silently demotes the offending row, and
/// demoting a failing re-measurement is how a part is reported on its
/// earlier passing value. So `freeze_report` refuses the whole freeze
/// instead, before anything is written.
///
/// The row is corrupted with direct SQL because nothing in the application
/// can produce it: `record_inspection` formats every `measured_at_utc`
/// through the one `rfc3339` helper. That is the point — this is the
/// backstop against a row written outside the application.
#[test]
fn a_measurement_with_an_unreadable_timestamp_refuses_the_freeze() {
    let mut conn = setup_db();
    let plan_id = seed_plan(&conn, "Bore D", "1", true);
    let unit = ReportUnit {
        part_serial: "SN-001".into(),
        part_uid: "uid1".into(),
    };
    measure(&mut conn, &plan_id, "uid1", 25.0);

    // Positive control: the freeze succeeds while the timestamp is readable.
    let ok_id = freeze(
        &mut conn,
        std::slice::from_ref(&unit),
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    assert!(!ok_id.is_empty());
    let drafted_before = count_kind(&conn, EventKind::QcReportDrafted);

    conn.execute(
        "UPDATE qc_inspections SET measured_at_utc = 'not-a-timestamp'
         WHERE tenant_id = ?1",
        duckdb::params![TEST_TENANT],
    )
    .unwrap();

    let m = meta();
    let plans = list_inspection_plans(&conn, TEST_TENANT, Some("prd_bracket"), false).unwrap();
    let inspections = aberp_qa::list_inspections_for_wo(&conn, TEST_TENANT, "wo_1").unwrap();
    assert!(
        !inspections.is_empty(),
        "precondition: there is a measurement to be refused over"
    );
    let tx = conn.transaction().unwrap();
    let err = freeze_report(
        &tx,
        &ctx(&m, "ervin"),
        FreezeReportInputs {
            report_kind: QcReportKind::DimensionalInspection,
            template: QcReportTemplate::AbenStandard,
            wo_id: "wo_1",
            product_id: "prd_bracket",
            partner_id: "ptr_prime",
            plans: &plans,
            inspections: &inspections,
            units: std::slice::from_ref(&unit),
            open_ncr_against_reported_part: false,
            traceability: ReportTraceability::default(),
            customer: ReportCustomer::default(),
            created_by: "ervin",
        },
        now(),
    )
    .expect_err("the freeze must refuse a measurement it cannot order");
    let msg = err.to_string();
    assert!(
        msg.contains("measured_at_utc"),
        "the refusal names the field: {msg}"
    );
    drop(tx);

    // Nothing was written — the refusal is before the draft, not after it.
    assert_eq!(
        count_kind(&conn, EventKind::QcReportDrafted),
        drafted_before,
        "a refused freeze must not leave a drafted report behind"
    );
}

/// Report numbers are allocated densely per tenant per year.
#[test]
fn report_numbers_are_allocated_in_sequence() {
    let mut conn = setup_db();
    seed_plan(&conn, "Bore D", "1", true);
    let a = freeze(
        &mut conn,
        &[],
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    let b = freeze(
        &mut conn,
        &[],
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    assert_eq!(
        get_report(&conn, TEST_TENANT, &a)
            .unwrap()
            .unwrap()
            .report_number,
        "QCR-2026-0001"
    );
    assert_eq!(
        get_report(&conn, TEST_TENANT, &b)
            .unwrap()
            .unwrap()
            .report_number,
        "QCR-2026-0002"
    );
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0199 §D3(a) — drawing refs with revision history.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn a_new_drawing_revision_supersedes_without_losing_history() {
    let mut conn = setup_db();
    let rev_b = record_drawing_ref(
        &mut conn,
        TEST_TENANT,
        NewPartDrawingRef {
            product_id: "prd_bracket".into(),
            drawing_number: "DWG-4471".into(),
            drawing_rev: "B".into(),
        },
        "ervin",
        t("2026-01-01T00:00:00Z"),
    )
    .unwrap();
    assert!(rev_b.is_current());

    let rev_c = record_drawing_ref(
        &mut conn,
        TEST_TENANT,
        NewPartDrawingRef {
            product_id: "prd_bracket".into(),
            drawing_number: "DWG-4471".into(),
            drawing_rev: "C".into(),
        },
        "ervin",
        t("2026-06-01T00:00:00Z"),
    )
    .unwrap();

    let current = current_drawing_ref(&conn, TEST_TENANT, "prd_bracket")
        .unwrap()
        .unwrap();
    assert_eq!(current.drawing_ref_id, rev_c.drawing_ref_id);
    assert_eq!(current.drawing_rev, "C");

    // The OLD revision is still readable — a 2026 report must still name
    // the revision it was inspected against when read in 2033.
    let history = list_drawing_refs(&conn, TEST_TENANT, "prd_bracket").unwrap();
    assert_eq!(history.len(), 2);
    let old = history.iter().find(|d| d.drawing_rev == "B").unwrap();
    assert!(!old.is_current());
    assert_eq!(old.superseded_at.as_deref(), Some("2026-06-01T00:00:00Z"));
}

/// Re-recording the SAME current revision is a no-op — an operator's
/// double-click must not manufacture a spurious revision event.
#[test]
fn recording_the_same_current_revision_is_idempotent() {
    let mut conn = setup_db();
    let first = record_drawing_ref(
        &mut conn,
        TEST_TENANT,
        NewPartDrawingRef {
            product_id: "prd_bracket".into(),
            drawing_number: "DWG-4471".into(),
            drawing_rev: "C".into(),
        },
        "ervin",
        now(),
    )
    .unwrap();
    let again = record_drawing_ref(
        &mut conn,
        TEST_TENANT,
        NewPartDrawingRef {
            product_id: "prd_bracket".into(),
            drawing_number: "DWG-4471".into(),
            drawing_rev: "C".into(),
        },
        "ervin",
        t("2026-09-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(first.drawing_ref_id, again.drawing_ref_id);
    assert_eq!(
        list_drawing_refs(&conn, TEST_TENANT, "prd_bracket")
            .unwrap()
            .len(),
        1
    );
}

/// A blank revision is refused: "inspected against rev ___" is an
/// unfalsifiable claim on a compliance record.
#[test]
fn a_drawing_without_a_revision_is_refused() {
    let mut conn = setup_db();
    let bad = record_drawing_ref(
        &mut conn,
        TEST_TENANT,
        NewPartDrawingRef {
            product_id: "prd_bracket".into(),
            drawing_number: "DWG-4471".into(),
            drawing_rev: "   ".into(),
        },
        "ervin",
        now(),
    );
    assert!(bad.is_err());
    assert!(list_drawing_refs(&conn, TEST_TENANT, "prd_bracket")
        .unwrap()
        .is_empty());
}

// ─────────────────────────────────────────────────────────────────────
// ADR-0199 §D7 — a frozen row's vocabularies are decoded STRICTLY.
//
// A frozen report line IS the evidence, so a token that no longer parses
// means the row was hand-edited or corrupted. Coercing it to something
// readable would let a tampered row present as conforming — the exact
// failure the whole snapshot discipline exists to prevent. (Contrast
// `qc::plans`, where the same columns are cosmetic metadata on a MUTABLE
// reference row and a lenient read is correct; that asymmetry is
// deliberate and documented at both sites.)
// ─────────────────────────────────────────────────────────────────────

/// Tamper each closed-vocab column on a frozen line in turn; each one
/// must fail the read LOUD rather than decode to a default.
#[test]
fn a_tampered_vocabulary_token_on_a_frozen_line_fails_the_read() {
    for (column, bogus) in [
        ("accountability", "measured_probably"),
        ("verdict", "ok"),
        ("characteristic_type", "DIMENSIONAL"),
        ("inspection_method", "probe"),
        ("characteristic_designator", "KEY"),
    ] {
        let mut conn = setup_db();
        let p1 = seed_plan(&conn, "Bore D", "1", true);
        measure(&mut conn, &p1, "uid1", 25.0);
        let qcr_id = freeze(
            &mut conn,
            &[ReportUnit {
                part_serial: "SN-001".into(),
                part_uid: "uid1".into(),
            }],
            QcReportKind::DimensionalInspection,
            QcReportTemplate::AbenStandard,
        );
        // Sanity: it reads cleanly before the tamper.
        assert_eq!(
            list_report_lines(&conn, TEST_TENANT, &qcr_id)
                .unwrap()
                .len(),
            1
        );

        conn.execute(
            &format!("UPDATE qc_report_lines SET {column} = ? WHERE tenant_id = ? AND qcr_id = ?;"),
            duckdb::params![bogus, TEST_TENANT, &qcr_id],
        )
        .unwrap();

        let read = list_report_lines(&conn, TEST_TENANT, &qcr_id);
        assert!(
            read.is_err(),
            "a tampered {column} = {bogus:?} must fail the read, not decode to a default"
        );
        let msg = format!("{}", read.unwrap_err());
        assert!(
            msg.contains(bogus),
            "the failure must NAME the offending token so an auditor can see what was \
             changed: {msg}"
        );
    }
}

/// The same discipline on the report HEADER: a tampered `disposition` or
/// `state` fails the read rather than presenting as shippable.
#[test]
fn a_tampered_disposition_or_state_on_the_header_fails_the_read() {
    for (column, bogus) in [
        ("disposition", "accept_probably"),
        ("state", "ISSUED"),
        ("report_kind", "fair"),
        ("template", "boeing"),
    ] {
        let mut conn = setup_db();
        seed_plan(&conn, "Bore D", "1", true);
        let qcr_id = freeze(
            &mut conn,
            &[],
            QcReportKind::DimensionalInspection,
            QcReportTemplate::AbenStandard,
        );
        conn.execute(
            &format!("UPDATE qc_reports SET {column} = ? WHERE tenant_id = ? AND qcr_id = ?;"),
            duckdb::params![bogus, TEST_TENANT, &qcr_id],
        )
        .unwrap();
        let read = get_report(&conn, TEST_TENANT, &qcr_id);
        assert!(
            read.is_err(),
            "a tampered {column} = {bogus:?} must fail the read — a corrupted \
             disposition that silently decoded to something readable could present \
             a rejected report as shippable"
        );
    }
}

/// **A measured line freezes the MEASUREMENT's tolerances, not the plan's
/// current ones.**
///
/// The window this closes: a part is measured against ±0.05, an operator
/// then widens the plan to ±5.0, and only afterwards is the report
/// generated. Reading the live plan at freeze time would print "nominal
/// 25.0, tol ±5.0, actual 25.04 — PASS" and the document would claim the
/// part conformed to a band it was never judged against. The measurement
/// row already froze the real band (V002's stated audit requirement); this
/// test pins that the report layer reads THAT and not the plan.
///
/// Note this is distinct from `a_frozen_report_is_immune_to_later_plan_edits`,
/// which edits the plan AFTER issuance. Here the edit lands BEFORE the
/// report is ever built, so an immunity check on the frozen rows cannot
/// see the difference.
#[test]
fn a_measured_line_records_the_band_it_was_measured_against_not_the_current_plan() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Bore D", "1", true);

    // Measured against the ORIGINAL ±0.05 band.
    measure(&mut conn, &p1, "uid1", 25.02);

    // The operator then widens the band AND renames the feature — before
    // any report exists.
    update_inspection_plan(
        &conn,
        TEST_TENANT,
        &p1,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "Bore D (renamed)".into(),
            nominal_value: 30.0,
            upper_tol: 5.0,
            lower_tol: -5.0,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("1".into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: Some("1/B4".into()),
            is_required: Some(true),
        },
    )
    .unwrap();

    let qcr_id = freeze(
        &mut conn,
        &[ReportUnit {
            part_serial: "SN-001".into(),
            part_uid: "uid1".into(),
        }],
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    let lines = list_report_lines(&conn, TEST_TENANT, &qcr_id).unwrap();
    assert_eq!(lines.len(), 1);
    let l = &lines[0];
    assert_eq!(
        l.nominal_value,
        Some(25.0),
        "the report must state the nominal the part was MEASURED against (25.0), \
         not the plan's current 30.0"
    );
    assert_eq!(l.upper_tol, Some(0.05), "…and the band it was judged by");
    assert_eq!(l.lower_tol, Some(-0.05));
    assert_eq!(l.actual_value, Some(25.02));
    assert_eq!(
        l.characteristic_name, "Bore D",
        "the NAME travels with the band for the same reason: the row must say what \
         the part was measured against, and a name that no longer matches its \
         tolerances would make the line self-inconsistent"
    );
}

/// The mirror case: an UNMEASURED line has no measurement to freeze, so it
/// necessarily reports the plan's current requirement — which is the right
/// answer, because that IS what still needs measuring.
#[test]
fn an_unmeasured_line_reports_the_current_plan_requirement() {
    let mut conn = setup_db();
    let p1 = seed_plan(&conn, "Bore D", "1", true);
    update_inspection_plan(
        &conn,
        TEST_TENANT,
        &p1,
        NewInspectionPlan {
            product_id: "prd_bracket".into(),
            feature_name: "Bore D".into(),
            nominal_value: 30.0,
            upper_tol: 0.2,
            lower_tol: -0.2,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: Some("1".into()),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: None,
            is_required: Some(true),
        },
    )
    .unwrap();

    let qcr_id = freeze(
        &mut conn,
        &[ReportUnit {
            part_serial: "SN-001".into(),
            part_uid: "uid1".into(),
        }],
        QcReportKind::DimensionalInspection,
        QcReportTemplate::AbenStandard,
    );
    let lines = list_report_lines(&conn, TEST_TENANT, &qcr_id).unwrap();
    assert_eq!(lines[0].accountability, Accountability::NotMeasured);
    assert_eq!(lines[0].nominal_value, Some(30.0));
    assert_eq!(lines[0].actual_value, None);
}
