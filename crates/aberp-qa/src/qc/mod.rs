//! S443 / ADR-0092 — QC dimensional-inspection module.
//!
//! The per-feature MEASUREMENT side of quality, distinct from the
//! routing-op DECISION queue in [`crate`] root (`qa_inspections`). Per
//! ADR-0092 §"Reconciliation" the two coexist at different altitudes;
//! this module adds new tables (`qc_inspection_plans`, `qc_inspections`)
//! rather than overloading `qa_inspections`, so the qa state machine and
//! its WO-completion gate are untouched.
//!
//! What lives here (the domain core):
//! - [`verdict::compute_verdict`] — the pure pass/minor/major/critical
//!   tier + calibration-stale rule ([[trust-code-not-operator]]).
//! - [`plans`] — `qc_inspection_plans` master-data CRUD (the nominal/tol
//!   source of truth; unique (product, feature) in code).
//! - [`inspections::record_inspection`] — the write chokepoint: verdict +
//!   row + the five inspection audit events (in the caller's tx). It does
//!   NOT create the NCR (the app layer does — see module docs).
//! - [`probe`] — the `ProbeIngestionSource` trait + a working
//!   `MockProbeSource` + the `todo!()`-stubbed MTConnect / Renishaw
//!   transports (no machine wired yet; the manual pipeline ships today).

pub mod drawings;
mod error;
pub mod inspections;
pub mod plans;
pub mod probe;
pub mod reports;
pub mod verdict;
pub mod vocab;

use duckdb::Connection;

pub use error::QcError;
pub use inspections::{
    link_auto_ncr, list_inspections_for_part, list_inspections_for_wo,
    list_recent_stale_calibration, record_ingestion_failure, record_inspection, QcInspection,
    QcSource, QcWriteContext, RecordInspectionInputs, RecordedInspection,
};
pub use plans::{
    archive_plan, create_plan, get_plan, list_plans, update_plan, InspectionPlan, NewInspectionPlan,
};
pub use probe::{
    MockProbeSource, MtconnectProbeSource, ProbeCursor, ProbeError, ProbeIngestionSource,
    RawProbeEvent, RenishawCentralSource,
};
pub use verdict::{compute_verdict, Verdict};

// ── ADR-0199 — the REPORTING layer on top of the ADR-0092 model ──
pub use drawings::{
    current_for_product as current_drawing_ref, get as get_drawing_ref,
    list_for_product as list_drawing_refs, supersede_and_create as record_drawing_ref,
    NewPartDrawingRef, PartDrawingRef,
};
pub use reports::{
    bind_reports_to_dispatch, build_report_lines, compute_disposition, freeze_report, get_report,
    issuance_chain_ref, issue_report, list_report_lines, list_reports_for_dispatch,
    list_reports_for_wo, record_render, serial_range_of, summarise, void_report,
    AccountabilityCounts, DraftLine, FreezeReportInputs, QcReport, QcReportLine, ReportCustomer,
    ReportTraceability, ReportUnit,
};
pub use vocab::{
    Accountability, CharacteristicDesignator, CharacteristicType, Disposition, InspectionMethod,
    QcReportKind, QcReportState, QcReportTemplate,
};

/// Apply `V002__qc.sql` (the two QC tables) and `V003__qc_report.sql`
/// (ADR-0199 — the report tables + the six additive plan columns).
/// Idempotent. Called by [`crate::ensure_schema`] so the QC tables exist
/// wherever the QA queue does.
///
/// V003 is STRICTLY ADDITIVE and runs unconditionally in BOTH editions.
/// The Defense-only half of ADR-0199 is the *capability* (routes, gate,
/// renderer — `qc_reporting_allowed_for`), not the schema: a Portable
/// tenant simply has zero rows in all three new tables, exactly as a
/// non-probing tenant has zero `qc_inspections` rows today. Gating the
/// migration itself would mean two divergent physical schemas and a
/// Portable DB that a Defense build could not open.
pub fn ensure_qc_schema(conn: &Connection) -> anyhow::Result<()> {
    use anyhow::Context;
    conn.execute_batch(include_str!("../../migrations/V002__qc.sql"))
        .context("ensure qc schema")?;
    conn.execute_batch(include_str!("../../migrations/V003__qc_report.sql"))
        .context("ensure qc report schema (ADR-0199)")
}
