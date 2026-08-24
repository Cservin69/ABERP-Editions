//! ADR-0199 — QC inspection report / Certificate of Conformance
//! orchestration.
//!
//! The glue between three things that cannot see each other:
//!
//! - `aberp_qa::qc::reports` — the frozen record + the pure accountability
//!   arithmetic. Knows nothing about partners, NCRs, part marks or PDFs.
//! - `aberp_qc_pdf` — the pure renderer. Knows nothing about a database.
//! - `aberp_dispatch` — the shipment. Must not gain a quality dependency,
//!   so it takes an injected [`QcShipmentDocumentBinder`].
//!
//! This module owns the three things that need all of them at once:
//!
//! 1. **Traceability resolution** — reading `wo_part_marks`, the drawing
//!    refs, the WO and the open NCRs, ONCE, at draft time, and handing
//!    the resolved values to `freeze_report` to be snapshotted.
//! 2. **Issuance** — rendering the bytes, hashing them, and pinning the
//!    hash into the chain. The bytes are never stored (ADR-0199 §D7).
//! 3. **Binding** — the [`aberp_dispatch::ShipmentDocumentBinder`] impl
//!    that runs inside `mark_shipped`'s transaction.
//!
//! ## Edition scope
//!
//! Every entry point here is Defense-only and says so by calling
//! [`crate::build_profile::assert_qc_reporting_allowed`] first — the
//! runtime backstop behind the compile-time
//! `qc_reporting_allowed()` binding (ADR-0199 §D9). The binder is the one
//! exception and it is deliberate: see [`QcShipmentDocumentBinder`].

use anyhow::{anyhow, Context, Result};
use duckdb::{Connection, Transaction};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use aberp_audit_ledger::{Actor, BinaryHash, LedgerMeta, TenantId};
use aberp_db::HandleArc;
use aberp_inventory::ActorKind;
use aberp_qa::{
    QcReport, QcReportKind, QcReportLine, QcReportTemplate, QcWriteContext, ReportTraceability,
    ReportUnit,
};

/// Errors the QC-report routes surface.
#[derive(Debug, thiserror::Error)]
pub enum QcReportError {
    /// The work order, partner or report does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Operator input failed an in-code invariant.
    #[error("{0}")]
    Validation(String),
    /// This build is not allowed to produce QC reports (ADR-0199 §D9).
    #[error("{0}")]
    NotPermitted(String),
    /// The report exists but is no longer CURRENT — voided, or superseded
    /// by a later report. There is no valid document to hand out.
    /// Surfaced as 409, never as bytes.
    #[error("{0}")]
    NotCurrent(String),
    /// Everything else — surfaced as 500.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<aberp_qa::QcError> for QcReportError {
    fn from(e: aberp_qa::QcError) -> Self {
        match e {
            aberp_qa::QcError::NotFound => QcReportError::NotFound("report or plan".into()),
            aberp_qa::QcError::Validation(m) => QcReportError::Validation(m),
            aberp_qa::QcError::UnitsMismatch { .. } => QcReportError::Validation(e.to_string()),
            aberp_qa::QcError::Storage(err) => QcReportError::Other(err),
        }
    }
}

/// SHA-256 of the rendered bytes, lowercase hex. The ONE place the hash
/// is computed, so the value pinned at issuance and the value recomputed
/// at every later render can never be produced by two different recipes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// **The canonical byte-form of an issued report.**
///
/// ADR-0199 §D7 pins the SHA-256 of the rendered bytes and stores no bytes,
/// so issuance and every later re-render MUST hash the same shape. Four
/// fields on `qc_reports` can change AFTER issuance, and each one would
/// silently break that pin if it reached the renderer:
///
/// | Field | Changes when |
/// |---|---|
/// | `rendered_sha256` | at issuance — a document cannot contain its own hash |
/// | `dsp_id` | when `mark_shipped` binds the report to a dispatch |
/// | `state` | on supersede / void |
/// | `superseded_by_qcr_id` | on supersede |
///
/// Two of the four are now ALSO absent from the rendered page — the
/// renderer prints neither `state` nor `rendered_sha256` (see
/// `aberp_qc_pdf::push_header` / `push_footer`), because normalising a
/// mutable field is only half a fix: forcing `state` to `Issued` is what
/// let a VOIDED report re-render with a confident ISSUED header. This
/// function stays as the defence in depth that keeps the hash stable if a
/// future edit reintroduces either field to the layout.
///
/// `dsp_id` is the one that actually bit: a report must be ISSUED before the
/// gate lets the shipment proceed, so the dispatch id is assigned strictly
/// after the hash is taken. Rendering it would make **every correctly
/// shipped report** report itself as tampered on the next download.
///
/// So the canonical form is the report AS ISSUED, and this is the ONE place
/// that defines it — both [`issue_report`] and [`render_report`] go through
/// it, which is what makes them provably agree.
///
/// The consequence is deliberate: the printed document does not name the
/// dispatch it rode on. That linkage is not lost — it is a chain event
/// (`qcr.report_attached_to_shipment`), it is on `GET /api/qc-reports/:id`,
/// and `GET /api/dispatches/:id/qc-reports` reads it from the other side.
/// Putting a mutable field inside a hashed document is the thing that
/// cannot work.
fn canonical_for_render(report: &QcReport) -> QcReport {
    let mut c = report.clone();
    c.rendered_sha256 = None;
    c.dsp_id = None;
    c.state = aberp_qa::QcReportState::Issued;
    c.superseded_by_qcr_id = None;
    c
}

/// **The ONE way a report becomes bytes.**
///
/// Issuance and every later re-render call this and nothing else, so the
/// two cannot drift: the canonicalisation, the chain reference and the
/// renderer call all live in a single place. Both used to build
/// `QcReportInputs` separately, and both hard-coded `chain_reference: ""`
/// — so every Certificate of Conformance printed `Audit chain ref: —`
/// directly under a fixed statement promising "the tamper-evident audit
/// chain entry cited below".
fn render_canonical(report: &QcReport, lines: &[QcReportLine]) -> Result<Vec<u8>, QcReportError> {
    let canonical = canonical_for_render(report);
    // A DRAFT has no `qcr.report_issued` entry yet, so it cites nothing
    // rather than citing an entry that does not exist. Issuance renders
    // against a copy whose `issued_at_utc` is already set, so the bytes
    // hashed at issuance carry the same reference a re-render produces.
    let chain_reference = if canonical.issued_at_utc.is_some() {
        aberp_qa::issuance_chain_ref(&canonical.qcr_id)
    } else {
        String::new()
    };
    aberp_qc_pdf::render(&aberp_qc_pdf::QcReportInputs {
        report: &canonical,
        lines,
        chain_reference: &chain_reference,
    })
    .map_err(|e| QcReportError::Other(anyhow!("render QC report: {e}")))
}

/// What the operator asked for when drafting a report.
#[derive(Debug, Clone)]
pub struct DraftReportRequest {
    pub wo_id: String,
    pub report_kind: QcReportKind,
    /// Operator override. `None` ⇒ resolve from the customer
    /// (ADR-0199 §D2).
    pub template: Option<QcReportTemplate>,
    pub notes: Option<String>,
}

/// A report plus its frozen lines, as returned to the route layer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReportWithLines {
    pub report: QcReport,
    pub lines: Vec<QcReportLine>,
}

fn write_ctx<'a>(
    tenant: &'a TenantId,
    ledger_meta: &'a LedgerMeta,
    operator: &str,
    session_id: &str,
) -> QcWriteContext<'a> {
    QcWriteContext {
        tenant: tenant.as_str(),
        actor: ActorKind::SpaOperator {
            operator_login: operator.to_string(),
        },
        ledger_meta,
        ledger_actor: Actor::from_local_cli(session_id.to_string(), operator),
    }
}

/// Resolve every traceability value the report snapshots, reading each
/// source exactly once (ADR-0199 §D3(c)).
///
/// Returns `(traceability, units, product_id, partner_id)`. Nothing here
/// invents a value: an absent drawing ref, heat lot or quote id stays
/// `None` and prints as a blank on the document, because a fabricated
/// drawing revision on a compliance record is worse than a visible gap.
fn resolve_context(
    conn: &Connection,
    tenant: &str,
    wo_id: &str,
    dsp_id: Option<&str>,
    notes: Option<String>,
) -> Result<(ReportTraceability, Vec<ReportUnit>, String, String), QcReportError> {
    let wo = aberp_work_orders::read_work_order(conn, tenant, wo_id)
        .map_err(|e| QcReportError::Other(anyhow!("read work order {wo_id}: {e}")))?
        .ok_or_else(|| QcReportError::NotFound(format!("work order {wo_id}")))?;

    let marks = crate::part_marking::list_part_marks(conn, tenant, wo_id)
        .context("list part marks for QC report")?;
    let units: Vec<ReportUnit> = marks
        .iter()
        .map(|m| ReportUnit {
            part_serial: m.serial_number.clone(),
            part_uid: m.part_uid.clone(),
        })
        .collect();

    // The heat/lot is snapshotted from the MARKS, not re-derived: the mark
    // recorded the lot at marking time, which is the fact the parts in the
    // box actually carry. Distinct lots across the units are reported as
    // such rather than collapsed to the first one — a silently-dropped
    // second heat number would misstate the traceability.
    let mut lots: Vec<String> = marks
        .iter()
        .filter_map(|m| m.heat_lot_reference.clone())
        .collect();
    lots.sort();
    lots.dedup();
    let heat_lot_reference = match lots.len() {
        0 => None,
        1 => Some(lots.remove(0)),
        _ => Some(lots.join(", ")),
    };

    let drawing = aberp_qa::current_drawing_ref(conn, tenant, &wo.product_id)?;

    // The partner comes from the dispatch when one exists, because the
    // report belongs to the delivery. Without a dispatch there is no
    // partner on the WO to fall back to, so the caller must supply one.
    let partner_id = match dsp_id {
        Some(d) => aberp_dispatch::get_dispatch(conn, tenant, d)
            .map_err(|e| QcReportError::Other(anyhow!("read dispatch {d}: {e}")))?
            .map(|dsp| dsp.partner_id),
        None => None,
    };
    let partner_id = match partner_id {
        Some(p) => p,
        None => dispatch_partner_for_wo(conn, tenant, wo_id)?.ok_or_else(|| {
            QcReportError::Validation(format!(
                "work order {wo_id} has no dispatch yet — a QC report is issued against a \
                 delivery, so create the dispatch first"
            ))
        })?,
    };

    let trace = ReportTraceability {
        source_quote_id: wo.source_quote_id.clone(),
        drawing_number: drawing.as_ref().map(|d| d.drawing_number.clone()),
        drawing_rev: drawing.as_ref().map(|d| d.drawing_rev.clone()),
        heat_lot_reference,
        // ABERP has no mill-cert record wired to a WO yet (the
        // `MaterialTraceabilitySeed.mill_cert_id` lives on the compliance
        // seed, not on the work order). Left None and printed blank rather
        // than guessed; closing it is master-data work, not report work.
        mill_cert_id: None,
        // Likewise: `machine_id` / `program_id` arrive on a probe event.
        // Manual entry (the Phase-1 path) carries neither, so both stay
        // None until the Phase-2 probe feed populates them.
        machine_id: None,
        program_id: None,
        notes,
    };
    Ok((trace, units, wo.product_id, partner_id))
}

/// The partner of the WO's dispatch, if it has one.
fn dispatch_partner_for_wo(
    conn: &Connection,
    tenant: &str,
    wo_id: &str,
) -> Result<Option<String>, QcReportError> {
    let mut stmt = conn
        .prepare(
            "SELECT partner_id FROM dispatches
             WHERE tenant_id = ? AND wo_id = ? ORDER BY created_at DESC LIMIT 1;",
        )
        .map_err(|e| QcReportError::Other(anyhow!("prepare dispatch partner lookup: {e}")))?;
    let mut rows = stmt
        .query_map(duckdb::params![tenant, wo_id], |r| r.get::<_, String>(0))
        .map_err(|e| QcReportError::Other(anyhow!("query dispatch partner: {e}")))?;
    match rows.next() {
        Some(r) => Ok(Some(r.map_err(|e| {
            QcReportError::Other(anyhow!("read dispatch partner: {e}"))
        })?)),
        None => Ok(None),
    }
}

/// Whether any Open/Contained NCR references one of the report's parts.
/// Drives the `accept_with_ncr` disposition arm (ADR-0199 §D4).
fn open_ncr_against(conn: &Connection, tenant: &str, units: &[ReportUnit]) -> Result<bool> {
    if units.is_empty() {
        return Ok(false);
    }
    let uids: Vec<String> = units.iter().map(|u| u.part_uid.clone()).collect();
    let ncrs = crate::quality::list_ncrs(conn, tenant, &crate::quality::NcrFilter::default())?;
    Ok(!crate::quality::open_ncr_ids_blocking_part_uids(&ncrs, &uids).is_empty())
}

/// Draft a report: resolve traceability, compute accountability, freeze
/// the lines, write the `drafted` row + one `qcr.report_drafted`.
#[allow(clippy::too_many_arguments)]
pub fn draft_report(
    db: &HandleArc,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    now: OffsetDateTime,
    req: DraftReportRequest,
) -> Result<ReportWithLines, QcReportError> {
    crate::build_profile::assert_qc_reporting_allowed("draft a QC report")
        .map_err(|e| QcReportError::NotPermitted(e.to_string()))?;

    let mut guard = db
        .write()
        .map_err(|e| QcReportError::Other(anyhow!("shared writer for QC report draft: {e}")))?;
    aberp_audit_ledger::ensure_schema(&guard)
        .map_err(|e| QcReportError::Other(anyhow!("ensure audit schema: {e}")))?;
    aberp_qa::ensure_schema(&guard)
        .map_err(|e| QcReportError::Other(anyhow!("ensure qa/qc schema: {e}")))?;
    aberp_dispatch::ensure_schema(&guard)
        .map_err(|e| QcReportError::Other(anyhow!("ensure dispatch schema: {e}")))?;
    crate::partners::ensure_schema(&guard)
        .map_err(|e| QcReportError::Other(anyhow!("ensure partners schema: {e}")))?;

    let (trace, units, product_id, partner_id) =
        resolve_context(&guard, tenant.as_str(), &req.wo_id, None, req.notes.clone())?;

    let template = match req.template {
        Some(t) => t,
        None => crate::partners::resolve_qc_report_template(&guard, tenant.as_str(), &partner_id)
            .map_err(QcReportError::Other)?,
    };

    // Only ENABLED, non-archived plans are in scope. An archived
    // characteristic is one the shop deliberately stopped inspecting, and
    // counting it as unaccounted-for would make every report permanently
    // incomplete after the first plan retirement.
    let plans: Vec<aberp_qa::InspectionPlan> =
        aberp_qa::list_inspection_plans(&guard, tenant.as_str(), Some(&product_id), false)?
            .into_iter()
            .filter(|p| p.enabled)
            .collect();
    let inspections = aberp_qa::list_inspections_for_wo(&guard, tenant.as_str(), &req.wo_id)?;
    let open_ncr =
        open_ncr_against(&guard, tenant.as_str(), &units).map_err(QcReportError::Other)?;
    // The ONLY read of `partners` on the whole report path. Resolved here,
    // at freeze, and snapshotted onto the header — see
    // `aberp_qa::QcReport::customer_name`.
    let customer = customer_info(&guard, tenant.as_str(), &partner_id)?;

    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash);
    let session_id = ulid::Ulid::new().to_string();
    let ctx = write_ctx(&tenant, &ledger_meta, operator, &session_id);

    let tx = guard
        .transaction()
        .map_err(|e| QcReportError::Other(anyhow!("begin QC report draft tx: {e}")))?;
    let (report, lines) = aberp_qa::freeze_report(
        &tx,
        &ctx,
        aberp_qa::FreezeReportInputs {
            report_kind: req.report_kind,
            template,
            wo_id: &req.wo_id,
            product_id: &product_id,
            partner_id: &partner_id,
            plans: &plans,
            inspections: &inspections,
            units: &units,
            open_ncr_against_reported_part: open_ncr,
            traceability: trace,
            customer: customer.into_snapshot(),
            created_by: operator,
        },
        now,
    )?;
    tx.commit()
        .map_err(|e| QcReportError::Other(anyhow!("commit QC report draft tx: {e}")))?;
    Ok(ReportWithLines { report, lines })
}

/// Issue a drafted report: render once, hash the bytes, pin the hash.
///
/// The bytes are DISCARDED after hashing — that is the whole point of
/// ADR-0199 §D7. Anyone who wants the document re-renders it from the
/// frozen rows, and the chain entry proves the bytes they get are the
/// bytes that were issued.
pub fn issue_report(
    db: &HandleArc,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    now: OffsetDateTime,
    qcr_id: &str,
) -> Result<ReportWithLines, QcReportError> {
    crate::build_profile::assert_qc_reporting_allowed("issue a QC report")
        .map_err(|e| QcReportError::NotPermitted(e.to_string()))?;

    let mut guard = db
        .write()
        .map_err(|e| QcReportError::Other(anyhow!("shared writer for QC report issue: {e}")))?;
    aberp_audit_ledger::ensure_schema(&guard)
        .map_err(|e| QcReportError::Other(anyhow!("ensure audit schema: {e}")))?;

    let report = aberp_qa::get_report(&guard, tenant.as_str(), qcr_id)?
        .ok_or_else(|| QcReportError::NotFound(format!("QC report {qcr_id}")))?;
    let lines = aberp_qa::list_report_lines(&guard, tenant.as_str(), qcr_id)?;

    // Render against a copy whose issuance stamps are already set, so the
    // bytes hashed here are the bytes a later re-render reproduces. The
    // footer prints `issued_by` / `issued_at_utc`; hashing a version that
    // still said "not issued" would guarantee the pin never matched.
    // Render the report AS IT WILL BE STORED once issued, then canonicalise
    // — so the bytes hashed here are exactly the bytes a re-render
    // reproduces. `issued_by` is trimmed to match what `issue_report`
    // persists (it trims); an untrimmed operator login here would put a
    // different string on the page than the one the row carries.
    let mut to_render = report.clone();
    to_render.issued_by = Some(operator.trim().to_string());
    to_render.issued_at_utc = Some(
        now.format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| QcReportError::Other(anyhow!("format issue stamp: {e}")))?,
    );
    to_render.renderer_version = Some(aberp_qc_pdf::QC_PDF_RENDERER_VERSION.to_string());
    let bytes = render_canonical(&to_render, &lines)?;
    let sha = sha256_hex(&bytes);

    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash);
    let session_id = ulid::Ulid::new().to_string();
    let ctx = write_ctx(&tenant, &ledger_meta, operator, &session_id);
    let tx = guard
        .transaction()
        .map_err(|e| QcReportError::Other(anyhow!("begin QC report issue tx: {e}")))?;
    let issued = aberp_qa::issue_report(
        &tx,
        &ctx,
        qcr_id,
        &sha,
        aberp_qc_pdf::QC_PDF_RENDERER_VERSION,
        operator,
        now,
    )?;
    tx.commit()
        .map_err(|e| QcReportError::Other(anyhow!("commit QC report issue tx: {e}")))?;
    Ok(ReportWithLines {
        report: issued,
        lines,
    })
}

/// Customer identity for the PDF, owned here because `aberp-qc-pdf` has
/// no database and `aberp-qa` has no partners table.
#[derive(Debug, Clone, Default)]
pub struct CustomerBlock {
    pub name: String,
    pub address_line: String,
    pub purchase_order: String,
}

impl CustomerBlock {
    /// Into the shape `freeze_report` snapshots onto the header. A blank
    /// field becomes `None` rather than `Some("")` so the two spellings of
    /// "absent" cannot produce two different documents.
    fn into_snapshot(self) -> aberp_qa::ReportCustomer {
        fn some_if_set(v: String) -> Option<String> {
            let t = v.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        aberp_qa::ReportCustomer {
            name: some_if_set(self.name),
            address_line: some_if_set(self.address_line),
            purchase_order: some_if_set(self.purchase_order),
        }
    }
}

fn customer_info(
    conn: &Connection,
    tenant: &str,
    partner_id: &str,
) -> Result<CustomerBlock, QcReportError> {
    let Some(p) =
        crate::partners::get_partner(conn, tenant, partner_id).map_err(QcReportError::Other)?
    else {
        // An unknown partner degrades to the id itself rather than a blank
        // or a guess — the same posture `resolve_export_recipient` takes.
        return Ok(CustomerBlock {
            name: partner_id.to_string(),
            ..Default::default()
        });
    };
    let address_line = [
        p.address_postal_code.as_deref(),
        p.address_city.as_deref(),
        p.address_street.as_deref(),
        p.address_country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(", ");
    Ok(CustomerBlock {
        name: if p.legal_name.trim().is_empty() {
            p.display_name
        } else {
            p.legal_name
        },
        address_line,
        // ABERP carries no customer-PO field on the partner or the WO; the
        // AS9102 Form 1 field 12 prints blank rather than a guess.
        purchase_order: String::new(),
    })
}

/// Re-render an existing report from its frozen rows. **Read-only** — the
/// caller decides whether to append a `qcr.report_rendered` entry.
///
/// Refuses a report that is no longer current (`Voided` / `Superseded`)
/// with [`QcReportError::NotCurrent`] — see the body for why.
///
/// Returns the bytes and the SHA-256, plus whether the SHA matched the one
/// pinned at issuance — `None` when the report is still a draft and has no
/// pin. `Some(false)` is the tamper signal: the frozen rows no longer
/// produce the bytes the chain says were issued.
pub fn render_report(
    conn: &Connection,
    tenant: &str,
    qcr_id: &str,
) -> Result<(QcReport, Vec<u8>, String, Option<bool>), QcReportError> {
    crate::build_profile::assert_qc_reporting_allowed("render a QC report")
        .map_err(|e| QcReportError::NotPermitted(e.to_string()))?;
    let report = aberp_qa::get_report(conn, tenant, qcr_id)?
        .ok_or_else(|| QcReportError::NotFound(format!("QC report {qcr_id}")))?;
    // **A report that is no longer CURRENT is not rendered at all.**
    //
    // The rendered page cannot carry `state` (it is post-issuance-mutable,
    // and printing it breaks the §D7 pin), so a `Voided` or `Superseded`
    // report would come out looking exactly like a valid issued one — a
    // document that no longer stands, still reading as evidence of
    // conformity. Refusing is the honest answer: the report, its reason and
    // its superseding id are all still on `GET /api/qc-reports/:id`, and the
    // transition is a chain entry.
    //
    // **Round 3 — `Superseded` refuses on the same footing as `Voided`.**
    // The two arrive through the SAME route (`void_report` picks the state
    // from whether a `superseded_by_qcr_id` was supplied) and mean the same
    // thing to a reader: this is not the document that stands. Superseded is
    // if anything the sharper hazard, because the report it replaces is
    // typically the FLATTERING one — the early `accept` that a later
    // `reject` corrected. That is the very pair the round-2 gate fix exists
    // to stop shipping on, and serving its clean, unmarked PDF hands an
    // auditor exactly the document the gate refused to ship on. Whether
    // "it was what was issued" makes an unmarked re-render defensible is not
    // a question worth resolving in the direction that can mislead.
    //
    // Handing an auditor a VOID/SUPERSEDED-stamped copy would be strictly
    // better and is flagged in the ADR as Phase 2 — it needs the stamp drawn
    // OUTSIDE the hashed byte-form, which is a renderer change, not a route
    // one.
    //
    // This refusal is a CURRENCY check, not a byte-form change: the §D7
    // invariant that a state transition perturbs no byte of the document is
    // unaffected and still pinned, on `render_canonical` itself — see
    // `render_canonical_is_byte_identical_across_a_supersede`.
    //
    // The `superseded_by_qcr_id.is_some()` arm is a belt on the braces: it
    // is written by `void_report` in the SAME statement that sets `state`,
    // so the two cannot disagree through the application. If a row ever does
    // disagree, the pointer to a replacement is the honest signal and the
    // stale `state` is the one to distrust.
    let not_current = match report.state {
        aberp_qa::QcReportState::Voided => Some("was voided"),
        aberp_qa::QcReportState::Superseded => Some("has been superseded"),
        _ if report.superseded_by_qcr_id.is_some() => Some("has been superseded"),
        _ => None,
    };
    if let Some(what) = not_current {
        return Err(QcReportError::NotCurrent(format!(
            "QC report {qcr_id} {what}{} — no document is issued for it",
            report
                .superseded_by_qcr_id
                .as_deref()
                .map(|s| format!(" by {s}"))
                .unwrap_or_default(),
        )));
    }
    let lines = aberp_qa::list_report_lines(conn, tenant, qcr_id)?;

    // Hash the SAME shape that was hashed at issuance — ONE helper does
    // both, so they cannot drift.
    let bytes = render_canonical(&report, &lines)?;
    let sha = sha256_hex(&bytes);
    // `None` for a DRAFT: it has no pinned hash, so there is nothing to
    // match. Reporting `false` there would flag every legitimate preview as
    // a divergence and teach an operator to ignore the signal.
    let matches = report
        .rendered_sha256
        .as_deref()
        .map(|pinned| pinned.eq_ignore_ascii_case(&sha));
    Ok((report, bytes, sha, matches))
}

// ── The shipment-document binder (ADR-0199 §D6) ────────────────────

/// The [`aberp_dispatch::ShipmentDocumentBinder`] implementation.
///
/// Runs INSIDE `mark_shipped`'s transaction, so the report binding and
/// the ship commit or roll back together.
///
/// **On a non-Defense build it binds nothing and succeeds.** That is not
/// a gap: on Portable no report can exist in the first place (the routes
/// are not mounted and `draft_report` refuses), so there is nothing to
/// bind, and *failing* here instead would refuse every Portable shipment
/// over a capability that build does not have. The refusal that matters —
/// "this Defense shipment has no complete report" — lives in the gate at
/// the route, before the transaction opens.
#[derive(Debug)]
pub struct QcShipmentDocumentBinder<'a> {
    pub ledger_meta: &'a LedgerMeta,
    pub ledger_actor: Actor,
    pub operator: String,
}

impl aberp_dispatch::ShipmentDocumentBinder for QcShipmentDocumentBinder<'_> {
    fn bind_qc_reports(
        &self,
        tx: &Transaction<'_>,
        tenant: &str,
        wo_id: &str,
        dsp_id: &str,
    ) -> Result<Vec<String>> {
        if !crate::build_profile::qc_reporting_allowed() {
            return Ok(Vec::new());
        }
        let ctx = QcWriteContext {
            tenant,
            actor: ActorKind::SpaOperator {
                operator_login: self.operator.clone(),
            },
            ledger_meta: self.ledger_meta,
            ledger_actor: self.ledger_actor.clone(),
        };
        aberp_qa::bind_reports_to_dispatch(tx, &ctx, wo_id, dsp_id)
            .map_err(|e| anyhow!("bind QC reports for {wo_id} → {dsp_id}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> QcReport {
        QcReport {
            qcr_id: "qcr_X".into(),
            report_number: "QCR-2026-0001".into(),
            report_kind: QcReportKind::DimensionalInspection,
            template: QcReportTemplate::AbenStandard,
            state: aberp_qa::QcReportState::Superseded,
            wo_id: "wo_1".into(),
            product_id: "prd_1".into(),
            dsp_id: Some("dsp_77".into()),
            partner_id: "ptr_1".into(),
            source_quote_id: None,
            drawing_number: Some("DWG-1".into()),
            drawing_rev: Some("C".into()),
            qty_reported: 2,
            serial_range: Some("SN-001 … SN-002 (2 units)".into()),
            heat_lot_reference: Some("HL-1".into()),
            mill_cert_id: None,
            machine_id: None,
            program_id: None,
            // Deliberately NOT the default of any field it touches: the
            // fixture has to be able to detect OVER-normalisation, and a
            // field that already holds the default value cannot.
            disposition: aberp_qa::Disposition::Incomplete,
            characteristics_required: 2,
            characteristics_measured: 1,
            characteristics_passed: 1,
            characteristics_failed: 0,
            characteristics_unaccounted: 1,
            rendered_sha256: Some("deadbeef".into()),
            renderer_version: Some("aberp-qc-pdf@0.0.0".into()),
            issued_at_utc: Some("2026-08-23T12:00:00Z".into()),
            issued_by: Some("ervin".into()),
            superseded_by_qcr_id: Some("qcr_Y".into()),
            created_at: "2026-08-23T11:00:00Z".into(),
            created_by: "ervin".into(),
            // Non-default for the same reason `disposition` is: `None` is
            // this field's default, so a fixture that left it `None` could
            // not detect `canonical_for_render` over-normalising it away.
            notes: Some("first article, re-run after rework".into()),
            customer_name: Some("Prime Aerospace Kft.".into()),
            customer_address_line: Some("1117 Budapest, Fő utca 1., HU".into()),
            customer_purchase_order: Some("PO-2026-889".into()),
        }
    }

    /// **Exactly the four post-issuance-mutable fields are normalised, and
    /// nothing else is.**
    ///
    /// Proven directly rather than only through a render, because the
    /// renderer does not currently READ `dsp_id` — so a render-level test
    /// cannot tell whether the normalisation is doing anything. It is the
    /// guard that keeps the pin correct if a future edit puts one of these
    /// fields back on the page (see the NOTE in `aberp-qc-pdf`), and a
    /// guard nobody tests is a guard that quietly stops working.
    #[test]
    fn canonical_for_render_normalises_only_the_post_issuance_fields() {
        let r = sample_report();
        let c = canonical_for_render(&r);

        // The four that change after the hash is taken.
        assert_eq!(
            c.rendered_sha256, None,
            "a document cannot contain its own hash"
        );
        assert_eq!(
            c.dsp_id, None,
            "dsp_id is assigned by mark_shipped, AFTER issuance"
        );
        assert_eq!(
            c.state,
            aberp_qa::QcReportState::Issued,
            "state moves on supersede/void"
        );
        assert_eq!(c.superseded_by_qcr_id, None);

        // Everything else is untouched — normalising more than necessary
        // would blank real evidence off the document.
        assert_eq!(c.qcr_id, r.qcr_id);
        assert_eq!(c.report_number, r.report_number);
        assert_eq!(c.drawing_number, r.drawing_number);
        assert_eq!(c.drawing_rev, r.drawing_rev);
        assert_eq!(c.serial_range, r.serial_range);
        assert_eq!(c.heat_lot_reference, r.heat_lot_reference);
        assert_eq!(c.qty_reported, r.qty_reported);
        assert_eq!(c.disposition, r.disposition);
        assert_eq!(c.characteristics_unaccounted, r.characteristics_unaccounted);
        assert_eq!(c.issued_by, r.issued_by);
        assert_eq!(c.issued_at_utc, r.issued_at_utc);
        assert_eq!(c.renderer_version, r.renderer_version);
        assert_eq!(c.notes, r.notes);
        assert_eq!(c.customer_name, r.customer_name);
        assert_eq!(c.customer_address_line, r.customer_address_line);
        assert_eq!(c.customer_purchase_order, r.customer_purchase_order);
    }

    /// One frozen line, enough to give the renderer a table to draw.
    fn sample_line() -> QcReportLine {
        QcReportLine {
            qcrl_id: "qcrl_1".into(),
            qcr_id: "qcr_X".into(),
            line_no: 1,
            part_serial: Some("SN-001".into()),
            part_uid: Some("uid_SN-001".into()),
            characteristic_number: Some("1".into()),
            characteristic_name: "Bore D".into(),
            characteristic_designator: None,
            characteristic_type: aberp_qa::CharacteristicType::Dimensional,
            inspection_method: Some(aberp_qa::InspectionMethod::OnMachineProbe),
            sheet_zone: Some("1/B4".into()),
            nominal_value: Some(25.0),
            upper_tol: Some(0.05),
            lower_tol: Some(-0.05),
            units: Some("mm".into()),
            actual_value: Some(25.012),
            deviation: Some(0.012),
            verdict: Some(aberp_qa::Verdict::Pass),
            accountability: aberp_qa::Accountability::Measured,
            qci_id: Some("qci_x".into()),
            measured_at_utc: Some("2026-08-23T10:00:00Z".into()),
            measured_by: Some("ervin".into()),
            probe_serial: Some("RMP600-007".into()),
            created_at: "2026-08-23T11:00:00Z".into(),
            required: true,
        }
    }

    /// **A supersede (or a void) perturbs no byte of the document** —
    /// ADR-0199 §D7's load-bearing invariant, pinned on the byte-form
    /// itself.
    ///
    /// This claim used to be asserted at the end of the integration test
    /// `an_issued_pdf_cites_its_chain_entry_and_is_not_stamped_draft`, by
    /// superseding a real report and re-rendering it through
    /// [`render_report`]. Round 3 made `render_report` REFUSE a non-current
    /// report — the right answer for a route that hands out documents, but
    /// it would have taken this invariant down with it. So the invariant
    /// moves here, to the layer it is actually about: `render_canonical`
    /// normalises `state` and `superseded_by_qcr_id` away, so the bytes (and
    /// therefore the pinned SHA) are identical across the transition, and a
    /// correctly issued report never reports itself as tampered.
    ///
    /// The two claims are independent and both matter: the CURRENCY check
    /// decides whether the bytes may be served, and the BYTE-FORM check
    /// decides whether the pin still verifies. Pinning them separately is
    /// what stops a future edit from "fixing" one by weakening the other.
    #[test]
    fn render_canonical_is_byte_identical_across_a_supersede() {
        let lines = vec![sample_line()];

        let mut issued = sample_report();
        issued.state = aberp_qa::QcReportState::Issued;
        issued.superseded_by_qcr_id = None;
        issued.dsp_id = None;
        let before = render_canonical(&issued, &lines).expect("render the issued report");

        // The two transitions that move `state` off `Issued`, plus the
        // `dsp_id` binding that `mark_shipped` performs after issuance.
        for (state, superseded_by) in [
            (aberp_qa::QcReportState::Superseded, Some("qcr_Y")),
            (aberp_qa::QcReportState::Voided, None),
        ] {
            let mut moved = issued.clone();
            moved.state = state;
            moved.superseded_by_qcr_id = superseded_by.map(str::to_string);
            moved.dsp_id = Some("dsp_77".into());
            let after = render_canonical(&moved, &lines).expect("render after the transition");
            assert_eq!(
                after, before,
                "moving state to {state:?} perturbed the hashed byte-form; every \
                 already-issued report's SHA pin would stop verifying"
            );
        }

        // Not vacuous: the fixture really does render something.
        assert!(
            before.len() > 512,
            "the fixture produced {} bytes — too small to be a real document, so \
             the equality above would prove nothing",
            before.len()
        );
    }

    /// The canonical form is idempotent — canonicalising an already-issued,
    /// unbound report changes nothing, so issuance and re-render agree.
    #[test]
    fn canonical_for_render_is_idempotent() {
        let once = canonical_for_render(&sample_report());
        let twice = canonical_for_render(&once);
        assert_eq!(once, twice);
    }

    /// The hash recipe is plain SHA-256 over the bytes, lowercase hex.
    /// Pinned against a known vector so a future refactor cannot quietly
    /// change what `rendered_sha256` means for every already-issued report.
    #[test]
    fn sha256_hex_is_plain_lowercase_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
