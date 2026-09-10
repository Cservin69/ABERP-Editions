//! End-to-end writer→verifier round-trip for the SHIPMENT evidence bundle
//! (ADR-0122 slice 3) — the test that closes D-99 residual 1 / AC10.
//!
//! Drives the real `aberp::export_shipment_bundle::run` against a real
//! DuckDB + audit-ledger + QC-report fixture, then runs the real
//! `aberp_verify::verify_bundle` over the archive it produced. That pairing
//! is the point: before this, `aberp-verify` could accept, re-hash and
//! cross-total a `qc/` directory and **nothing in the tree ever produced
//! one** — `qc_archive_path` had exactly two callers, the verifier and its
//! own unit test.
//!
//! The report is issued through the SAME rendering path the application
//! uses, so the `rendered_sha256` pinned in the chain is a real hash of real
//! bytes. A fixture that pinned a placeholder would make the SHA check
//! vacuous — and would in fact trip ADR-0122 §D2's tamper refusal, which is
//! itself exercised below.
//!
//! Defense-only: the whole subcommand is gated on `qc_reporting_allowed`
//! (ADR-0199 §D9), so this file runs under `--features production`.

#![cfg(feature = "production")]

use std::io::Read;
use std::path::{Path, PathBuf};

use duckdb::{params, Connection};

use aberp::cli::ExportShipmentBundleArgs;
use aberp::part_marking::{
    data_matrix_payload, ensure_schema as ensure_part_schema, generate_part_uid, record_part_marks,
    PartMark,
};
use aberp_audit_ledger::{
    ensure_schema as audit_ensure_schema, Actor, BinaryHash, EventKind, Ledger, LedgerMeta,
    TenantId,
};
use aberp_inventory::ActorKind;
use aberp_qa::{
    create_inspection_plan, freeze_report, list_inspection_plans, list_inspections_for_wo,
    record_inspection, CharacteristicType, FreezeReportInputs, InspectionMethod, NewInspectionPlan,
    QcReportKind, QcReportTemplate, QcSource, QcWriteContext, RecordInspectionInputs,
    ReportCustomer, ReportTraceability, ReportUnit,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const T: &str = "shipment_bundle_rt";
const WO: &str = "wo-rt";
const DSP: &str = "dsp_01ARZ3NDEKTSV4RRFFQ69G5FAV";
const BH: BinaryHash = BinaryHash::from_bytes([0xAB; 32]);

fn now() -> OffsetDateTime {
    OffsetDateTime::parse("2026-09-10T12:00:00Z", &Rfc3339).unwrap()
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(TenantId::new(T).unwrap(), BH)
}

fn ctx(m: &LedgerMeta) -> QcWriteContext<'_> {
    QcWriteContext {
        tenant: T,
        actor: ActorKind::SpaOperator {
            operator_login: "ervin".into(),
        },
        ledger_meta: m,
        ledger_actor: Actor::from_local_cli("rt-session".into(), "ervin"),
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-shipment-bundle-rt")
        .join(format!("{tag}-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A tenant DB with an issued QC report bound to a shipped dispatch, and the
/// matching chain entries. Returns `(db_path, qcr_id, pinned_sha)`.
fn fixture(tag: &str) -> (PathBuf, String, String) {
    let dir = temp_dir(tag);
    let db_path = dir.join("aberp.duckdb");
    {
        let mut conn = Connection::open(&db_path).unwrap();
        audit_ensure_schema(&conn).unwrap();
        ensure_part_schema(&conn).unwrap();
        aberp_qa::ensure_schema(&conn).unwrap();
        aberp_work_orders::ensure_schema(&conn).unwrap();
        aberp_dispatch::ensure_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO work_orders (
                wo_id, tenant_id, wo_number, product_id, qty_target, state, created_at
             ) VALUES (?1, ?2, ?3, 'prd_bracket', '1', 'completed', '2026-09-01T00:00:00Z')",
            params![WO, T, WO],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatches (dsp_id, tenant_id, wo_id, partner_id, state, created_at)
             VALUES (?1, ?2, ?3, 'ptr_x', 'shipped', '2026-09-02T00:00:00Z')",
            params![DSP, T, WO],
        )
        .unwrap();

        // One marked unit, one required characteristic, one measurement in
        // tolerance — the minimum that produces a report that ACCEPTS.
        let part_uid = generate_part_uid();
        let serial = "SN-001".to_string();
        record_part_marks(
            &conn,
            T,
            WO,
            &[PartMark {
                wo_id: WO.to_string(),
                unit_index: 1,
                part_uid: part_uid.clone(),
                serial_number: serial.clone(),
                data_matrix_payload: data_matrix_payload(&part_uid, &serial, None),
                heat_lot_reference: Some("HL-9911".into()),
                marked_at_utc: "2026-09-02T00:00:00Z".to_string(),
                marked_by_operator: "op".to_string(),
                iuid: String::new(),
            }],
        )
        .unwrap();
        let units = vec![ReportUnit {
            part_serial: serial,
            part_uid: part_uid.clone(),
        }];

        let plan_id = create_inspection_plan(
            &conn,
            T,
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
                characteristic_type: Some(CharacteristicType::Dimensional),
                inspection_method: Some(InspectionMethod::OnMachineProbe),
                sheet_zone: None,
                is_required: Some(true),
            },
        )
        .unwrap()
        .plan_id;

        let m = meta();
        let plan = aberp_qa::get_inspection_plan(&conn, T, &plan_id)
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
                actual_value: 25.0,
                units: "mm".into(),
                probe_serial: None,
                last_calibration_at: None,
                measured_at: now(),
                current_time: now(),
                stale_window_seconds: 86_400,
                linked_part_uid: Some(part_uid),
                linked_heat_lot: Some("HL-9911".into()),
                linked_wo_id: Some(WO.into()),
                recorded_by: "ervin".into(),
            },
        )
        .unwrap();
        tx.commit().unwrap();

        // Freeze the report.
        let plans = list_inspection_plans(&conn, T, Some("prd_bracket"), false).unwrap();
        let inspections = list_inspections_for_wo(&conn, T, WO).unwrap();
        let tx = conn.transaction().unwrap();
        let (report, _) = freeze_report(
            &tx,
            &ctx(&m),
            FreezeReportInputs {
                report_kind: QcReportKind::DimensionalInspection,
                template: QcReportTemplate::AbenStandard,
                wo_id: WO,
                product_id: "prd_bracket",
                partner_id: "ptr_x",
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
        let qcr_id = report.qcr_id.clone();

        tx.commit().unwrap();
        drop(conn);

        // Issue through the APPLICATION's own path, not a hand-rolled one.
        // `qc_report::issue_report` renders a copy whose issuance stamps are
        // already set and pins the hash of THOSE bytes — the only thing a
        // later re-render can reproduce. A fixture that pinned a placeholder
        // would make the whole SHA check vacuous (and would in fact trip
        // §D2's tamper refusal, which is exercised separately below).
        let db_handle = aberp_db::Handle::open_default(&db_path, TenantId::new(T).unwrap())
            .expect("shared handle for issuance");
        aberp::qc_report::issue_report(
            &db_handle,
            TenantId::new(T).unwrap(),
            BH,
            "ervin",
            now(),
            &qcr_id,
        )
        .expect("issue the report through the real path");
        {
            let mut guard = db_handle.write().unwrap();
            let tx = guard.transaction().unwrap();
            aberp_qa::bind_reports_to_dispatch(&tx, &ctx(&m), WO, DSP).unwrap();
            tx.commit().unwrap();
        }
        let sha = {
            let conn = db_handle.read().unwrap();
            aberp_qa::get_report(&conn, T, &qcr_id)
                .unwrap()
                .unwrap()
                .rendered_sha256
                .expect("an issued report carries its pin")
        };
        drop(db_handle);

        // The shipment's own chain entry. `bind_reports_to_dispatch` fires
        // `qcr.report_attached_to_shipment`; the ship event is what §D1b
        // requires before a SHIPMENT bundle may be produced at all.
        let mut ledger = Ledger::open(&db_path, TenantId::new(T).unwrap(), BH).unwrap();
        ledger
            .append(
                EventKind::DispatchShipped,
                aberp_dispatch::DispatchShippedPayload {
                    dsp_id: DSP.to_string(),
                    wo_id: WO.to_string(),
                    partner_id: "ptr_x".to_string(),
                    carrier_kind: aberp_dispatch::CarrierKind::SelfDelivery,
                    tracking_number: None,
                    shipped_at: "2026-09-02T00:00:00Z".to_string(),
                    spawned_invoice_id: Some("drf_1".to_string()),
                    actor: "ervin".to_string(),
                    idempotency_key: "idem-rt".to_string(),
                }
                .to_bytes(),
                Actor::from_local_cli("rt-session".into(), "ervin"),
                None,
            )
            .unwrap();
        // Keep the mirror in step. A bare `Ledger::open` + `append` writes the
        // DB half only, and the exporter asserts mirror-vs-DB agreement
        // (ADR-0030 §5) before emitting anything — so a fixture that skipped
        // this would be testing the mirror check, not the bundle. (The same
        // ordering hazard on real money paths is what D-22 closed.)
        ledger
            .sync_mirror(&aberp_audit_ledger::mirror_path_for(&db_path))
            .unwrap();
        (db_path, qcr_id, sha)
    }
}

fn args(db: &Path, out: &Path) -> ExportShipmentBundleArgs {
    ExportShipmentBundleArgs {
        dispatch_id: DSP.to_string(),
        out: out.to_path_buf(),
        allow_overwrite: false,
        db: db.to_path_buf(),
        tenant: T.to_string(),
    }
}

/// Read every `bundle/*` path out of the produced archive.
fn archive_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let f = std::fs::File::open(path).unwrap();
    let dec = zstd::stream::read::Decoder::new(f).unwrap();
    let mut tar = tar::Archive::new(dec);
    let mut out = Vec::new();
    for e in tar.entries().unwrap() {
        let mut e = e.unwrap();
        let p = e.path().unwrap().to_string_lossy().into_owned();
        let mut b = Vec::new();
        e.read_to_end(&mut b).unwrap();
        out.push((p, b));
    }
    out
}

/// **The AC10 closer.** The real writer emits a `qc/` document, and the real
/// verifier accepts the archive — including re-hashing that document against
/// the `rendered_sha256` the chain pins.
#[test]
fn the_shipment_bundle_carries_a_qc_document_the_real_verifier_accepts() {
    let (db, qcr_id, pinned) = fixture("ok");
    let out = db.parent().unwrap().join("bundle.tar.zst");
    aberp::export_shipment_bundle::run(&args(&db, &out)).expect("export succeeds");

    let entries = archive_entries(&out);
    let qc_path = format!("bundle/qc/{qcr_id}.pdf");
    let (_, pdf) = entries
        .iter()
        .find(|(p, _)| *p == qc_path)
        .unwrap_or_else(|| panic!("archive must carry {qc_path}; got {:?}", paths(&entries)));
    assert!(
        pdf.starts_with(b"%PDF"),
        "the qc/ entry must be the rendered document"
    );

    // The bytes reproduce the chain pin — the property ADR-0199 §D7's
    // no-store decision rests on.
    use sha2::{Digest, Sha256};
    assert_eq!(
        hex::encode(Sha256::digest(pdf)),
        pinned,
        "a re-render must hash to the SHA the chain pinned at issuance"
    );

    // Manifest states the scope honestly and claims the document.
    let (_, manifest_bytes) = entries
        .iter()
        .find(|(p, _)| *p == "bundle/manifest.json")
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes).unwrap();
    assert_eq!(manifest["version"], serde_json::json!(2));
    assert_eq!(manifest["scope_kind"], serde_json::json!("dispatch"));
    assert_eq!(manifest["scope_id"], serde_json::json!(DSP));
    // §F1 — a shipment cannot be joined back to the invoice that bills it,
    // so the manifest must claim no invoice rather than invent one.
    assert_eq!(manifest["invoice_id"], serde_json::Value::Null);
    assert_eq!(manifest["qc_documents"], serde_json::json!(1));
    assert_eq!(manifest["qc_documents_omitted"], serde_json::json!([]));

    // And the real verifier accepts the whole thing.
    let report = aberp_verify::verify_bundle(&out).expect("verifier reads the archive");
    assert!(
        report.is_ok(),
        "the real verifier must accept the real writer's shipment bundle:\n{}",
        report.failure_details().join("\n")
    );
}

fn paths(entries: &[(String, Vec<u8>)]) -> Vec<&str> {
    entries.iter().map(|(p, _)| p.as_str()).collect()
}

/// **§D1b revert-proof, end to end.** A dispatch with no ship event is
/// refused rather than exported. Pass 1 matches `mes.dispatch_created`, so
/// the slice would be non-empty and a zero-entry check would not catch it.
#[test]
fn a_dispatch_that_never_shipped_produces_no_archive() {
    let dir = temp_dir("unshipped");
    let db = dir.join("aberp.duckdb");
    {
        let conn = Connection::open(&db).unwrap();
        audit_ensure_schema(&conn).unwrap();
        aberp_qa::ensure_schema(&conn).unwrap();
    }
    let mut ledger = Ledger::open(&db, TenantId::new(T).unwrap(), BH).unwrap();
    ledger
        .append(
            EventKind::DispatchCreated,
            serde_json::to_vec(&serde_json::json!({ "dsp_id": DSP, "wo_id": WO })).unwrap(),
            Actor::from_local_cli("s".into(), "u"),
            None,
        )
        .unwrap();
    ledger
        .sync_mirror(&aberp_audit_ledger::mirror_path_for(&db))
        .unwrap();
    drop(ledger);

    let out = dir.join("bundle.tar.zst");
    let err = aberp::export_shipment_bundle::run(&args(&db, &out))
        .expect_err("a created-only dispatch must refuse");
    assert!(format!("{err:#}").contains("never shipped"));
    assert!(
        !out.exists(),
        "a refused export must leave no partial archive behind"
    );
}

/// **§D2 revert-proof — the tamper arm REFUSES.** Same renderer, different
/// bytes: the frozen report rows moved under a report that is supposed to be
/// frozen. The export refuses rather than emitting a document the verifier
/// would fail, or omitting it quietly.
#[test]
fn a_same_renderer_sha_divergence_refuses_the_whole_export() {
    let (db, qcr_id, _) = fixture("tamper");
    // Move a frozen row WITHOUT touching `renderer_version`. This is exactly
    // the shape the 2x2's refuse cell exists for.
    //
    // The mutated column has to be one the renderer actually PRINTS, or the
    // bytes do not move and the test proves nothing. A first cut of this
    // mutated `created_by`, which is not on the page: the export succeeded
    // and the test caught its own vacuity. `report_number` is the report's
    // identity on the document, so it cannot be invisible.
    {
        let conn = Connection::open(&db).unwrap();
        let touched = conn
            .execute(
                "UPDATE qc_reports SET report_number = report_number || '-TAMPERED'
                 WHERE tenant_id = ?1 AND qcr_id = ?2",
                params![T, &qcr_id],
            )
            .unwrap();
        assert_eq!(touched, 1, "the mutation must actually hit the frozen row");
    }
    let out = db.parent().unwrap().join("bundle.tar.zst");
    let err = aberp::export_shipment_bundle::run(&args(&db, &out))
        .expect_err("a same-renderer SHA divergence must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("SAME renderer") && msg.contains(&qcr_id),
        "the refusal must name the report and say the renderer is unchanged: {msg}"
    );
    assert!(!out.exists(), "a refused export writes no archive");
}

/// **§D2/§D3 — the renderer-upgrade arm OMITS AND NAMES, and still ships a
/// bundle.** This is the cell that stops a routine `aberp-qc-pdf` bump from
/// bricking the export for every dispatch forever.
#[test]
fn a_renderer_version_change_omits_the_document_and_names_it_in_the_manifest() {
    let (db, qcr_id, _) = fixture("renderer");
    {
        let conn = Connection::open(&db).unwrap();
        // Pretend the report was issued by an older renderer whose bytes this
        // build can no longer reproduce.
        conn.execute(
            "UPDATE qc_reports SET renderer_version = 'aberp-qc-pdf@0.9.0',
                                   rendered_sha256 = ?3
             WHERE tenant_id = ?1 AND qcr_id = ?2",
            params![T, &qcr_id, "00".repeat(32)],
        )
        .unwrap();
    }
    let out = db.parent().unwrap().join("bundle.tar.zst");
    aberp::export_shipment_bundle::run(&args(&db, &out))
        .expect("an unreproducible document must NOT refuse the export");

    let entries = archive_entries(&out);
    assert!(
        !entries.iter().any(|(p, _)| p.starts_with("bundle/qc/")),
        "the unreproducible document must not be bundled"
    );
    let (_, manifest_bytes) = entries
        .iter()
        .find(|(p, _)| *p == "bundle/manifest.json")
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes).unwrap();
    assert_eq!(manifest["qc_documents"], serde_json::json!(0));
    let omitted = manifest["qc_documents_omitted"].as_array().unwrap();
    assert_eq!(omitted.len(), 1, "the hole must be stated, not inferred");
    assert_eq!(omitted[0]["qcr_id"], serde_json::json!(qcr_id));
    let reason = omitted[0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("0.9.0") && reason.contains("cannot be reproduced"),
        "the reason must name the renderer that is gone: {reason}"
    );

    // The archive is still verifiable: an issued report with no bundled PDF
    // is explicitly NOT a verifier failure.
    let report = aberp_verify::verify_bundle(&out).expect("verifier reads the archive");
    assert!(
        report.is_ok(),
        "a bundle that names its omission must still verify:\n{}",
        report.failure_details().join("\n")
    );
}
