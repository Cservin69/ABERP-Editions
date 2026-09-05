//! End-to-end pin: the Defense demo seed produces a delivery a QC/AS9102
//! report can actually be built from, and the whole report lifecycle
//! (draft → issue → render) works against that seeded data.
//!
//! This is the runtime proof behind the QC-report SCREEN: the screen only
//! calls `draft_report` / `issue_report` / `render_report` (through the Tauri
//! bridge), so exercising them here against the seed proves the screen would
//! render a real report over a real delivery — the demoability payoff the
//! static gates (svelte-check / vitest) cannot show.
//!
//! Defense-only: the report layer is gated behind `qc_reporting_allowed()`,
//! so the whole file compiles to nothing on the Portable arm.
#![cfg(feature = "production")]

use aberp::{demo_seed, qc_report, serve};
use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_qa::{QcReportKind, QcReportState, QcReportTemplate};
use duckdb::Connection;
use time::OffsetDateTime;

const TENANT: &str = "demo";

/// Seed a scratch `demo` tenant and hand back its db path + a FRESH handle
/// opened AFTER the seed committed — exactly the posture `serve` boot has
/// after `aberp demo-seed` (separate processes), and the only handle that
/// sees every seed row (the seed writes through residual openers a
/// seed-time handle would be blind to).
fn seed_and_open() -> (std::path::PathBuf, aberp_db::HandleArc, TenantId) {
    let dir = std::env::temp_dir()
        .join("aberp-defense-demo-qc-e2e")
        .join(ulid::Ulid::new().to_string());
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let db_path = dir.join("aberp.duckdb");
    let tenant = TenantId::new(TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([7u8; 32]);

    let seed_handle =
        serve::open_tenant_handle(&db_path, tenant.clone()).expect("seed-time handle");
    demo_seed::seed(&db_path, &seed_handle, &tenant, binary_hash).expect("seed");
    drop(seed_handle);

    let handle = serve::open_tenant_handle(&db_path, tenant.clone()).expect("fresh boot handle");
    (db_path, handle, tenant)
}

/// Find a seeded delivery whose work order actually carries QC inspections —
/// the report-eligible one. A report is drafted against a WO that has a
/// dispatch (resolve_context's gate), and only a measured batch produces
/// characteristics, so this is the delivery a demo would report on.
fn reportable_wo(db_path: &std::path::Path) -> String {
    let conn = Connection::open(db_path).expect("fresh read opener");
    let dispatches =
        aberp_dispatch::list_dispatches(&conn, TENANT, None, 50, 0).expect("list dispatches");
    assert!(
        !dispatches.is_empty(),
        "the seed must write at least one delivery"
    );
    dispatches
        .into_iter()
        .map(|d| d.wo_id)
        .find(|wo| {
            aberp_qa::list_inspections_for_wo(&conn, TENANT, wo)
                .map(|v| !v.is_empty())
                .unwrap_or(false)
        })
        .expect("a seeded delivery whose work order has QC inspections")
}

#[test]
fn seeded_delivery_drafts_issues_and_renders_a_qc_report() {
    let (db_path, handle, tenant) = seed_and_open();
    let binary_hash = BinaryHash::from_bytes([7u8; 32]);
    let now = OffsetDateTime::now_utc();
    let wo_id = reportable_wo(&db_path);

    // ── Draft an AS9102 FAIR against the seeded delivery. This runs the real
    //    resolve_context (WO, part marks, drawing ref, dispatch partner) and
    //    the AS9102 characteristic accountability off the seeded plans +
    //    measurements — the exact call the screen's "Draft report" makes.
    let drafted = qc_report::draft_report(
        &handle,
        tenant.clone(),
        binary_hash,
        "demo-operator",
        now,
        qc_report::DraftReportRequest {
            wo_id: wo_id.clone(),
            report_kind: QcReportKind::As9102Fair,
            // An AS9102 FAIR requires the AS9102 Rev C template — the backend
            // rejects the house `aben_standard` default for this kind, so the
            // operator (and this test) must pick the compatible template. The
            // screen surfaces that 400 inline; a demo-seed follow-up could set
            // the aerospace partner's default template to As9102RevC so the
            // FAIR drafts with no override.
            template: Some(QcReportTemplate::As9102RevC),
            notes: None,
        },
    )
    .expect("draft a QC report against the seeded delivery");

    assert_eq!(drafted.report.state, QcReportState::Drafted);
    assert_eq!(drafted.report.wo_id, wo_id);
    assert!(
        !drafted.lines.is_empty(),
        "a report over a measured delivery must enumerate characteristics"
    );
    assert!(
        drafted.report.characteristics_required > 0,
        "the seeded product has required inspection characteristics"
    );
    // Every enumerated line is accounted for: measured + not-measured +
    // not-applicable == the required-characteristic accountability set. The
    // report must never silently drop a required characteristic.
    assert_eq!(
        drafted.report.characteristics_measured + drafted.report.characteristics_unaccounted,
        drafted.report.characteristics_required,
        "measured + unaccounted must reconcile to required (AS9102 accountability)"
    );
    // Not issued yet: no pinned hash. The report_number, however, IS
    // allocated at draft/freeze time (not at issue), so it is already set.
    assert!(drafted.report.rendered_sha256.is_none());
    assert!(
        !drafted.report.report_number.is_empty(),
        "report_number is allocated at freeze (draft), not at issue"
    );

    // ── Issue: freeze the bytes + pin the SHA-256.
    let issued = qc_report::issue_report(
        &handle,
        tenant.clone(),
        binary_hash,
        "demo-operator",
        now,
        &drafted.report.qcr_id,
    )
    .expect("issue the drafted report");

    assert_eq!(issued.report.state, QcReportState::Issued);
    let pinned = issued
        .report
        .rendered_sha256
        .clone()
        .expect("issuing pins a SHA-256");
    assert_eq!(pinned.len(), 64, "SHA-256 is 64 lowercase hex chars");
    assert_eq!(
        issued.report.report_number, drafted.report.report_number,
        "the report_number is stable across issue (allocated once, at draft)"
    );

    // ── Render on demand and confirm the bytes reproduce the pinned hash
    //    (the tamper check the /pdf route surfaces as x-aberp-qc-sha-matches).
    let read = Connection::open(&db_path).expect("fresh read opener");
    let (rep, bytes, sha, matches) =
        qc_report::render_report(&read, TENANT, &drafted.report.qcr_id)
            .expect("render the issued report");
    assert_eq!(rep.qcr_id, drafted.report.qcr_id);
    assert!(!bytes.is_empty(), "a rendered PDF has bytes");
    assert_eq!(
        sha, pinned,
        "re-rendered bytes reproduce the issued SHA-256"
    );
    assert_eq!(
        matches,
        Some(true),
        "an untampered issued report matches its pinned hash"
    );
}
