//! ADR-0199 §D3(c)(d) + §D4 — the QC report record and the
//! characteristic-accountability computation.
//!
//! ## The one idea this module exists to enforce
//!
//! **A QC report enumerates every required characteristic, and prints an
//! explicit row for the ones nobody measured.**
//!
//! A report that lists only what *was* measured is the selective-recording
//! failure mode ADR-0092 names in its own Context — *"re-measure the
//! marginal feature until it passes"* — moved from the shop floor to the
//! printer. It looks complete precisely because the rows that would have
//! failed are absent. So [`build_report_lines`] starts from the PLAN side,
//! not the measurement side: it walks every enabled, non-archived
//! characteristic for the product, joins measurements onto it, and writes
//! [`Accountability::NotMeasured`] with a NULL actual wherever the join
//! finds nothing. `characteristics_unaccounted > 0` forces
//! [`Disposition::Incomplete`], and the ADR-0199 §D6 shipment gate refuses
//! to ship an incomplete report.
//!
//! ## Why the record is frozen
//!
//! ADR-0199 §C3: a QC report is a compliance record, in the same class as
//! an issued invoice — once it goes out the door attached to a shipment,
//! what it said at that moment is the fact. `qc_inspection_plans` rows are
//! mutable (`update_plan` / `archive_plan`), so a report rendered live
//! would silently rewrite its own history the first time an operator
//! edited a tolerance. `qc_inspections` already made this call one level
//! down (its plan values are denormalised snapshots); this module inherits
//! that discipline for the report.
//!
//! ## Purity split
//!
//! [`build_report_lines`], [`summarise`] and [`compute_disposition`] are
//! **pure** — no clock, no I/O, no RNG — so the accountability arithmetic
//! is table-testable without a database. Everything that touches DuckDB or
//! the audit ledger lives below them and does no arithmetic of its own.

use duckdb::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, EventKind};

use super::error::QcError;
use super::inspections::{QcInspection, QcWriteContext};
use super::plans::InspectionPlan;
use super::verdict::Verdict;
use super::vocab::{
    Accountability, CharacteristicDesignator, CharacteristicType, Disposition, InspectionMethod,
    QcReportKind, QcReportState, QcReportTemplate,
};

// ── Records ────────────────────────────────────────────────────────

/// One `qc_reports` row — the frozen header.
///
/// Every `drawing_*` / `heat_lot_*` / `serial_range` / `machine_id` field
/// is a SNAPSHOT resolved once at freeze time, never re-derived at render
/// time (ADR-0199 §D3(c)).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QcReport {
    /// `qcr_<ULID>`.
    pub qcr_id: String,
    pub report_number: String,
    pub report_kind: QcReportKind,
    pub template: QcReportTemplate,
    pub state: QcReportState,
    pub wo_id: String,
    pub product_id: String,
    /// Set when the report is bound to a shipment inside `mark_shipped`'s
    /// transaction (ADR-0199 §D6).
    pub dsp_id: Option<String>,
    pub partner_id: String,
    pub source_quote_id: Option<String>,
    pub drawing_number: Option<String>,
    pub drawing_rev: Option<String>,
    pub qty_reported: u32,
    pub serial_range: Option<String>,
    pub heat_lot_reference: Option<String>,
    pub mill_cert_id: Option<String>,
    pub machine_id: Option<String>,
    pub program_id: Option<String>,
    pub disposition: Disposition,
    pub characteristics_required: u32,
    pub characteristics_measured: u32,
    pub characteristics_passed: u32,
    pub characteristics_failed: u32,
    pub characteristics_unaccounted: u32,
    /// SHA-256 (lowercase hex) of the bytes emitted at issuance. The bytes
    /// themselves are NEVER stored (ADR-0199 §D7); this hash is pinned into
    /// the audit chain and is what a re-render is checked against.
    pub rendered_sha256: Option<String>,
    pub renderer_version: Option<String>,
    pub issued_at_utc: Option<String>,
    pub issued_by: Option<String>,
    pub superseded_by_qcr_id: Option<String>,
    pub created_at: String,
    pub created_by: String,
    pub notes: Option<String>,
}

/// One `qc_report_lines` row — a frozen characteristic on a frozen report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QcReportLine {
    /// `qcrl_<ULID>`.
    pub qcrl_id: String,
    pub qcr_id: String,
    /// Stable render order.
    pub line_no: u32,
    /// `None` = a lot-level characteristic (ADR-0199 §Open Q10).
    pub part_serial: Option<String>,
    pub part_uid: Option<String>,
    pub characteristic_number: Option<String>,
    pub characteristic_name: String,
    pub characteristic_designator: Option<CharacteristicDesignator>,
    pub characteristic_type: CharacteristicType,
    pub inspection_method: Option<InspectionMethod>,
    pub sheet_zone: Option<String>,
    pub nominal_value: Option<f64>,
    pub upper_tol: Option<f64>,
    pub lower_tol: Option<f64>,
    pub units: Option<String>,
    /// `None` **iff** `accountability != Measured`. The renderer prints a
    /// blank cell; it never prints a zero.
    pub actual_value: Option<f64>,
    pub deviation: Option<f64>,
    pub verdict: Option<Verdict>,
    pub accountability: Accountability,
    /// The `qc_inspections` row this line froze, when one exists.
    pub qci_id: Option<String>,
    pub measured_at_utc: Option<String>,
    pub measured_by: Option<String>,
    pub probe_serial: Option<String>,
    pub created_at: String,
    /// Whether the source characteristic counted toward accountability.
    ///
    /// Not persisted as its own column — it is recoverable from the
    /// counts, and adding a column would mean two places to state one
    /// fact. Carried on the in-memory value so the renderer can mark an
    /// optional characteristic without re-reading the (mutable) plan.
    #[serde(default)]
    pub required: bool,
}

// ── The pure core ──────────────────────────────────────────────────

/// One serialised unit in the report's scope, as resolved from
/// `wo_part_marks` by the app layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportUnit {
    /// The unit's serial number, as marked.
    pub part_serial: String,
    /// The unit's part UID — what `qc_inspections.linked_part_uid` carries.
    pub part_uid: String,
}

/// A line as computed, before it is given an id and written.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftLine {
    pub line_no: u32,
    pub part_serial: Option<String>,
    pub part_uid: Option<String>,
    pub characteristic_number: Option<String>,
    pub characteristic_name: String,
    pub characteristic_designator: Option<CharacteristicDesignator>,
    pub characteristic_type: CharacteristicType,
    pub inspection_method: Option<InspectionMethod>,
    pub sheet_zone: Option<String>,
    pub nominal_value: Option<f64>,
    pub upper_tol: Option<f64>,
    pub lower_tol: Option<f64>,
    pub units: Option<String>,
    pub actual_value: Option<f64>,
    pub deviation: Option<f64>,
    pub verdict: Option<Verdict>,
    pub accountability: Accountability,
    pub qci_id: Option<String>,
    pub measured_at_utc: Option<String>,
    pub measured_by: Option<String>,
    pub probe_serial: Option<String>,
    /// Whether the source characteristic counts toward accountability.
    pub required: bool,
}

/// The five accountability counts (ADR-0199 §D3(d)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AccountabilityCounts {
    /// Lines generated from a characteristic that counts toward
    /// accountability. The denominator of the operator's "11 / 14" chip.
    pub required: u32,
    /// Lines that carry a real measurement (any verdict).
    pub measured: u32,
    /// Measured lines whose verdict is `Pass`.
    pub passed: u32,
    /// Measured lines whose verdict is a failing tier.
    pub failed: u32,
    /// **Required** lines with NO measurement. The number that forces
    /// `incomplete` and refuses the shipment.
    pub unaccounted: u32,
    /// Lines measured with a stale-calibration probe. Neither pass nor
    /// fail — a measurement from an uncalibrated probe is not evidence of
    /// conformity (ISO 9001 §7.1.5.2), so it forces `incomplete` too.
    pub calibration_stale: u32,
}

/// Whether a characteristic is reported once for the whole lot rather
/// than once per serialised unit (ADR-0199 §Open Q10).
///
/// Material and process characteristics — heat/lot conformity, coating
/// thickness, heat-treat certification — are facts about the batch, not
/// about an individual part. Attributing them per-serial would print the
/// same certificate fact N times and imply N independent verifications
/// that never happened.
fn is_lot_level(kind: CharacteristicType) -> bool {
    matches!(
        kind,
        CharacteristicType::Material | CharacteristicType::Process
    )
}

/// The characteristic type a plan row reports as. `None` in the column
/// means the plan predates ADR-0199, and every pre-ADR-0199 plan is a
/// nominal + tolerance band — i.e. dimensional.
fn effective_type(plan: &InspectionPlan) -> CharacteristicType {
    plan.characteristic_type.unwrap_or_default()
}

/// Deterministic characteristic order: balloon number when present
/// (numerically where it parses, so `"2"` sorts before `"10"`), then
/// feature name, then plan id as the final tiebreak.
///
/// Determinism here is load-bearing, not cosmetic: ADR-0199 §D7 pins the
/// SHA-256 of the rendered bytes into the audit chain, and a re-render in
/// 2033 must reproduce the 2026 bytes. An unstable line order would break
/// that pin on the first re-render.
fn characteristic_sort_key(plan: &InspectionPlan) -> (u8, i64, String, String, String) {
    match plan.characteristic_number.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => {
            // "7.2" → (7, "7.2"); "14" → (14, "14"); "A3" → (i64::MAX, "A3").
            let major = n
                .split(['.', '-'])
                .next()
                .and_then(|h| h.parse::<i64>().ok())
                .unwrap_or(i64::MAX);
            (
                0,
                major,
                n.to_string(),
                plan.feature_name.trim().to_string(),
                plan.plan_id.clone(),
            )
        }
        // Un-ballooned characteristics sort after ballooned ones.
        _ => (
            1,
            i64::MAX,
            String::new(),
            plan.feature_name.trim().to_string(),
            plan.plan_id.clone(),
        ),
    }
}

/// Pick the measurement that represents a (unit, characteristic) pair.
///
/// **The latest measurement wins**, by `measured_at_utc` then `qci_id`.
/// Rationale: the legitimate shop-floor sequence is measure → fail →
/// NCR → rework → re-measure → pass, and reporting the historical failure
/// as the part's condition would misreport a reworked part. The superseded
/// measurements are NOT lost — every one of them already appended its own
/// `qc.inspection_recorded` + `qc.inspection_failed` entry to the hash
/// chain when it happened, so "re-measure until it passes" remains visible
/// to an auditor walking the ledger even though the report shows the
/// accepted value.
fn latest_measurement<'a>(
    inspections: &'a [QcInspection],
    plan_id: &str,
    unit_uid: Option<&str>,
    lot_level: bool,
) -> Option<&'a QcInspection> {
    inspections
        .iter()
        .filter(|i| i.inspection_plan_id == plan_id)
        .filter(|i| {
            // A lot-level line accepts any measurement of the
            // characteristic; a per-serial line demands the measurement be
            // attributed to THAT unit.
            lot_level || i.linked_part_uid.as_deref() == unit_uid
        })
        .max_by(|a, b| {
            a.measured_at_utc
                .cmp(&b.measured_at_utc)
                .then_with(|| a.qci_id.cmp(&b.qci_id))
        })
}

fn line_from(
    line_no: u32,
    plan: &InspectionPlan,
    unit: Option<&ReportUnit>,
    measurement: Option<&QcInspection>,
) -> DraftLine {
    let ctype = effective_type(plan);
    match measurement {
        Some(m) => DraftLine {
            line_no,
            part_serial: unit.map(|u| u.part_serial.clone()),
            part_uid: unit
                .map(|u| u.part_uid.clone())
                .or_else(|| m.linked_part_uid.clone()),
            characteristic_number: plan.characteristic_number.clone(),
            // The MEASUREMENT's snapshot of the feature/tolerances is
            // used, not the plan's live values: the inspection row
            // already froze what the part was actually measured against
            // (V002's stated audit requirement), and the plan may have
            // been edited since. Using the plan here would let an
            // operator's tolerance edit retroactively change what a
            // measured part is reported as having conformed to.
            characteristic_name: m.feature_name.clone(),
            characteristic_designator: plan.characteristic_designator,
            characteristic_type: ctype,
            inspection_method: plan.inspection_method,
            sheet_zone: plan.sheet_zone.clone(),
            nominal_value: Some(m.nominal_value),
            upper_tol: Some(m.upper_tol),
            lower_tol: Some(m.lower_tol),
            units: Some(m.units.clone()),
            actual_value: Some(m.actual_value),
            deviation: Some(m.deviation),
            verdict: Some(m.verdict),
            accountability: Accountability::Measured,
            qci_id: Some(m.qci_id.clone()),
            measured_at_utc: Some(m.measured_at_utc.clone()),
            measured_by: Some(m.recorded_by.clone()),
            probe_serial: m.probe_serial.clone(),
            required: plan.counts_toward_accountability(),
        },
        // The accountability row. Everything measurement-shaped is `None`
        // — a blank cell on the printed page, never a zero.
        None => DraftLine {
            line_no,
            part_serial: unit.map(|u| u.part_serial.clone()),
            part_uid: unit.map(|u| u.part_uid.clone()),
            characteristic_number: plan.characteristic_number.clone(),
            characteristic_name: plan.feature_name.trim().to_string(),
            characteristic_designator: plan.characteristic_designator,
            characteristic_type: ctype,
            inspection_method: plan.inspection_method,
            sheet_zone: plan.sheet_zone.clone(),
            nominal_value: Some(plan.nominal_value),
            upper_tol: Some(plan.upper_tol),
            lower_tol: Some(plan.lower_tol),
            units: Some(plan.units.trim().to_string()),
            actual_value: None,
            deviation: None,
            verdict: None,
            accountability: Accountability::NotMeasured,
            qci_id: None,
            measured_at_utc: None,
            measured_by: None,
            probe_serial: None,
            required: plan.counts_toward_accountability(),
        },
    }
}

/// **The accountability computation** (ADR-0199 §D4). Pure.
///
/// Walks the PLAN side and joins measurements onto it:
///
/// - Per-serial characteristics produce one line per unit in `units`.
/// - Lot-level characteristics ([`is_lot_level`]) produce exactly one
///   line, with `part_serial = None`.
/// - When `units` is empty (a WO with no marked parts) every
///   characteristic degrades to a single lot-level line rather than
///   producing nothing — a report with zero rows would be vacuously
///   "complete", which is the failure this whole module exists to
///   prevent.
///
/// Archived and disabled plans are excluded by the caller's query, not
/// here; this function reports exactly the characteristics it is given.
///
/// Line order is unit-major (unit 1's full characteristic list, then unit
/// 2's), with lot-level characteristics last. Deterministic — see
/// [`characteristic_sort_key`].
pub fn build_report_lines(
    plans: &[InspectionPlan],
    inspections: &[QcInspection],
    units: &[ReportUnit],
) -> Vec<DraftLine> {
    let mut ordered: Vec<&InspectionPlan> = plans.iter().collect();
    ordered.sort_by_key(|p| characteristic_sort_key(p));

    let (lot_plans, serial_plans): (Vec<&InspectionPlan>, Vec<&InspectionPlan>) = ordered
        .into_iter()
        .partition(|p| is_lot_level(effective_type(p)));

    let mut lines = Vec::new();
    let mut line_no = 0u32;

    if units.is_empty() {
        // No marked units: every characteristic reports once, at lot level.
        for plan in serial_plans.iter().chain(lot_plans.iter()) {
            line_no += 1;
            let m = latest_measurement(inspections, &plan.plan_id, None, true);
            lines.push(line_from(line_no, plan, None, m));
        }
        return lines;
    }

    for unit in units {
        for plan in &serial_plans {
            line_no += 1;
            let m = latest_measurement(
                inspections,
                &plan.plan_id,
                Some(unit.part_uid.as_str()),
                false,
            );
            lines.push(line_from(line_no, plan, Some(unit), m));
        }
    }
    for plan in &lot_plans {
        line_no += 1;
        let m = latest_measurement(inspections, &plan.plan_id, None, true);
        lines.push(line_from(line_no, plan, None, m));
    }
    lines
}

/// Tally the five accountability counts over computed lines. Pure.
pub fn summarise(lines: &[DraftLine]) -> AccountabilityCounts {
    let mut c = AccountabilityCounts::default();
    for l in lines {
        if l.required {
            c.required += 1;
        }
        match l.accountability {
            Accountability::Measured => {
                c.measured += 1;
                match l.verdict {
                    Some(Verdict::Pass) => c.passed += 1,
                    Some(v) if v.is_failing() => c.failed += 1,
                    Some(Verdict::CalibrationStale) => c.calibration_stale += 1,
                    // A measured line with no verdict cannot be produced by
                    // `line_from` (the two are set together), but a
                    // hand-edited row could. Count it as neither pass nor
                    // fail and let `compute_disposition` refuse to call the
                    // report complete rather than silently accepting it.
                    _ => {}
                }
            }
            Accountability::NotMeasured => {
                if l.required {
                    c.unaccounted += 1;
                }
            }
            Accountability::NotApplicable => {}
        }
    }
    c
}

/// **The disposition rule** (ADR-0199 §D4). Computed, never
/// operator-typed. Pure.
///
/// ```text
/// any line failed                                   → reject
/// any required characteristic unaccounted-for
///   or any line CAL-STALE                           → incomplete
/// all required accounted for, all pass,
///   but an NCR is open against a listed part UID    → accept_with_ncr
/// otherwise                                         → accept
/// ```
///
/// `open_ncr_against_reported_part` is supplied by the app layer —
/// `aberp-qa` cannot depend on `apps/aberp`'s NCR module, the same
/// boundary `record_inspection` already respects by returning
/// `auto_ncr_recommended` instead of creating the NCR itself.
///
/// A measured-but-verdict-less line (only reachable by tampering) is
/// counted in neither `passed` nor `failed`, so it lands in the final
/// arm's `measured != passed` check and yields `Incomplete`.
pub fn compute_disposition(
    counts: AccountabilityCounts,
    open_ncr_against_reported_part: bool,
) -> Disposition {
    if counts.failed > 0 {
        return Disposition::Reject;
    }
    if counts.unaccounted > 0 || counts.calibration_stale > 0 {
        return Disposition::Incomplete;
    }
    // Every measured line must have actually passed. This catches the
    // tampered "measured, no verdict" row rather than treating it as an
    // accept.
    if counts.measured != counts.passed {
        return Disposition::Incomplete;
    }
    if open_ncr_against_reported_part {
        return Disposition::AcceptWithNcr;
    }
    Disposition::Accept
}

/// Render the human-readable serial range for the report header
/// (`"SN-001 … SN-012 (12 units)"`). Pure; snapshotted onto the row.
pub fn serial_range_of(units: &[ReportUnit]) -> Option<String> {
    if units.is_empty() {
        return None;
    }
    let mut serials: Vec<&str> = units.iter().map(|u| u.part_serial.as_str()).collect();
    serials.sort_unstable();
    let n = serials.len();
    if n == 1 {
        return Some(serials[0].to_string());
    }
    Some(format!("{} … {} ({} units)", serials[0], serials[n - 1], n))
}

// ── Writes ─────────────────────────────────────────────────────────

fn rfc3339(ts: OffsetDateTime) -> Result<String, QcError> {
    ts.format(&Rfc3339)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("format timestamp: {e}")))
}

fn emit(
    tx: &Transaction<'_>,
    ctx: &QcWriteContext<'_>,
    kind: EventKind,
    payload: serde_json::Value,
    idempotency_key: String,
) -> Result<(), QcError> {
    let kind_str = kind.as_str();
    append_in_tx(
        tx,
        ctx.ledger_meta,
        kind,
        serde_json::to_vec(&payload).expect("serialize qc report payload"),
        ctx.ledger_actor.clone(),
        Some(idempotency_key),
    )
    .map_err(|e| QcError::Storage(anyhow::anyhow!("audit append {kind_str}: {e}")))?;
    Ok(())
}

/// Traceability the app layer resolved once, to be snapshotted onto the
/// report header (ADR-0199 §D3(c)). Every field is available in the tree
/// today; none is re-derived at render time.
#[derive(Debug, Clone, Default)]
pub struct ReportTraceability {
    pub source_quote_id: Option<String>,
    pub drawing_number: Option<String>,
    pub drawing_rev: Option<String>,
    pub heat_lot_reference: Option<String>,
    pub mill_cert_id: Option<String>,
    pub machine_id: Option<String>,
    pub program_id: Option<String>,
    pub notes: Option<String>,
}

/// Everything [`freeze_report`] needs.
#[derive(Debug)]
pub struct FreezeReportInputs<'a> {
    pub report_kind: QcReportKind,
    pub template: QcReportTemplate,
    pub wo_id: &'a str,
    pub product_id: &'a str,
    pub partner_id: &'a str,
    /// The characteristics in scope — enabled, non-archived plans for the
    /// product. The caller filters; this function reports what it is given.
    pub plans: &'a [InspectionPlan],
    /// Every measurement for the WO.
    pub inspections: &'a [QcInspection],
    /// The serialised units in scope, from `wo_part_marks`.
    pub units: &'a [ReportUnit],
    /// Supplied by the app layer (see [`compute_disposition`]).
    pub open_ncr_against_reported_part: bool,
    pub traceability: ReportTraceability,
    pub created_by: &'a str,
}

/// Compute the lines, freeze them, and write the `drafted` report inside
/// the caller's transaction. Emits one `qcr.report_drafted`.
///
/// The report is NOT issued here: issuance is the separate act that
/// renders the bytes and pins their hash ([`issue_report`]). Splitting
/// the two means the operator can preview an incomplete report and see
/// exactly which characteristics are missing, without a half-built
/// document ever acquiring a chain-pinned identity.
pub fn freeze_report(
    tx: &Transaction<'_>,
    ctx: &QcWriteContext<'_>,
    inputs: FreezeReportInputs<'_>,
    now: OffsetDateTime,
) -> Result<(QcReport, Vec<QcReportLine>), QcError> {
    if !inputs.template.permits(inputs.report_kind) {
        return Err(QcError::Validation(format!(
            "template {} does not produce a {} report",
            inputs.template.as_str(),
            inputs.report_kind.as_str()
        )));
    }

    let draft_lines = build_report_lines(inputs.plans, inputs.inspections, inputs.units);
    let counts = summarise(&draft_lines);
    let disposition = compute_disposition(counts, inputs.open_ncr_against_reported_part);

    let qcr_id = format!("qcr_{}", Ulid::new());
    let created_at = rfc3339(now)?;
    let report_number = allocate_report_number(tx, ctx.tenant, now)?;
    // The report states what it ACCOUNTED FOR, not what the work order
    // planned. The WO's `qty_target` is deliberately not read: claiming a
    // quantity the document did not enumerate would be the same class of
    // overstatement the accountability rule exists to prevent. With no
    // marked units the report degrades to one lot-level document.
    let qty_reported = if inputs.units.is_empty() {
        1
    } else {
        inputs.units.len() as u32
    };
    let serial_range = serial_range_of(inputs.units);
    let t = &inputs.traceability;

    tx.execute(
        "INSERT INTO qc_reports (
            qcr_id, tenant_id, report_number, report_kind, template, state,
            wo_id, product_id, dsp_id, partner_id, source_quote_id,
            drawing_number, drawing_rev, qty_reported, serial_range,
            heat_lot_reference, mill_cert_id, machine_id, program_id, disposition,
            characteristics_required, characteristics_measured, characteristics_passed,
            characteristics_failed, characteristics_unaccounted,
            rendered_sha256, renderer_version, issued_at_utc, issued_by,
            superseded_by_qcr_id, created_at, created_by, notes
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                   ?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, NULL, ?, ?, ?);",
        params![
            &qcr_id,
            ctx.tenant,
            &report_number,
            inputs.report_kind.as_str(),
            inputs.template.as_str(),
            QcReportState::Drafted.as_str(),
            inputs.wo_id.trim(),
            inputs.product_id.trim(),
            inputs.partner_id.trim(),
            t.source_quote_id.as_deref(),
            t.drawing_number.as_deref(),
            t.drawing_rev.as_deref(),
            qty_reported,
            serial_range.as_deref(),
            t.heat_lot_reference.as_deref(),
            t.mill_cert_id.as_deref(),
            t.machine_id.as_deref(),
            t.program_id.as_deref(),
            disposition.as_str(),
            counts.required,
            counts.measured,
            counts.passed,
            counts.failed,
            counts.unaccounted,
            &created_at,
            inputs.created_by.trim(),
            t.notes.as_deref(),
        ],
    )
    .map_err(|e| QcError::Storage(anyhow::anyhow!("INSERT qc_reports: {e}")))?;

    let mut written = Vec::with_capacity(draft_lines.len());
    for l in &draft_lines {
        let qcrl_id = format!("qcrl_{}", Ulid::new());
        tx.execute(
            "INSERT INTO qc_report_lines (
                qcrl_id, tenant_id, qcr_id, line_no, part_serial, part_uid,
                characteristic_number, characteristic_name, characteristic_designator,
                characteristic_type, inspection_method, sheet_zone,
                nominal_value, upper_tol, lower_tol, units,
                actual_value, deviation, verdict, accountability,
                qci_id, measured_at_utc, measured_by, probe_serial, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            params![
                &qcrl_id,
                ctx.tenant,
                &qcr_id,
                l.line_no,
                l.part_serial.as_deref(),
                l.part_uid.as_deref(),
                l.characteristic_number.as_deref(),
                l.characteristic_name.as_str(),
                l.characteristic_designator.map(|v| v.as_str()),
                l.characteristic_type.as_str(),
                l.inspection_method.map(|v| v.as_str()),
                l.sheet_zone.as_deref(),
                l.nominal_value,
                l.upper_tol,
                l.lower_tol,
                l.units.as_deref(),
                l.actual_value,
                l.deviation,
                l.verdict.map(|v| v.as_str()),
                l.accountability.as_str(),
                l.qci_id.as_deref(),
                l.measured_at_utc.as_deref(),
                l.measured_by.as_deref(),
                l.probe_serial.as_deref(),
                &created_at,
            ],
        )
        .map_err(|e| QcError::Storage(anyhow::anyhow!("INSERT qc_report_lines: {e}")))?;
        written.push(QcReportLine {
            qcrl_id,
            qcr_id: qcr_id.clone(),
            line_no: l.line_no,
            part_serial: l.part_serial.clone(),
            part_uid: l.part_uid.clone(),
            characteristic_number: l.characteristic_number.clone(),
            characteristic_name: l.characteristic_name.clone(),
            characteristic_designator: l.characteristic_designator,
            characteristic_type: l.characteristic_type,
            inspection_method: l.inspection_method,
            sheet_zone: l.sheet_zone.clone(),
            nominal_value: l.nominal_value,
            upper_tol: l.upper_tol,
            lower_tol: l.lower_tol,
            units: l.units.clone(),
            actual_value: l.actual_value,
            deviation: l.deviation,
            verdict: l.verdict,
            accountability: l.accountability,
            qci_id: l.qci_id.clone(),
            measured_at_utc: l.measured_at_utc.clone(),
            measured_by: l.measured_by.clone(),
            probe_serial: l.probe_serial.clone(),
            created_at: created_at.clone(),
            required: l.required,
        });
    }

    emit(
        tx,
        ctx,
        EventKind::QcReportDrafted,
        json!({
            "qcr_id": qcr_id,
            "report_number": report_number,
            "report_kind": inputs.report_kind.as_str(),
            "template": inputs.template.as_str(),
            "wo_id": inputs.wo_id.trim(),
            "product_id": inputs.product_id.trim(),
            "partner_id": inputs.partner_id.trim(),
            "disposition": disposition.as_str(),
            "characteristics_required": counts.required,
            "characteristics_measured": counts.measured,
            "characteristics_passed": counts.passed,
            "characteristics_failed": counts.failed,
            "characteristics_unaccounted": counts.unaccounted,
            "characteristics_calibration_stale": counts.calibration_stale,
        }),
        format!("qcr_drafted:{qcr_id}"),
    )?;

    let report = get_report_in_tx(tx, ctx.tenant, &qcr_id)?
        .ok_or_else(|| QcError::Storage(anyhow::anyhow!("qc report vanished after insert")))?;
    Ok((report, written))
}

/// Allocate the operator-facing report number, `QCR-<YYYY>-<NNNN>`.
///
/// Allocated in code, not by a SQL sequence ([[no-sql-specific]]), and
/// safe because every write path runs under the ONE shared
/// `aberp_db::Handle` writer — the count and the insert are serialised by
/// the same exclusive guard and ride the same transaction.
fn allocate_report_number(
    tx: &Transaction<'_>,
    tenant: &str,
    now: OffsetDateTime,
) -> Result<String, QcError> {
    let year = now.year();
    let prefix = format!("QCR-{year}-");
    let like = format!("{prefix}%");
    let n: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM qc_reports WHERE tenant_id = ? AND report_number LIKE ?;",
            params![tenant, &like],
            |r| r.get(0),
        )
        .map_err(|e| QcError::Storage(anyhow::anyhow!("count qc_reports for numbering: {e}")))?;
    Ok(format!("{prefix}{:04}", n.max(0) + 1))
}

/// Issue a drafted report: pin the rendered bytes' SHA-256 and flip to
/// `issued`. Emits one `qcr.report_issued` carrying the hash, the
/// renderer version, the full accountability counts and the traceability
/// keys (ADR-0199 §D7).
///
/// **The bytes are not stored.** The report re-renders deterministically
/// from the frozen lines, and the chain proves the bytes anyone re-renders
/// are the bytes that were issued.
///
/// Refuses a non-`drafted` report: re-issuing would mint a second hash for
/// a document that already has a chain-pinned identity.
#[allow(clippy::too_many_arguments)]
pub fn issue_report(
    tx: &Transaction<'_>,
    ctx: &QcWriteContext<'_>,
    qcr_id: &str,
    rendered_sha256: &str,
    renderer_version: &str,
    issued_by: &str,
    now: OffsetDateTime,
) -> Result<QcReport, QcError> {
    let report = get_report_in_tx(tx, ctx.tenant, qcr_id)?.ok_or(QcError::NotFound)?;
    if report.state != QcReportState::Drafted {
        return Err(QcError::Validation(format!(
            "report {qcr_id} is {} — only a drafted report can be issued \
             (a mistake is corrected by a new report, never by re-issuing this one)",
            report.state.as_str()
        )));
    }
    let issued_at = rfc3339(now)?;
    tx.execute(
        "UPDATE qc_reports SET state = ?, rendered_sha256 = ?, renderer_version = ?,
                issued_at_utc = ?, issued_by = ?
         WHERE tenant_id = ? AND qcr_id = ?;",
        params![
            QcReportState::Issued.as_str(),
            rendered_sha256.trim(),
            renderer_version.trim(),
            &issued_at,
            issued_by.trim(),
            ctx.tenant,
            qcr_id,
        ],
    )
    .map_err(|e| QcError::Storage(anyhow::anyhow!("UPDATE qc_reports (issue): {e}")))?;

    emit(
        tx,
        ctx,
        EventKind::QcReportIssued,
        json!({
            "qcr_id": qcr_id,
            "report_number": report.report_number,
            "report_kind": report.report_kind.as_str(),
            "template": report.template.as_str(),
            "wo_id": report.wo_id,
            "product_id": report.product_id,
            "partner_id": report.partner_id,
            "drawing_number": report.drawing_number,
            "drawing_rev": report.drawing_rev,
            "serial_range": report.serial_range,
            "qty_reported": report.qty_reported,
            "heat_lot_reference": report.heat_lot_reference,
            "mill_cert_id": report.mill_cert_id,
            "machine_id": report.machine_id,
            "program_id": report.program_id,
            "disposition": report.disposition.as_str(),
            "characteristics_required": report.characteristics_required,
            "characteristics_measured": report.characteristics_measured,
            "characteristics_passed": report.characteristics_passed,
            "characteristics_failed": report.characteristics_failed,
            "characteristics_unaccounted": report.characteristics_unaccounted,
            "rendered_sha256": rendered_sha256.trim(),
            "renderer_version": renderer_version.trim(),
            "issued_by": issued_by.trim(),
            "issued_at_utc": issued_at,
        }),
        format!("qcr_issued:{qcr_id}"),
    )?;

    get_report_in_tx(tx, ctx.tenant, qcr_id)?
        .ok_or_else(|| QcError::Storage(anyhow::anyhow!("qc report vanished after issue")))
}

/// Bind every issued, unbound report for `wo_id` to `dsp_id`, in the
/// caller's transaction. Emits one `qcr.report_attached_to_shipment` per
/// bound report. Returns the bound ids.
///
/// This is what the injected `ShipmentDocumentBinder` calls from inside
/// `mark_shipped`'s single transaction (ADR-0199 §D6): a shipment that
/// commits while its report binding rolls back is a shipped part with no
/// attached QC record, and a bound report on a rolled-back shipment is a
/// document claiming a delivery that never happened. One transaction,
/// both or neither.
pub fn bind_reports_to_dispatch(
    tx: &Transaction<'_>,
    ctx: &QcWriteContext<'_>,
    wo_id: &str,
    dsp_id: &str,
) -> Result<Vec<String>, QcError> {
    // EVERY issued, unbound report for the WO is bound — including one whose
    // disposition does not release, if such a report was issued and then
    // superseded by a corrected one that does. That is deliberate: the
    // shipment's document set is the faithful record of what was issued for
    // these parts, and filtering the unflattering one out of the binding
    // would leave the chain saying a report exists while the shipment's own
    // record does not mention it. Whether a non-releasing report may ship at
    // all is the GATE's decision, made before this transaction opens.
    let candidates = query_reports_tx(
        tx,
        "WHERE tenant_id = ? AND wo_id = ? AND state = ? AND dsp_id IS NULL
         ORDER BY created_at ASC, qcr_id ASC",
        params![ctx.tenant, wo_id.trim(), QcReportState::Issued.as_str()],
    )?;
    let mut bound = Vec::with_capacity(candidates.len());
    for r in candidates {
        tx.execute(
            "UPDATE qc_reports SET dsp_id = ? WHERE tenant_id = ? AND qcr_id = ?;",
            params![dsp_id.trim(), ctx.tenant, &r.qcr_id],
        )
        .map_err(|e| QcError::Storage(anyhow::anyhow!("UPDATE qc_reports (bind): {e}")))?;
        emit(
            tx,
            ctx,
            EventKind::QcReportAttachedToShipment,
            json!({
                "qcr_id": r.qcr_id,
                "report_number": r.report_number,
                "report_kind": r.report_kind.as_str(),
                "dsp_id": dsp_id.trim(),
                "wo_id": wo_id.trim(),
                "disposition": r.disposition.as_str(),
            }),
            format!("qcr_attached:{}:{}", r.qcr_id, dsp_id.trim()),
        )?;
        bound.push(r.qcr_id);
    }
    Ok(bound)
}

/// Record a re-render. Emits `qcr.report_rendered` with the SHA of the
/// bytes just produced, so a divergence from the issued SHA is detectable
/// in the chain without anyone storing a byte (ADR-0199 §D7).
/// `matches_issued` is `None` for a DRAFT — it has no pinned hash, so there
/// is nothing to match, and recording `false` there would flag every
/// legitimate preview as a divergence and teach a reader to ignore the
/// signal. `Some(false)` is the real tamper signal.
pub fn record_render(
    tx: &Transaction<'_>,
    ctx: &QcWriteContext<'_>,
    qcr_id: &str,
    sha256: &str,
    renderer_version: &str,
    matches_issued: Option<bool>,
) -> Result<(), QcError> {
    emit(
        tx,
        ctx,
        EventKind::QcReportRendered,
        json!({
            "qcr_id": qcr_id,
            "rendered_sha256": sha256.trim(),
            "renderer_version": renderer_version.trim(),
            "matches_issued_sha256": matches_issued,
        }),
        format!("qcr_rendered:{}:{}", qcr_id, sha256.trim()),
    )
}

/// Void a report. There is no delete path — `voided` and
/// `superseded_by_qcr_id` are the only ways a report stops being current,
/// matching the invoice posture where a mistake is corrected by a new
/// document.
pub fn void_report(
    tx: &Transaction<'_>,
    ctx: &QcWriteContext<'_>,
    qcr_id: &str,
    reason: &str,
    superseded_by: Option<&str>,
) -> Result<(), QcError> {
    let report = get_report_in_tx(tx, ctx.tenant, qcr_id)?.ok_or(QcError::NotFound)?;
    let new_state = if superseded_by.is_some() {
        QcReportState::Superseded
    } else {
        QcReportState::Voided
    };
    tx.execute(
        "UPDATE qc_reports SET state = ?, superseded_by_qcr_id = ?
         WHERE tenant_id = ? AND qcr_id = ?;",
        params![
            new_state.as_str(),
            superseded_by.map(str::trim),
            ctx.tenant,
            qcr_id
        ],
    )
    .map_err(|e| QcError::Storage(anyhow::anyhow!("UPDATE qc_reports (void): {e}")))?;
    emit(
        tx,
        ctx,
        EventKind::QcReportVoided,
        json!({
            "qcr_id": qcr_id,
            "report_number": report.report_number,
            "reason": reason.trim(),
            "superseded_by_qcr_id": superseded_by.map(str::trim),
            "new_state": new_state.as_str(),
        }),
        format!("qcr_voided:{qcr_id}"),
    )
}

// ── Reads ──────────────────────────────────────────────────────────

const REPORT_COLUMNS: &str = "qcr_id, report_number, report_kind, template, state,
     wo_id, product_id, dsp_id, partner_id, source_quote_id, drawing_number, drawing_rev,
     qty_reported, serial_range, heat_lot_reference, mill_cert_id, machine_id, program_id,
     disposition, characteristics_required, characteristics_measured, characteristics_passed,
     characteristics_failed, characteristics_unaccounted, rendered_sha256, renderer_version,
     issued_at_utc, issued_by, superseded_by_qcr_id, created_at, created_by, notes";

/// Fetch one report by id (tenant-scoped).
pub fn get_report(
    conn: &Connection,
    tenant: &str,
    qcr_id: &str,
) -> Result<Option<QcReport>, QcError> {
    Ok(query_reports(
        conn,
        "WHERE tenant_id = ? AND qcr_id = ?",
        params![tenant, qcr_id],
    )?
    .into_iter()
    .next())
}

/// Every report for a work order, newest first.
pub fn list_reports_for_wo(
    conn: &Connection,
    tenant: &str,
    wo_id: &str,
) -> Result<Vec<QcReport>, QcError> {
    query_reports(
        conn,
        "WHERE tenant_id = ? AND wo_id = ? ORDER BY created_at DESC, qcr_id DESC",
        params![tenant, wo_id],
    )
}

/// Every report bound to a dispatch, oldest first (the order they were
/// bound in, which is the order they belong in the box).
pub fn list_reports_for_dispatch(
    conn: &Connection,
    tenant: &str,
    dsp_id: &str,
) -> Result<Vec<QcReport>, QcError> {
    query_reports(
        conn,
        "WHERE tenant_id = ? AND dsp_id = ? ORDER BY created_at ASC, qcr_id ASC",
        params![tenant, dsp_id],
    )
}

/// The frozen lines of a report, in render order.
pub fn list_report_lines(
    conn: &Connection,
    tenant: &str,
    qcr_id: &str,
) -> Result<Vec<QcReportLine>, QcError> {
    let sql = format!(
        "SELECT {LINE_COLUMNS} FROM qc_report_lines
         WHERE tenant_id = ? AND qcr_id = ? ORDER BY line_no ASC, qcrl_id ASC;"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("prepare report-lines query: {e}")))?;
    let rows = stmt
        .query_map(params![tenant, qcr_id], parse_line_row)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("query report lines: {e}")))?;
    let mut acc = Vec::new();
    for r in rows {
        let parsed = r.map_err(|e| QcError::Storage(anyhow::anyhow!("read line row: {e}")))?;
        acc.push(parsed.map_err(QcError::Storage)?);
    }
    Ok(acc)
}

const LINE_COLUMNS: &str = "qcrl_id, qcr_id, line_no, part_serial, part_uid,
     characteristic_number, characteristic_name, characteristic_designator,
     characteristic_type, inspection_method, sheet_zone, nominal_value, upper_tol,
     lower_tol, units, actual_value, deviation, verdict, accountability, qci_id,
     measured_at_utc, measured_by, probe_serial, created_at";

fn get_report_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    qcr_id: &str,
) -> Result<Option<QcReport>, QcError> {
    Ok(query_reports_tx(
        tx,
        "WHERE tenant_id = ? AND qcr_id = ?",
        params![tenant, qcr_id],
    )?
    .into_iter()
    .next())
}

fn query_reports(
    conn: &Connection,
    where_order: &str,
    p: impl duckdb::Params,
) -> Result<Vec<QcReport>, QcError> {
    let sql = format!("SELECT {REPORT_COLUMNS} FROM qc_reports {where_order};");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("prepare reports query: {e}")))?;
    collect_reports(
        stmt.query_map(p, parse_report_row)
            .map_err(|e| QcError::Storage(anyhow::anyhow!("query reports: {e}")))?,
    )
}

fn query_reports_tx(
    tx: &Transaction<'_>,
    where_order: &str,
    p: impl duckdb::Params,
) -> Result<Vec<QcReport>, QcError> {
    let sql = format!("SELECT {REPORT_COLUMNS} FROM qc_reports {where_order};");
    let mut stmt = tx
        .prepare(&sql)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("prepare reports query (tx): {e}")))?;
    collect_reports(
        stmt.query_map(p, parse_report_row)
            .map_err(|e| QcError::Storage(anyhow::anyhow!("query reports (tx): {e}")))?,
    )
}

fn collect_reports<I>(rows: I) -> Result<Vec<QcReport>, QcError>
where
    I: Iterator<Item = duckdb::Result<Result<QcReport, anyhow::Error>>>,
{
    let mut acc = Vec::new();
    for r in rows {
        let parsed = r.map_err(|e| QcError::Storage(anyhow::anyhow!("read report row: {e}")))?;
        acc.push(parsed.map_err(QcError::Storage)?);
    }
    Ok(acc)
}

/// Decode a `qc_reports` row. Every closed-vocab column is decoded
/// STRICTLY — an unknown token fails the read.
///
/// This is the opposite posture from `plans::vocab_opt`, deliberately: a
/// plan's designator is cosmetic metadata on a mutable reference row,
/// whereas a frozen report's `state` / `disposition` / `report_kind` IS
/// the evidence. Silently coercing a corrupted `disposition` to something
/// readable would let a tampered row present as shippable.
fn parse_report_row(row: &duckdb::Row<'_>) -> duckdb::Result<Result<QcReport, anyhow::Error>> {
    let kind_str: String = row.get(2)?;
    let template_str: String = row.get(3)?;
    let state_str: String = row.get(4)?;
    let disposition_str: String = row.get(18)?;
    Ok((|| {
        Ok(QcReport {
            qcr_id: row.get(0)?,
            report_number: row.get(1)?,
            report_kind: QcReportKind::from_storage_str(&kind_str)
                .map_err(|e| anyhow::anyhow!("{e}: {kind_str:?}"))?,
            template: QcReportTemplate::from_storage_str(&template_str)
                .map_err(|e| anyhow::anyhow!("{e}: {template_str:?}"))?,
            state: QcReportState::from_storage_str(&state_str)
                .map_err(|e| anyhow::anyhow!("{e}: {state_str:?}"))?,
            wo_id: row.get(5)?,
            product_id: row.get(6)?,
            dsp_id: row.get(7)?,
            partner_id: row.get(8)?,
            source_quote_id: row.get(9)?,
            drawing_number: row.get(10)?,
            drawing_rev: row.get(11)?,
            qty_reported: row.get::<_, i32>(12)?.max(0) as u32,
            serial_range: row.get(13)?,
            heat_lot_reference: row.get(14)?,
            mill_cert_id: row.get(15)?,
            machine_id: row.get(16)?,
            program_id: row.get(17)?,
            disposition: Disposition::from_storage_str(&disposition_str)
                .map_err(|e| anyhow::anyhow!("{e}: {disposition_str:?}"))?,
            characteristics_required: row.get::<_, i32>(19)?.max(0) as u32,
            characteristics_measured: row.get::<_, i32>(20)?.max(0) as u32,
            characteristics_passed: row.get::<_, i32>(21)?.max(0) as u32,
            characteristics_failed: row.get::<_, i32>(22)?.max(0) as u32,
            characteristics_unaccounted: row.get::<_, i32>(23)?.max(0) as u32,
            rendered_sha256: row.get(24)?,
            renderer_version: row.get(25)?,
            issued_at_utc: row.get(26)?,
            issued_by: row.get(27)?,
            superseded_by_qcr_id: row.get(28)?,
            created_at: row.get(29)?,
            created_by: row.get(30)?,
            notes: row.get(31)?,
        })
    })())
}

/// Decode a `qc_report_lines` row. Strict on every vocabulary, for the
/// reason given on [`parse_report_row`].
///
/// `required` is recovered from the row's own shape rather than stored:
/// a `not_measured` line only exists on a report at all because the
/// characteristic was in scope, and the counts on the header carry the
/// authoritative arithmetic. The renderer uses it only to mark optional
/// characteristics.
fn parse_line_row(row: &duckdb::Row<'_>) -> duckdb::Result<Result<QcReportLine, anyhow::Error>> {
    let designator_str: Option<String> = row.get(7)?;
    let ctype_str: String = row.get(8)?;
    let method_str: Option<String> = row.get(9)?;
    let verdict_str: Option<String> = row.get(17)?;
    let accountability_str: String = row.get(18)?;
    Ok((|| {
        let designator = match designator_str.as_deref() {
            Some(s) => Some(
                CharacteristicDesignator::from_storage_str(s)
                    .map_err(|e| anyhow::anyhow!("{e}: {s:?}"))?,
            ),
            None => None,
        };
        let method = match method_str.as_deref() {
            Some(s) => Some(
                InspectionMethod::from_storage_str(s).map_err(|e| anyhow::anyhow!("{e}: {s:?}"))?,
            ),
            None => None,
        };
        let verdict = match verdict_str.as_deref() {
            Some(s) => {
                Some(Verdict::from_storage_str(s).map_err(|e| anyhow::anyhow!("{e}: {s:?}"))?)
            }
            None => None,
        };
        Ok(QcReportLine {
            qcrl_id: row.get(0)?,
            qcr_id: row.get(1)?,
            line_no: row.get::<_, i32>(2)?.max(0) as u32,
            part_serial: row.get(3)?,
            part_uid: row.get(4)?,
            characteristic_number: row.get(5)?,
            characteristic_name: row.get(6)?,
            characteristic_designator: designator,
            characteristic_type: CharacteristicType::from_storage_str(&ctype_str)
                .map_err(|e| anyhow::anyhow!("{e}: {ctype_str:?}"))?,
            inspection_method: method,
            sheet_zone: row.get(10)?,
            nominal_value: row.get(11)?,
            upper_tol: row.get(12)?,
            lower_tol: row.get(13)?,
            units: row.get(14)?,
            actual_value: row.get(15)?,
            deviation: row.get(16)?,
            verdict,
            accountability: Accountability::from_storage_str(&accountability_str)
                .map_err(|e| anyhow::anyhow!("{e}: {accountability_str:?}"))?,
            qci_id: row.get(19)?,
            measured_at_utc: row.get(20)?,
            measured_by: row.get(21)?,
            probe_serial: row.get(22)?,
            created_at: row.get(23)?,
            required: true,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qc::plans::InspectionPlan;
    use crate::qc::vocab::{CharacteristicType, InspectionMethod};

    fn plan(
        id: &str,
        feature: &str,
        number: Option<&str>,
        required: Option<bool>,
    ) -> InspectionPlan {
        InspectionPlan {
            plan_id: id.into(),
            product_id: "prd_bracket".into(),
            feature_name: feature.into(),
            nominal_value: 25.0,
            upper_tol: 0.05,
            lower_tol: -0.05,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            created_at: "2026-08-01T00:00:00Z".into(),
            archived_at: None,
            characteristic_number: number.map(str::to_string),
            characteristic_designator: None,
            characteristic_type: Some(CharacteristicType::Dimensional),
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: None,
            is_required: required,
        }
    }

    fn lot_plan(id: &str, feature: &str) -> InspectionPlan {
        let mut p = plan(id, feature, Some("90"), Some(true));
        p.characteristic_type = Some(CharacteristicType::Material);
        p
    }

    fn measurement(
        qci: &str,
        plan_id: &str,
        part_uid: Option<&str>,
        actual: f64,
        verdict: Verdict,
        at: &str,
    ) -> QcInspection {
        QcInspection {
            qci_id: qci.into(),
            measured_at_utc: at.into(),
            source: crate::qc::inspections::QcSource::Manual,
            source_event_id: None,
            inspection_plan_id: plan_id.into(),
            feature_name: "Bore D".into(),
            nominal_value: 25.0,
            upper_tol: 0.05,
            lower_tol: -0.05,
            units: "mm".into(),
            actual_value: actual,
            deviation: actual - 25.0,
            verdict,
            probe_serial: Some("RMP600-007".into()),
            last_calibration_at_utc: None,
            calibration_stale_at_event: verdict == Verdict::CalibrationStale,
            auto_ncr_id: None,
            linked_part_uid: part_uid.map(str::to_string),
            linked_heat_lot: None,
            linked_wo_id: Some("wo_1".into()),
            recorded_by: "ervin".into(),
            created_at: at.into(),
        }
    }

    fn unit(serial: &str, uid: &str) -> ReportUnit {
        ReportUnit {
            part_serial: serial.into(),
            part_uid: uid.into(),
        }
    }

    // ── THE safety-critical behaviour (ADR-0199 §D4 / §AC2) ──────────

    /// **A missing required characteristic is a PRINTED ROW, not an
    /// omission, and it makes the report `incomplete`.**
    ///
    /// This is the single most important assertion in the crate. A report
    /// that lists only what was measured looks complete precisely because
    /// the rows that would have failed are absent.
    #[test]
    fn a_missing_required_characteristic_becomes_a_row_and_forces_incomplete() {
        let plans = vec![
            plan("p1", "Bore D", Some("1"), Some(true)),
            plan("p2", "Face Z", Some("2"), Some(true)),
            plan("p3", "Slot W", Some("3"), Some(true)),
            plan("p4", "Hole X", Some("4"), Some(true)),
        ];
        // Three of four measured, on the one unit.
        let inspections = vec![
            measurement(
                "q1",
                "p1",
                Some("uid1"),
                25.0,
                Verdict::Pass,
                "2026-08-02T10:00:00Z",
            ),
            measurement(
                "q2",
                "p2",
                Some("uid1"),
                25.0,
                Verdict::Pass,
                "2026-08-02T10:01:00Z",
            ),
            measurement(
                "q3",
                "p3",
                Some("uid1"),
                25.0,
                Verdict::Pass,
                "2026-08-02T10:02:00Z",
            ),
        ];
        let units = vec![unit("SN-001", "uid1")];

        let lines = build_report_lines(&plans, &inspections, &units);

        // FOUR rows, not three.
        assert_eq!(lines.len(), 4, "every required characteristic gets a row");
        let missing: Vec<&DraftLine> = lines
            .iter()
            .filter(|l| l.accountability == Accountability::NotMeasured)
            .collect();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].characteristic_name, "Hole X");
        assert_eq!(
            missing[0].actual_value, None,
            "a not-measured line must carry NO actual — a zero would be a fabricated measurement"
        );
        assert_eq!(missing[0].verdict, None);

        let counts = summarise(&lines);
        assert_eq!(counts.required, 4);
        assert_eq!(counts.measured, 3);
        assert_eq!(counts.passed, 3);
        assert_eq!(counts.failed, 0);
        assert_eq!(counts.unaccounted, 1);

        let disposition = compute_disposition(counts, false);
        assert_eq!(
            disposition,
            Disposition::Incomplete,
            "one unaccounted required characteristic makes the whole report incomplete"
        );
        assert!(
            !disposition.permits_shipment(),
            "an incomplete report MUST refuse the shipment"
        );
    }

    /// Full accountability ⇒ accept, and the shipment is permitted. The
    /// positive control for the test above: without this, a mutation that
    /// made EVERYTHING incomplete would still pass the negative test.
    #[test]
    fn full_accountability_accepts_and_permits_shipment() {
        let plans = vec![
            plan("p1", "Bore D", Some("1"), Some(true)),
            plan("p2", "Face Z", Some("2"), Some(true)),
        ];
        let inspections = vec![
            measurement(
                "q1",
                "p1",
                Some("uid1"),
                25.0,
                Verdict::Pass,
                "2026-08-02T10:00:00Z",
            ),
            measurement(
                "q2",
                "p2",
                Some("uid1"),
                25.0,
                Verdict::Pass,
                "2026-08-02T10:01:00Z",
            ),
        ];
        let lines = build_report_lines(&plans, &inspections, &[unit("SN-001", "uid1")]);
        let counts = summarise(&lines);
        assert_eq!(counts.unaccounted, 0);
        assert_eq!(counts.measured, 2);
        assert_eq!(counts.passed, 2);
        let d = compute_disposition(counts, false);
        assert_eq!(d, Disposition::Accept);
        assert!(d.permits_shipment());
    }

    /// Accountability is PER UNIT: measuring every characteristic on serial
    /// 1 does not account for serial 2.
    #[test]
    fn accountability_is_per_serialised_unit() {
        let plans = vec![plan("p1", "Bore D", Some("1"), Some(true))];
        let inspections = vec![measurement(
            "q1",
            "p1",
            Some("uid1"),
            25.0,
            Verdict::Pass,
            "2026-08-02T10:00:00Z",
        )];
        let units = vec![unit("SN-001", "uid1"), unit("SN-002", "uid2")];

        let lines = build_report_lines(&plans, &inspections, &units);
        assert_eq!(lines.len(), 2, "one row per characteristic per unit");
        let counts = summarise(&lines);
        assert_eq!(counts.required, 2);
        assert_eq!(counts.measured, 1);
        assert_eq!(counts.unaccounted, 1, "SN-002 is unmeasured");
        assert_eq!(compute_disposition(counts, false), Disposition::Incomplete);
    }

    /// A failing measurement rejects — and reject also refuses a shipment.
    #[test]
    fn a_failed_characteristic_rejects() {
        let plans = vec![plan("p1", "Bore D", Some("1"), Some(true))];
        let inspections = vec![measurement(
            "q1",
            "p1",
            Some("uid1"),
            25.9,
            Verdict::Major,
            "2026-08-02T10:00:00Z",
        )];
        let lines = build_report_lines(&plans, &inspections, &[unit("SN-001", "uid1")]);
        let counts = summarise(&lines);
        assert_eq!(counts.failed, 1);
        let d = compute_disposition(counts, false);
        assert_eq!(d, Disposition::Reject);
        assert!(!d.permits_shipment());
    }

    /// **Calibration-stale is neither a pass nor a fail** and forces
    /// `incomplete` (ISO 9001 §7.1.5.2 — a probe that may be lying is not
    /// evidence of conformity). ADR-0199 §AC9.
    #[test]
    fn calibration_stale_is_not_a_pass_and_forces_incomplete() {
        let plans = vec![plan("p1", "Bore D", Some("1"), Some(true))];
        let inspections = vec![measurement(
            "q1",
            "p1",
            Some("uid1"),
            25.0,
            Verdict::CalibrationStale,
            "2026-08-02T10:00:00Z",
        )];
        let lines = build_report_lines(&plans, &inspections, &[unit("SN-001", "uid1")]);
        let counts = summarise(&lines);
        assert_eq!(counts.measured, 1, "the row IS recorded");
        assert_eq!(counts.passed, 0, "but it is not a pass");
        assert_eq!(counts.failed, 0, "and it is not a fail");
        assert_eq!(counts.calibration_stale, 1);
        assert_eq!(counts.unaccounted, 0, "it is not 'unmeasured' either");
        let d = compute_disposition(counts, false);
        assert_eq!(d, Disposition::Incomplete);
        assert!(!d.permits_shipment());
    }

    /// An OPTIONAL characteristic (`is_required = Some(false)`) still gets
    /// a row, but does not count toward accountability and cannot make the
    /// report incomplete.
    #[test]
    fn an_optional_characteristic_prints_but_does_not_block() {
        let plans = vec![
            plan("p1", "Bore D", Some("1"), Some(true)),
            plan("p2", "Cosmetic", Some("2"), Some(false)),
        ];
        let inspections = vec![measurement(
            "q1",
            "p1",
            Some("uid1"),
            25.0,
            Verdict::Pass,
            "2026-08-02T10:00:00Z",
        )];
        let lines = build_report_lines(&plans, &inspections, &[unit("SN-001", "uid1")]);
        assert_eq!(lines.len(), 2, "the optional characteristic still prints");
        let counts = summarise(&lines);
        assert_eq!(counts.required, 1);
        assert_eq!(counts.unaccounted, 0);
        assert_eq!(compute_disposition(counts, false), Disposition::Accept);
    }

    /// A pre-ADR-0199 plan (`is_required = None`) counts as REQUIRED. The
    /// conservative reading: reading NULL as "optional" would silently drop
    /// every legacy characteristic out of the accountability count.
    #[test]
    fn a_legacy_plan_with_null_is_required_counts_as_required() {
        let plans = vec![plan("p1", "Legacy", None, None)];
        let lines = build_report_lines(&plans, &[], &[unit("SN-001", "uid1")]);
        let counts = summarise(&lines);
        assert_eq!(counts.required, 1);
        assert_eq!(counts.unaccounted, 1);
        assert_eq!(compute_disposition(counts, false), Disposition::Incomplete);
    }

    /// A WO with no marked units still produces a full accountability
    /// report at lot level. A zero-row report would be vacuously
    /// "complete" — the exact hole this module exists to close.
    #[test]
    fn a_wo_with_no_marked_units_still_accounts_for_every_characteristic() {
        let plans = vec![
            plan("p1", "Bore D", Some("1"), Some(true)),
            plan("p2", "Face Z", Some("2"), Some(true)),
        ];
        let lines = build_report_lines(&plans, &[], &[]);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.part_serial.is_none()));
        let counts = summarise(&lines);
        assert_eq!(counts.required, 2);
        assert_eq!(counts.unaccounted, 2);
        assert_eq!(compute_disposition(counts, false), Disposition::Incomplete);
    }

    /// A lot-level (material/process) characteristic renders ONCE, not
    /// once per serial — ADR-0199 §Open Q10, default accepted.
    #[test]
    fn a_lot_level_characteristic_renders_once_for_the_whole_shipment() {
        let plans = vec![
            plan("p1", "Bore D", Some("1"), Some(true)),
            lot_plan("p9", "Material grade"),
        ];
        let inspections = vec![
            measurement(
                "q1",
                "p1",
                Some("uid1"),
                25.0,
                Verdict::Pass,
                "2026-08-02T10:00:00Z",
            ),
            measurement(
                "q2",
                "p1",
                Some("uid2"),
                25.0,
                Verdict::Pass,
                "2026-08-02T10:01:00Z",
            ),
            // The lot measurement carries NO part uid.
            measurement("q9", "p9", None, 1.0, Verdict::Pass, "2026-08-02T10:02:00Z"),
        ];
        let units = vec![unit("SN-001", "uid1"), unit("SN-002", "uid2")];
        let lines = build_report_lines(&plans, &inspections, &units);

        // 2 units × 1 dimensional + 1 lot-level = 3 rows, not 4.
        assert_eq!(lines.len(), 3);
        let lot_lines: Vec<&DraftLine> = lines.iter().filter(|l| l.part_serial.is_none()).collect();
        assert_eq!(lot_lines.len(), 1);
        assert_eq!(
            lot_lines[0].characteristic_type,
            CharacteristicType::Material
        );
        assert_eq!(lot_lines[0].accountability, Accountability::Measured);
        assert_eq!(
            compute_disposition(summarise(&lines), false),
            Disposition::Accept
        );
    }

    /// A measurement attributed to a DIFFERENT unit does not account for
    /// this one. Without this, one measured part would silently release a
    /// whole batch.
    #[test]
    fn a_measurement_on_another_unit_does_not_account_for_this_one() {
        let plans = vec![plan("p1", "Bore D", Some("1"), Some(true))];
        let inspections = vec![measurement(
            "q1",
            "p1",
            Some("uid_OTHER"),
            25.0,
            Verdict::Pass,
            "2026-08-02T10:00:00Z",
        )];
        let lines = build_report_lines(&plans, &inspections, &[unit("SN-001", "uid1")]);
        assert_eq!(lines[0].accountability, Accountability::NotMeasured);
        assert_eq!(summarise(&lines).unaccounted, 1);
    }

    /// The LATEST measurement wins — the rework path (measure → fail →
    /// rework → re-measure → pass) must report the accepted value.
    #[test]
    fn the_latest_measurement_represents_the_characteristic() {
        let plans = vec![plan("p1", "Bore D", Some("1"), Some(true))];
        let inspections = vec![
            measurement(
                "q1",
                "p1",
                Some("uid1"),
                25.9,
                Verdict::Major,
                "2026-08-02T10:00:00Z",
            ),
            measurement(
                "q2",
                "p1",
                Some("uid1"),
                25.01,
                Verdict::Pass,
                "2026-08-02T14:00:00Z",
            ),
        ];
        let lines = build_report_lines(&plans, &inspections, &[unit("SN-001", "uid1")]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].verdict, Some(Verdict::Pass));
        assert_eq!(lines[0].qci_id.as_deref(), Some("q2"));
        assert_eq!(
            compute_disposition(summarise(&lines), false),
            Disposition::Accept
        );
    }

    /// An open NCR against a reported part downgrades a clean report to
    /// `accept_with_ncr` — which still ships (the ADR-0090 open-NCR gate
    /// owns that refusal, not this one).
    #[test]
    fn an_open_ncr_downgrades_accept_but_still_permits_shipment() {
        let plans = vec![plan("p1", "Bore D", Some("1"), Some(true))];
        let inspections = vec![measurement(
            "q1",
            "p1",
            Some("uid1"),
            25.0,
            Verdict::Pass,
            "2026-08-02T10:00:00Z",
        )];
        let lines = build_report_lines(&plans, &inspections, &[unit("SN-001", "uid1")]);
        let d = compute_disposition(summarise(&lines), true);
        assert_eq!(d, Disposition::AcceptWithNcr);
        assert!(d.permits_shipment());
    }

    /// Reject outranks incomplete when both apply (ADR-0199 §D4's stated
    /// ordering). Both refuse the shipment, so the ordering is about which
    /// problem the operator is told about first.
    #[test]
    fn reject_outranks_incomplete() {
        let counts = AccountabilityCounts {
            required: 3,
            measured: 2,
            passed: 1,
            failed: 1,
            unaccounted: 1,
            calibration_stale: 0,
        };
        assert_eq!(compute_disposition(counts, false), Disposition::Reject);
    }

    /// A tampered row — "measured" with no verdict — must NOT read as an
    /// accept. It counts as neither pass nor fail, and the
    /// `measured != passed` guard catches it.
    #[test]
    fn a_measured_line_with_no_verdict_cannot_produce_an_accept() {
        let counts = AccountabilityCounts {
            required: 1,
            measured: 1,
            passed: 0,
            failed: 0,
            unaccounted: 0,
            calibration_stale: 0,
        };
        assert_eq!(compute_disposition(counts, false), Disposition::Incomplete);
    }

    /// Line ORDER is deterministic and balloon-aware ("2" before "10").
    /// Determinism is load-bearing: ADR-0199 §D7 pins the SHA-256 of the
    /// rendered bytes, and an unstable order breaks the pin on the first
    /// re-render.
    #[test]
    fn line_order_is_deterministic_and_balloon_numeric() {
        let plans = vec![
            plan("pC", "Third", Some("10"), Some(true)),
            plan("pA", "First", Some("2"), Some(true)),
            plan("pB", "Second", Some("7.2"), Some(true)),
            plan("pD", "Unballooned", None, Some(true)),
        ];
        let names: Vec<&str> = build_report_lines(&plans, &[], &[])
            .iter()
            .map(|l| l.characteristic_name.as_str())
            .map(|s| Box::leak(s.to_string().into_boxed_str()) as &str)
            .collect();
        assert_eq!(names, vec!["First", "Second", "Third", "Unballooned"]);

        // And the order does not depend on the input order.
        let mut shuffled = plans.clone();
        shuffled.reverse();
        let names2: Vec<String> = build_report_lines(&shuffled, &[], &[])
            .iter()
            .map(|l| l.characteristic_name.clone())
            .collect();
        assert_eq!(names2, vec!["First", "Second", "Third", "Unballooned"]);
    }

    /// The serial range is a human-readable snapshot, sorted, with a count.
    #[test]
    fn serial_range_summarises_the_units() {
        assert_eq!(serial_range_of(&[]), None);
        assert_eq!(
            serial_range_of(&[unit("SN-007", "u1")]),
            Some("SN-007".to_string())
        );
        assert_eq!(
            serial_range_of(&[
                unit("SN-003", "u3"),
                unit("SN-001", "u1"),
                unit("SN-002", "u2"),
            ]),
            Some("SN-001 … SN-003 (3 units)".to_string())
        );
    }
}
