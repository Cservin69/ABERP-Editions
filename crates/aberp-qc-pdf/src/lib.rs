//! ADR-0199 §D5 — `aberp-qc-pdf`, the QC-report renderer.
//!
//! ## What this crate is
//!
//! A single pure function, [`render`], that turns ONE frozen
//! `qc_reports` + `qc_report_lines` snapshot into PDF bytes in one of
//! three shapes:
//!
//! | Shape | What it prints |
//! |---|---|
//! | [`QcReportKind::DimensionalInspection`] | Header, one row per characteristic per serialised unit (balloon #, name, type, nominal, tol −/+, **actual**, deviation, units, method, verdict), the traceability block, the accountability summary, the disposition, the signature block. |
//! | [`QcReportKind::CertificateOfConformance`] | One page: the conformance statement, part + drawing rev, qty, serial range, heat/lot + mill cert, the report number it certifies against, disposition, signature block. **No characteristic table.** |
//! | [`QcReportKind::As9102Fair`] | AS9102 **Rev C** Forms 1 / 2 / 3 — part-number accountability, product accountability, characteristic accountability. |
//!
//! ## Invariants
//!
//! **Pure — no clock, no I/O, no RNG, no async. Same inputs ⇒
//! byte-identical output.** This is load-bearing, not stylistic:
//! ADR-0199 §D7 retains reports by pinning the SHA-256 of the emitted
//! bytes into the hash-chained audit ledger and NOT storing the bytes.
//! That pin is only meaningful if a re-render in 2033 reproduces the
//! 2026 bytes exactly. Every value printed comes from the frozen rows;
//! nothing is derived from the environment.
//!
//! **No template engine.** ADR-0199 §D2 rejected Tera / Handlebars /
//! operator-uploaded layouts explicitly: a compliance document assembled
//! from operator-editable templates is a falsification surface — an
//! operator who can edit the template can hide a failing characteristic,
//! and nothing in the audit chain would show it. Layout lives in
//! reviewed Rust, so a change in the output is a code change with a diff
//! attached. That is *why* the SHA-256 pin means anything.
//!
//! **A `not_measured` line is printed, never dropped.** The renderer has
//! no filter that could omit a row. It prints `—` in the actual column
//! and `NOT MEASURED` in the verdict column, so the gap is visible on
//! paper and not merely in a count.
//!
//! ## Pushback against reuse
//!
//! `aberp-invoice-pdf` is organised around NAV §169/§172 invariants
//! (party blocks, VAT-rate breakdown, HUF/EUR rate stamping, per-line tax
//! columns) with no correspondence in a QC report — the same reasoning
//! `aberp-quote-pdf` recorded when it declined to build on it. The
//! ADR-0044 silver/gold palette and the footer identity grammar are
//! ported the way `aberp-quote-pdf` ported them, so all three
//! customer-facing documents read as one company. The day a third caller
//! needs the Helvetica/WinAnsi byte tables, a `pdf-style-helpers` crate
//! is the right factor-out — same call as `aberp-quote-pdf` made,
//! unchanged.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};
use thiserror::Error;

pub use aberp_qa::{
    Accountability, CharacteristicDesignator, CharacteristicType, Disposition, InspectionMethod,
    QcReport, QcReportKind, QcReportLine, QcReportTemplate, Verdict,
};

/// Crate version stamp — printed in the footer and recorded as
/// `renderer_version` on the issued report, so an auditor can tell which
/// renderer produced the bytes a SHA was taken over.
pub const QC_PDF_RENDERER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The AS9102 revision this crate's FAIR forms implement. Ervin confirmed
/// **Rev C** explicitly (ADR-0199 §Open Q1). Printed on Form 1 so the
/// document states which revision it claims to be.
pub const AS9102_REVISION: &str = "Rev C";

// ─── Page geometry (A4), matching aberp-quote-pdf ─────────────────────
const PAGE_WIDTH: i64 = 595;
const PAGE_HEIGHT: i64 = 842;
const MARGIN_LEFT: i64 = 40;
const MARGIN_RIGHT: i64 = PAGE_WIDTH - 40;
const MARGIN_TOP: i64 = PAGE_HEIGHT - 56;
/// Flowing-content floor. The footer block occupies up to `footer_y + 30
/// = 94`; a body row drawn at or below this `y` would collide with it, so
/// the characteristic table page-breaks here.
const CONTENT_BOTTOM: i64 = 120;

// ─── ADR-0044 silver / gold palette, ported from invoice + quote ──────

/// RGB triple in 0..=1, encoded for `lopdf`'s `Object::Real` (f32).
type Color = (f32, f32, f32);

/// Body ink — warm near-black.
const INK: Color = (0.13, 0.13, 0.15);
/// Section labels + column headers. Refined silver-grey.
const MUTED: Color = (0.46, 0.47, 0.51);
/// Structural rules. Soft warm silver.
const SILVER_LINE: Color = (0.72, 0.72, 0.74);
/// The ONE gold accent per ADR-0044's restraint: the rule above the
/// disposition banner.
const GOLD_ACCENT: Color = (0.72, 0.54, 0.12);
/// Failure / refusal red — a failing verdict, a `NOT MEASURED` cell, and
/// the `REJECT` / `INCOMPLETE` disposition banner.
const DANGER_RED: Color = (0.75, 0.0, 0.0);
/// Caution amber — `CAL-STALE`, which is neither a pass nor a fail.
const CAUTION_AMBER: Color = (0.70, 0.45, 0.0);

/// Seller identity. Single-tenant, same posture (and same values) as
/// `aberp-quote-pdf`: the pure renderer has no app-config access, and both
/// values are owned by `apps/aberp/src/build_profile.rs::expected_tenant_identity`.
const SELLER_LEGAL_NAME: &str = "Áben Consulting KFT.";
const SELLER_TAX_NUMBER: &str = "24904362-2-41";

/// The conformance statement printed on the CoC. Fixed wording, in code,
/// for the same reason the layout is in code: an editable conformance
/// statement is a falsification surface.
const COC_STATEMENT: &str = concat!(
    "We hereby certify that the parts identified below were manufactured, inspected and ",
    "tested in accordance with the applicable drawing revision and the purchase-order ",
    "requirements, and that they conform in all respects unless a deviation is recorded on ",
    "this certificate. Objective evidence of conformity is retained in the referenced QC ",
    "inspection report and in the tamper-evident audit chain entry cited below."
);

/// Party identification for the customer block. Supplied by the caller
/// (the app layer owns the `partners` table; this crate does not).
#[derive(Debug, Clone, Default)]
pub struct QcPartyInfo<'a> {
    /// Customer display / legal name.
    pub name: &'a str,
    /// Optional single-line address.
    pub address_line: &'a str,
    /// Optional customer purchase-order reference.
    pub purchase_order: &'a str,
}

/// Everything [`render`] needs. All of it comes from frozen rows.
#[derive(Debug, Clone)]
pub struct QcReportInputs<'a> {
    /// The frozen `qc_reports` row.
    pub report: &'a QcReport,
    /// The frozen `qc_report_lines`, in `line_no` order.
    pub lines: &'a [QcReportLine],
    /// The customer this document is for.
    pub customer: QcPartyInfo<'a>,
    /// The chain entry an auditor can look the issuance up by. Empty
    /// string when the report is still a draft.
    pub chain_reference: &'a str,
}

/// Failure taxonomy for [`render`].
#[derive(Debug, Error)]
pub enum QcPdfError {
    /// `lopdf` rejected the document on `save_to`. Indicates a bug in this
    /// crate, not bad input; surfaced loud per CLAUDE.md rule 12.
    #[error("lopdf save failed: {0}")]
    LopdfSave(String),
}

/// Render a QC document. **Pure**: no clock, no I/O, no RNG.
///
/// The shape is chosen by `inputs.report.report_kind`; the template on
/// the report row selects the house vs AS9102 header grammar. The
/// (kind, template) pairing was already validated at freeze time by
/// `QcReportTemplate::permits`, so this function renders whatever it is
/// given rather than second-guessing a frozen row.
pub fn render(inputs: &QcReportInputs<'_>) -> Result<Vec<u8>, QcPdfError> {
    let mut doc = Document::with_version("1.5");

    let pages_id = doc.new_object_id();
    let helvetica_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let helvetica_bold_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => helvetica_id,
            "F2" => helvetica_bold_id,
        },
    });

    let page_streams = match inputs.report.report_kind {
        QcReportKind::DimensionalInspection => build_dimensional(inputs),
        QcReportKind::CertificateOfConformance => build_coc(inputs),
        QcReportKind::As9102Fair => build_fair(inputs),
    };

    let mut kids: Vec<Object> = Vec::with_capacity(page_streams.len());
    for ops in page_streams {
        let content = Content { operations: ops };
        let content_stream = Stream::new(
            dictionary! {},
            content
                .encode()
                .map_err(|e| QcPdfError::LopdfSave(format!("encode content: {e}")))?,
        );
        let content_id = doc.add_object(content_stream);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(PAGE_WIDTH),
                Object::Integer(PAGE_HEIGHT),
            ],
            "Resources" => resources_id,
        });
        kids.push(Object::Reference(page_id));
    }

    let page_count = kids.len() as i64;
    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => page_count,
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let mut out = Vec::with_capacity(16384);
    doc.save_to(&mut out)
        .map_err(|e| QcPdfError::LopdfSave(e.to_string()))?;
    Ok(out)
}

// ─── Shape 1: the per-shipment dimensional inspection report ──────────

fn build_dimensional(inputs: &QcReportInputs<'_>) -> Vec<Vec<Operation>> {
    let r = inputs.report;
    let mut pages: Vec<Vec<Operation>> = Vec::new();
    let mut ops: Vec<Operation> = Vec::with_capacity(512);

    let title = match r.template {
        QcReportTemplate::As9102RevC => "Dimensional Inspection Report (AS9102 Rev C basis)",
        _ => "Dimensional Inspection Report",
    };
    let mut y = push_header(&mut ops, inputs, title);

    y = push_identity_block(&mut ops, inputs, y);
    y -= 6;

    // ── Characteristic table ──
    push_text_c(&mut ops, MARGIN_LEFT, y, "F2", 10, MUTED, "CHARACTERISTICS");
    y -= 14;
    push_char_table_header(&mut ops, y);
    y -= 6;
    push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y);
    y -= 12;

    for line in inputs.lines {
        if y < CONTENT_BOTTOM {
            y = page_break(&mut pages, &mut ops, inputs);
            push_char_table_header(&mut ops, y);
            y -= 6;
            push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y);
            y -= 12;
        }
        push_char_row(&mut ops, y, line);
        y -= 11;
    }

    y -= 10;
    if y < CONTENT_BOTTOM + 90 {
        y = page_break(&mut pages, &mut ops, inputs);
    }
    y = push_accountability_block(&mut ops, r, y);
    y = push_traceability_block(&mut ops, r, y);
    push_disposition_banner(&mut ops, r, y);
    let y = y - 46;
    push_signature_block(&mut ops, inputs, y);

    push_footer(&mut ops, inputs);
    pages.push(ops);
    pages
}

// ─── Shape 2: the Certificate of Conformance ──────────────────────────

fn build_coc(inputs: &QcReportInputs<'_>) -> Vec<Vec<Operation>> {
    let r = inputs.report;
    let mut ops: Vec<Operation> = Vec::with_capacity(128);
    let mut y = push_header(&mut ops, inputs, "Certificate of Conformance");

    // The statement, wrapped. NO characteristic table — by design: a CoC
    // certifies conformity and cites the report that carries the numbers.
    for chunk in wrap_chunks(COC_STATEMENT, 104) {
        push_text_c(&mut ops, MARGIN_LEFT, y, "F1", 9, INK, &chunk);
        y -= 12;
    }
    y -= 10;
    push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y);
    y -= 18;

    y = push_identity_block(&mut ops, inputs, y);
    y = push_traceability_block(&mut ops, r, y);

    // The CoC states the accountability summary but not the rows — an
    // auditor reading the certificate alone must still be able to see
    // that nothing went unaccounted for.
    y = push_accountability_block(&mut ops, r, y);
    push_disposition_banner(&mut ops, r, y);
    let y = y - 46;
    push_signature_block(&mut ops, inputs, y);

    push_footer(&mut ops, inputs);
    vec![ops]
}

// ─── Shape 3: AS9102 Rev C, Forms 1 / 2 / 3 ───────────────────────────

fn build_fair(inputs: &QcReportInputs<'_>) -> Vec<Vec<Operation>> {
    let r = inputs.report;
    let mut pages: Vec<Vec<Operation>> = Vec::new();

    // ── Form 1 — Part Number Accountability ──
    let mut ops: Vec<Operation> = Vec::with_capacity(160);
    let mut y = push_header(
        &mut ops,
        inputs,
        &format!("AS9102 {AS9102_REVISION} — Form 1: Part Number Accountability"),
    );
    y = push_form_kv(&mut ops, y, "1. Part Number", &r.product_id);
    y = push_form_kv(
        &mut ops,
        y,
        "2. Part Name",
        first_characteristic_part_name(inputs),
    );
    y = push_form_kv(&mut ops, y, "3. Serial Number", opt(&r.serial_range));
    y = push_form_kv(&mut ops, y, "4. FAIR Identifier", &r.report_number);
    y = push_form_kv(&mut ops, y, "5. Part Revision Level", opt(&r.drawing_rev));
    y = push_form_kv(&mut ops, y, "6. Drawing Number", opt(&r.drawing_number));
    y = push_form_kv(
        &mut ops,
        y,
        "7. Drawing Revision Level",
        opt(&r.drawing_rev),
    );
    y = push_form_kv(&mut ops, y, "8. Additional Changes", opt(&r.notes));
    y = push_form_kv(&mut ops, y, "9. Manufacturing Process Reference", &r.wo_id);
    y = push_form_kv(&mut ops, y, "10. Organization Name", SELLER_LEGAL_NAME);
    y = push_form_kv(&mut ops, y, "11. Supplier Code", SELLER_TAX_NUMBER);
    y = push_form_kv(
        &mut ops,
        y,
        "12. P.O. Number",
        inputs.customer.purchase_order,
    );
    y = push_form_kv(&mut ops, y, "13. Detail / Assembly FAI", "Detail Part FAI");
    y = push_form_kv(&mut ops, y, "14. Full / Partial FAI", "Full FAI");
    y -= 8;
    push_disposition_banner(&mut ops, r, y);
    let y1 = y - 46;
    push_signature_block(&mut ops, inputs, y1);
    push_footer(&mut ops, inputs);
    pages.push(std::mem::take(&mut ops));

    // ── Form 2 — Product Accountability ──
    let mut y = push_header(
        &mut ops,
        inputs,
        &format!("AS9102 {AS9102_REVISION} — Form 2: Product Accountability"),
    );
    push_text_c(
        &mut ops,
        MARGIN_LEFT,
        y,
        "F1",
        8,
        MUTED,
        "Materials, special processes and functional testing accounted for on this first article.",
    );
    y -= 18;
    push_form2_header(&mut ops, y);
    y -= 6;
    push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y);
    y -= 12;

    // Form 2 covers the material / process / functional characteristics.
    let mut form2_rows = 0usize;
    for line in inputs.lines.iter().filter(|l| is_form2_line(l)) {
        if y < CONTENT_BOTTOM {
            y = page_break(&mut pages, &mut ops, inputs);
            push_form2_header(&mut ops, y);
            y -= 18;
        }
        push_form2_row(&mut ops, y, line, r);
        y -= 11;
        form2_rows += 1;
    }
    if form2_rows == 0 {
        // An EMPTY Form 2 is stated, not left blank. A blank form reads as
        // "not filled in"; this states the fact that no material, process
        // or functional characteristic was defined for the part.
        push_text_c(
            &mut ops,
            MARGIN_LEFT,
            y,
            "F1",
            9,
            CAUTION_AMBER,
            "No material, special-process or functional characteristics are defined for this part.",
        );
        y -= 14;
    }
    y -= 10;
    y = push_traceability_block(&mut ops, r, y);
    push_signature_block(&mut ops, inputs, y - 6);
    push_footer(&mut ops, inputs);
    pages.push(std::mem::take(&mut ops));

    // ── Form 3 — Characteristic Accountability ──
    let mut y = push_header(
        &mut ops,
        inputs,
        &format!("AS9102 {AS9102_REVISION} — Form 3: Characteristic Accountability"),
    );
    push_char_table_header(&mut ops, y);
    y -= 6;
    push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y);
    y -= 12;
    for line in inputs.lines {
        if y < CONTENT_BOTTOM {
            y = page_break(&mut pages, &mut ops, inputs);
            push_char_table_header(&mut ops, y);
            y -= 6;
            push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y);
            y -= 12;
        }
        push_char_row(&mut ops, y, line);
        y -= 11;
    }
    y -= 10;
    if y < CONTENT_BOTTOM + 80 {
        y = page_break(&mut pages, &mut ops, inputs);
    }
    y = push_accountability_block(&mut ops, r, y);
    push_disposition_banner(&mut ops, r, y);
    push_signature_block(&mut ops, inputs, y - 46);
    push_footer(&mut ops, inputs);
    pages.push(ops);

    pages
}

/// Form 2 covers material, special-process and functional characteristics
/// — the non-dimensional half of the part's conformity.
fn is_form2_line(l: &QcReportLine) -> bool {
    matches!(
        l.characteristic_type,
        CharacteristicType::Material | CharacteristicType::Process | CharacteristicType::Functional
    )
}

/// Form 1 field 2 ("Part Name"). ABERP has no part-name field distinct
/// from `product_id`, so the product id is printed rather than a guess.
fn first_characteristic_part_name<'a>(inputs: &'a QcReportInputs<'a>) -> &'a str {
    &inputs.report.product_id
}

// ─── Shared blocks ────────────────────────────────────────────────────

/// Title band + report number/date meta + under-rule. Returns the `y` the
/// body starts at.
fn push_header(ops: &mut Vec<Operation>, inputs: &QcReportInputs<'_>, title: &str) -> i64 {
    let r = inputs.report;
    let mut y = MARGIN_TOP;
    push_text_c(ops, MARGIN_LEFT, y, "F2", 18, INK, title);
    y -= 20;
    push_text_c(
        ops,
        MARGIN_LEFT,
        y,
        "F1",
        9,
        MUTED,
        &format!(
            "Report {}   ·   {}   ·   {}",
            r.report_number,
            r.state.as_str().to_uppercase(),
            r.issued_at_utc.as_deref().unwrap_or("not issued"),
        ),
    );
    y -= 18;
    push_rule(ops, MARGIN_LEFT, MARGIN_RIGHT, y);
    y - 18
}

/// Customer + part + drawing identity, two columns.
fn push_identity_block(ops: &mut Vec<Operation>, inputs: &QcReportInputs<'_>, mut y: i64) -> i64 {
    let r = inputs.report;
    let c = &inputs.customer;
    let right = MARGIN_LEFT + 280;

    push_text_c(ops, MARGIN_LEFT, y, "F2", 10, MUTED, "CUSTOMER");
    push_text_c(ops, right, y, "F2", 10, MUTED, "PART");
    y -= 14;
    push_kv_at(ops, MARGIN_LEFT, y, "Name", c.name);
    push_kv_at(ops, right, y, "Part number", &r.product_id);
    y -= 12;
    push_kv_at(ops, MARGIN_LEFT, y, "Address", c.address_line);
    push_kv_at(ops, right, y, "Drawing", opt(&r.drawing_number));
    y -= 12;
    push_kv_at(ops, MARGIN_LEFT, y, "P.O.", c.purchase_order);
    push_kv_at(ops, right, y, "Drawing rev", opt(&r.drawing_rev));
    y -= 12;
    push_kv_at(ops, MARGIN_LEFT, y, "Work order", &r.wo_id);
    push_kv_at(
        ops,
        right,
        y,
        "Quantity",
        &format!("{} pcs", r.qty_reported),
    );
    y -= 12;
    push_kv_at(ops, MARGIN_LEFT, y, "Report no.", &r.report_number);
    push_kv_at(ops, right, y, "Serials", opt(&r.serial_range));
    y -= 20;
    y
}

// NOTE — the dispatch id is deliberately NOT printed on these documents.
//
// A report must be ISSUED before the ADR-0199 §D6 gate lets the shipment
// proceed, so `qc_reports.dsp_id` is assigned strictly AFTER the SHA-256 of
// the rendered bytes is pinned (§D7). Printing a field that changes after
// issuance would make EVERY correctly shipped report report itself as
// tampered on the next download. The caller normalises it away before
// rendering (`aberp::qc_report::canonical_for_render`) — this comment
// exists so a future edit does not "restore the missing row" and silently
// break the pin. The linkage is not lost: it is a chain event
// (`qcr.report_attached_to_shipment`) and it is on both HTTP surfaces.

/// Traceability — heat/lot, mill cert, machine, NC program, source quote.
fn push_traceability_block(ops: &mut Vec<Operation>, r: &QcReport, mut y: i64) -> i64 {
    push_text_c(ops, MARGIN_LEFT, y, "F2", 10, MUTED, "TRACEABILITY");
    y -= 14;
    let right = MARGIN_LEFT + 280;
    push_kv_at(
        ops,
        MARGIN_LEFT,
        y,
        "Heat / lot",
        opt(&r.heat_lot_reference),
    );
    push_kv_at(ops, right, y, "Machine", opt(&r.machine_id));
    y -= 12;
    push_kv_at(ops, MARGIN_LEFT, y, "Mill cert", opt(&r.mill_cert_id));
    push_kv_at(ops, right, y, "NC program", opt(&r.program_id));
    y -= 12;
    push_kv_at(ops, MARGIN_LEFT, y, "Source quote", opt(&r.source_quote_id));
    push_kv_at(ops, right, y, "Partner", &r.partner_id);
    y -= 20;
    y
}

/// The accountability summary — the numbers that decide the disposition.
///
/// `unaccounted` prints in RED whenever it is non-zero, because that
/// single number is what refuses the shipment.
fn push_accountability_block(ops: &mut Vec<Operation>, r: &QcReport, mut y: i64) -> i64 {
    push_text_c(
        ops,
        MARGIN_LEFT,
        y,
        "F2",
        10,
        MUTED,
        "CHARACTERISTIC ACCOUNTABILITY",
    );
    y -= 14;
    push_text_c(
        ops,
        MARGIN_LEFT,
        y,
        "F1",
        9,
        INK,
        &format!(
            "Required: {}    Measured: {}    Passed: {}    Failed: {}",
            r.characteristics_required,
            r.characteristics_measured,
            r.characteristics_passed,
            r.characteristics_failed,
        ),
    );
    y -= 12;
    let unaccounted = format!("Unaccounted for: {}", r.characteristics_unaccounted);
    let color = if r.characteristics_unaccounted > 0 {
        DANGER_RED
    } else {
        INK
    };
    push_text_c(ops, MARGIN_LEFT, y, "F2", 9, color, &unaccounted);
    y -= 18;
    y
}

/// The disposition banner: one gold rule and the verdict word, coloured
/// by whether it permits a shipment.
fn push_disposition_banner(ops: &mut Vec<Operation>, r: &QcReport, y: i64) {
    push_rule_gold(ops, MARGIN_LEFT, MARGIN_RIGHT, y + 12);
    let (label, color) = match r.disposition {
        Disposition::Accept => ("ACCEPT", INK),
        Disposition::AcceptWithNcr => ("ACCEPT WITH NCR", CAUTION_AMBER),
        Disposition::Reject => ("REJECT", DANGER_RED),
        Disposition::Incomplete => ("INCOMPLETE — NOT RELEASABLE", DANGER_RED),
    };
    push_text_c(ops, MARGIN_LEFT, y - 4, "F2", 13, color, label);
    if !r.disposition.permits_shipment() {
        push_text_c(
            ops,
            MARGIN_LEFT,
            y - 18,
            "F1",
            8,
            DANGER_RED,
            "This report does not release the parts for shipment.",
        );
    }
}

/// Signature block. Phase 1 is printed name + operator login + the chain
/// reference — NOT a cryptographic signature (ADR-0199 §Open Q5, default
/// accepted). The document says so, rather than implying a signing
/// ceremony that did not happen.
fn push_signature_block(ops: &mut Vec<Operation>, inputs: &QcReportInputs<'_>, mut y: i64) {
    let r = inputs.report;
    push_rule(ops, MARGIN_LEFT, MARGIN_RIGHT, y + 14);
    push_text_c(ops, MARGIN_LEFT, y, "F2", 9, MUTED, "AUTHORISED BY");
    y -= 12;
    push_kv_at(
        ops,
        MARGIN_LEFT,
        y,
        "Name",
        r.issued_by.as_deref().unwrap_or("—"),
    );
    push_kv_at(
        ops,
        MARGIN_LEFT + 280,
        y,
        "Date",
        r.issued_at_utc.as_deref().unwrap_or("—"),
    );
    y -= 12;
    push_kv_at(
        ops,
        MARGIN_LEFT,
        y,
        "Audit chain ref",
        if inputs.chain_reference.is_empty() {
            "—"
        } else {
            inputs.chain_reference
        },
    );
    y -= 12;
    push_text_c(
        ops,
        MARGIN_LEFT,
        y,
        "F1",
        7,
        MUTED,
        "Attributed by operator login and tamper-evident audit-chain entry; not a qualified electronic signature.",
    );
}

fn push_char_table_header(ops: &mut Vec<Operation>, y: i64) {
    for (x, label) in CHAR_COLUMNS {
        push_text_c(ops, *x, y, "F2", 7, MUTED, label);
    }
}

/// Column layout for the characteristic table. One table, used by both
/// the dimensional report and AS9102 Form 3 — the ADR's point that Form 3
/// is a strict superset means the per-shipment report is a projection of
/// it, not a second model.
const CHAR_COLUMNS: &[(i64, &str)] = &[
    (MARGIN_LEFT, "#"),
    (MARGIN_LEFT + 22, "SERIAL"),
    (MARGIN_LEFT + 78, "CHARACTERISTIC"),
    (MARGIN_LEFT + 192, "TYPE"),
    (MARGIN_LEFT + 232, "NOMINAL"),
    (MARGIN_LEFT + 282, "-TOL"),
    (MARGIN_LEFT + 318, "+TOL"),
    (MARGIN_LEFT + 354, "ACTUAL"),
    (MARGIN_LEFT + 404, "DEV"),
    (MARGIN_LEFT + 444, "METHOD"),
    (MARGIN_LEFT + 486, "RESULT"),
];

/// One characteristic row.
///
/// A `not_measured` line prints `—` in every measurement cell and
/// `NOT MEASURED` in red under RESULT. There is deliberately NO code path
/// here that skips a line.
fn push_char_row(ops: &mut Vec<Operation>, y: i64, l: &QcReportLine) {
    let cells: [String; 11] = [
        l.line_no.to_string(),
        truncate(l.part_serial.as_deref().unwrap_or("LOT"), 9),
        truncate(&characteristic_label(l), 19),
        short_type(l.characteristic_type).to_string(),
        num(l.nominal_value),
        num(l.lower_tol),
        num(l.upper_tol),
        num(l.actual_value),
        num(l.deviation),
        short_method(l.inspection_method).to_string(),
        String::new(), // RESULT is drawn separately (it is coloured).
    ];
    for (i, (x, _)) in CHAR_COLUMNS.iter().enumerate() {
        if i == CHAR_COLUMNS.len() - 1 {
            break;
        }
        push_text_c(ops, *x, y, "F1", 7, INK, &cells[i]);
    }
    let (result, color) = result_cell(l);
    let x_result = CHAR_COLUMNS[CHAR_COLUMNS.len() - 1].0;
    push_text_c(ops, x_result, y, "F2", 7, color, result);
}

/// The RESULT cell's text and colour. This is the one place a reader's
/// eye lands, so every outcome is named explicitly and none of them is
/// blank.
fn result_cell(l: &QcReportLine) -> (&'static str, Color) {
    match l.accountability {
        Accountability::NotMeasured => ("NOT MEASURED", DANGER_RED),
        Accountability::NotApplicable => ("N/A", MUTED),
        Accountability::Measured => match l.verdict {
            Some(Verdict::Pass) => ("PASS", INK),
            Some(Verdict::Minor) => ("FAIL/MIN", DANGER_RED),
            Some(Verdict::Major) => ("FAIL/MAJ", DANGER_RED),
            Some(Verdict::Critical) => ("FAIL/CRIT", DANGER_RED),
            // A stale-calibration measurement is NOT a pass: a probe that
            // may be lying is not evidence of conformity (ISO 9001
            // §7.1.5.2). It prints as its own outcome, in amber.
            Some(Verdict::CalibrationStale) => ("CAL-STALE", CAUTION_AMBER),
            // Only reachable on a tampered row (the writer sets verdict and
            // accountability together). Named loudly rather than blank.
            None => ("NO VERDICT", DANGER_RED),
        },
    }
}

fn push_form2_header(ops: &mut Vec<Operation>, y: i64) {
    push_text_c(ops, MARGIN_LEFT, y, "F2", 7, MUTED, "#");
    push_text_c(
        ops,
        MARGIN_LEFT + 22,
        y,
        "F2",
        7,
        MUTED,
        "MATERIAL / PROCESS / TEST",
    );
    push_text_c(ops, MARGIN_LEFT + 220, y, "F2", 7, MUTED, "TYPE");
    push_text_c(ops, MARGIN_LEFT + 270, y, "F2", 7, MUTED, "SPECIFICATION");
    push_text_c(ops, MARGIN_LEFT + 380, y, "F2", 7, MUTED, "CERT / LOT");
    push_text_c(ops, MARGIN_LEFT + 486, y, "F2", 7, MUTED, "RESULT");
}

fn push_form2_row(ops: &mut Vec<Operation>, y: i64, l: &QcReportLine, r: &QcReport) {
    push_text_c(ops, MARGIN_LEFT, y, "F1", 7, INK, &l.line_no.to_string());
    push_text_c(
        ops,
        MARGIN_LEFT + 22,
        y,
        "F1",
        7,
        INK,
        &truncate(&l.characteristic_name, 33),
    );
    push_text_c(
        ops,
        MARGIN_LEFT + 220,
        y,
        "F1",
        7,
        INK,
        short_type(l.characteristic_type),
    );
    push_text_c(
        ops,
        MARGIN_LEFT + 270,
        y,
        "F1",
        7,
        INK,
        &truncate(l.sheet_zone.as_deref().unwrap_or("—"), 18),
    );
    push_text_c(
        ops,
        MARGIN_LEFT + 380,
        y,
        "F1",
        7,
        INK,
        &truncate(
            r.mill_cert_id
                .as_deref()
                .or(r.heat_lot_reference.as_deref())
                .unwrap_or("—"),
            17,
        ),
    );
    let (result, color) = result_cell(l);
    push_text_c(ops, MARGIN_LEFT + 486, y, "F2", 7, color, result);
}

fn push_form_kv(ops: &mut Vec<Operation>, y: i64, label: &str, value: &str) -> i64 {
    push_text_c(ops, MARGIN_LEFT, y, "F1", 9, MUTED, label);
    push_text_c(ops, MARGIN_LEFT + 200, y, "F1", 9, INK, blank_dash(value));
    y - 15
}

/// Finalize the current page (stamp its footer) and return the fresh
/// top-of-page `y`.
fn page_break(
    pages: &mut Vec<Vec<Operation>>,
    ops: &mut Vec<Operation>,
    inputs: &QcReportInputs<'_>,
) -> i64 {
    push_footer(ops, inputs);
    pages.push(std::mem::take(ops));
    MARGIN_TOP
}

/// Seller identity + the SHA-256 pin. The hash is printed on every page:
/// it is the retention mechanism (ADR-0199 §D7), so the document itself
/// carries the value an auditor re-computes.
fn push_footer(ops: &mut Vec<Operation>, inputs: &QcReportInputs<'_>) {
    let r = inputs.report;
    let footer_y = 64;
    push_text_c(
        ops,
        MARGIN_LEFT,
        footer_y + 30,
        "F2",
        8,
        INK,
        &format!("{SELLER_LEGAL_NAME}  ·  Adószám: {SELLER_TAX_NUMBER}"),
    );
    push_rule(ops, MARGIN_LEFT, MARGIN_RIGHT, footer_y + 24);
    push_text_c(
        ops,
        MARGIN_LEFT,
        footer_y + 8,
        "F1",
        7,
        MUTED,
        &format!(
            "QC report {} · {} · renderer {}",
            r.report_number,
            r.report_kind.as_str(),
            r.renderer_version
                .as_deref()
                .unwrap_or(QC_PDF_RENDERER_VERSION),
        ),
    );
    push_text_c(
        ops,
        MARGIN_LEFT,
        footer_y - 4,
        "F1",
        7,
        MUTED,
        &format!(
            "Issued SHA-256: {}",
            r.rendered_sha256
                .as_deref()
                .unwrap_or("(draft — not issued)")
        ),
    );
}

// ─── Primitives (ported from aberp-quote-pdf) ─────────────────────────

fn push_text_c(
    ops: &mut Vec<Operation>,
    x: i64,
    y: i64,
    font_key: &str,
    size: i64,
    color: Color,
    s: &str,
) {
    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new(
        "Tf",
        vec![Object::Name(font_key.as_bytes().to_vec()), size.into()],
    ));
    ops.push(Operation::new(
        "rg",
        vec![
            Object::Real(color.0),
            Object::Real(color.1),
            Object::Real(color.2),
        ],
    ));
    ops.push(Operation::new("Td", vec![x.into(), y.into()]));
    ops.push(Operation::new(
        "Tj",
        vec![Object::String(
            winansi_bytes(s),
            lopdf::StringFormat::Literal,
        )],
    ));
    ops.push(Operation::new("ET", vec![]));
}

fn push_kv_at(ops: &mut Vec<Operation>, x: i64, y: i64, label: &str, value: &str) {
    push_text_c(ops, x, y, "F1", 8, MUTED, label);
    push_text_c(ops, x + 86, y, "F1", 8, INK, blank_dash(value));
}

fn push_rule(ops: &mut Vec<Operation>, x0: i64, x1: i64, y: i64) {
    push_rule_c(ops, x0, x1, y, SILVER_LINE, 0.5);
}

fn push_rule_gold(ops: &mut Vec<Operation>, x0: i64, x1: i64, y: i64) {
    push_rule_c(ops, x0, x1, y, GOLD_ACCENT, 0.85);
}

fn push_rule_c(ops: &mut Vec<Operation>, x0: i64, x1: i64, y: i64, color: Color, weight: f32) {
    ops.push(Operation::new(
        "RG",
        vec![
            Object::Real(color.0),
            Object::Real(color.1),
            Object::Real(color.2),
        ],
    ));
    ops.push(Operation::new("w", vec![Object::Real(weight)]));
    ops.push(Operation::new("m", vec![x0.into(), y.into()]));
    ops.push(Operation::new("l", vec![x1.into(), y.into()]));
    ops.push(Operation::new("S", vec![]));
}

/// The characteristic's printed label: balloon number when it has one,
/// then the name.
fn characteristic_label(l: &QcReportLine) -> String {
    match l.characteristic_number.as_deref() {
        Some(n) if !n.trim().is_empty() => format!("[{}] {}", n.trim(), l.characteristic_name),
        _ => l.characteristic_name.clone(),
    }
}

fn short_type(t: CharacteristicType) -> &'static str {
    match t {
        CharacteristicType::Dimensional => "DIM",
        CharacteristicType::Material => "MAT",
        CharacteristicType::Process => "PROC",
        CharacteristicType::Note => "NOTE",
        CharacteristicType::Functional => "FUNC",
    }
}

fn short_method(m: Option<InspectionMethod>) -> &'static str {
    match m {
        Some(InspectionMethod::OnMachineProbe) => "PROBE",
        Some(InspectionMethod::Cmm) => "CMM",
        Some(InspectionMethod::Gauge) => "GAUGE",
        Some(InspectionMethod::Visual) => "VISUAL",
        Some(InspectionMethod::CertReview) => "CERT",
        None => "—",
    }
}

/// Format an optional measurement. `None` prints an em-dash, NEVER a
/// zero: a blank cell is honest about an absent measurement, a `0.000`
/// would be a fabricated one.
fn num(v: Option<f64>) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.4}"),
        // A non-finite value can only arrive via a tampered row; naming it
        // beats printing "NaN" or silently blanking it.
        Some(_) => "INVALID".to_string(),
        None => "—".to_string(),
    }
}

fn opt(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("—")
}

fn blank_dash(s: &str) -> &str {
    if s.trim().is_empty() {
        "—"
    } else {
        s
    }
}

/// Truncate to `max` CHARACTERS (not bytes), marking the cut with `…` so
/// a reader can tell the value was elided rather than short.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn wrap_chunks(s: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in s.split_whitespace() {
        if current.chars().count() + 1 + word.chars().count() > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// WinAnsi byte-encode. Hungarian double-acute falls back to
/// single-acute; unknown chars emit `?` (visible loud, not silent loss).
fn winansi_bytes(s: &str) -> Vec<u8> {
    s.chars().map(winansi_byte_for_char).collect()
}

fn winansi_byte_for_char(c: char) -> u8 {
    match c {
        c if (c as u32) < 0x80 => c as u8,
        '\u{0150}' => 0xD6, // Ő → Ö
        '\u{0151}' => 0xF6, // ő → ö
        '\u{0170}' => 0xDC, // Ű → Ü
        '\u{0171}' => 0xFC, // ű → ü
        '\u{20AC}' => 0x80, // €
        '\u{2014}' => 0x97, // — em dash
        '\u{2013}' => 0x96, // – en dash
        '\u{2026}' => 0x85, // … ellipsis
        '\u{00B7}' => 0xB7, // · middle dot
        c if (c as u32) >= 0xA0 && (c as u32) <= 0xFF => c as u8,
        _ => b'?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(kind: QcReportKind, template: QcReportTemplate) -> QcReport {
        QcReport {
            qcr_id: "qcr_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            report_number: "QCR-2026-0001".into(),
            report_kind: kind,
            template,
            state: aberp_qa::QcReportState::Issued,
            wo_id: "wo_1".into(),
            product_id: "prd_bracket".into(),
            dsp_id: Some("dsp_77".into()),
            partner_id: "ptr_prime".into(),
            source_quote_id: None,
            drawing_number: Some("DWG-4471".into()),
            drawing_rev: Some("C".into()),
            qty_reported: 2,
            serial_range: Some("SN-001 … SN-002 (2 units)".into()),
            heat_lot_reference: Some("HL-9911".into()),
            mill_cert_id: None,
            machine_id: Some("NTX-2000".into()),
            program_id: Some("O4471".into()),
            disposition: Disposition::Accept,
            characteristics_required: 4,
            characteristics_measured: 4,
            characteristics_passed: 4,
            characteristics_failed: 0,
            characteristics_unaccounted: 0,
            rendered_sha256: None,
            renderer_version: Some("aberp-qc-pdf@0.0.0".into()),
            issued_at_utc: Some("2026-08-23T12:00:00Z".into()),
            issued_by: Some("ervin".into()),
            superseded_by_qcr_id: None,
            created_at: "2026-08-23T11:00:00Z".into(),
            created_by: "ervin".into(),
            notes: None,
        }
    }

    fn line(no: u32, serial: Option<&str>, measured: bool) -> QcReportLine {
        QcReportLine {
            qcrl_id: format!("qcrl_{no:026}"),
            qcr_id: "qcr_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            line_no: no,
            part_serial: serial.map(str::to_string),
            part_uid: serial.map(|s| format!("uid_{s}")),
            characteristic_number: Some(no.to_string()),
            characteristic_name: format!("Feature {no}"),
            characteristic_designator: Some(CharacteristicDesignator::Key),
            characteristic_type: CharacteristicType::Dimensional,
            inspection_method: Some(InspectionMethod::OnMachineProbe),
            sheet_zone: Some("1/B4".into()),
            nominal_value: Some(25.0),
            upper_tol: Some(0.05),
            lower_tol: Some(-0.05),
            units: Some("mm".into()),
            actual_value: if measured { Some(25.012) } else { None },
            deviation: if measured { Some(0.012) } else { None },
            verdict: if measured { Some(Verdict::Pass) } else { None },
            accountability: if measured {
                Accountability::Measured
            } else {
                Accountability::NotMeasured
            },
            qci_id: measured.then(|| "qci_x".to_string()),
            measured_at_utc: measured.then(|| "2026-08-23T10:00:00Z".to_string()),
            measured_by: measured.then(|| "ervin".to_string()),
            probe_serial: measured.then(|| "RMP600-007".to_string()),
            created_at: "2026-08-23T11:00:00Z".into(),
            required: true,
        }
    }

    fn inputs<'a>(r: &'a QcReport, lines: &'a [QcReportLine]) -> QcReportInputs<'a> {
        QcReportInputs {
            report: r,
            lines,
            customer: QcPartyInfo {
                name: "Prime Aerospace Kft.",
                address_line: "1117 Budapest, Fő utca 1., HU",
                purchase_order: "PO-2026-889",
            },
            chain_reference: "aud_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        }
    }

    /// **The renderer is byte-deterministic** (ADR-0199 §AC3).
    ///
    /// This is the invariant the whole retention design rests on: §D7
    /// stores the SHA-256 of the bytes and NOT the bytes, so a re-render in
    /// 2033 must reproduce the 2026 bytes exactly. If this test ever fails,
    /// every already-issued report's pin becomes unverifiable.
    #[test]
    fn render_is_byte_identical_across_calls_for_every_shape() {
        for kind in [
            QcReportKind::DimensionalInspection,
            QcReportKind::CertificateOfConformance,
            QcReportKind::As9102Fair,
        ] {
            let template = match kind {
                QcReportKind::As9102Fair => QcReportTemplate::As9102RevC,
                QcReportKind::CertificateOfConformance => QcReportTemplate::CocOnly,
                _ => QcReportTemplate::AbenStandard,
            };
            let r = report(kind, template);
            let lines = vec![
                line(1, Some("SN-001"), true),
                line(2, Some("SN-001"), false),
                line(3, Some("SN-002"), true),
                line(4, None, true),
            ];
            let a = render(&inputs(&r, &lines)).unwrap();
            let b = render(&inputs(&r, &lines)).unwrap();
            assert_eq!(
                a,
                b,
                "{} render is not byte-deterministic — the ADR-0199 §D7 SHA pin \
                 would be unverifiable",
                kind.as_str()
            );
            assert!(a.starts_with(b"%PDF-"), "output must be a PDF");
            assert!(!a.is_empty());
        }
    }

    /// A `not_measured` line is PRINTED. The renderer has no filter that
    /// can drop a row, and the RESULT cell names the gap explicitly.
    #[test]
    fn a_not_measured_line_renders_as_an_explicit_row() {
        let l = line(2, Some("SN-001"), false);
        let (result, color) = result_cell(&l);
        assert_eq!(result, "NOT MEASURED");
        assert_eq!(color, DANGER_RED);
        // Its measurement cells are em-dashes, never zeros.
        assert_eq!(num(l.actual_value), "—");
        assert_eq!(num(l.deviation), "—");
    }

    /// A stale-calibration line is its own outcome — not a pass, not a
    /// fail (ISO 9001 §7.1.5.2 / ADR-0199 §AC9).
    #[test]
    fn calibration_stale_renders_as_its_own_outcome() {
        let mut l = line(1, Some("SN-001"), true);
        l.verdict = Some(Verdict::CalibrationStale);
        let (result, color) = result_cell(&l);
        assert_eq!(result, "CAL-STALE");
        assert_eq!(color, CAUTION_AMBER);
        assert_ne!(result, "PASS");
    }

    /// Every failing tier renders in red and names its severity.
    #[test]
    fn failing_verdicts_render_red_and_named() {
        for (v, expected) in [
            (Verdict::Minor, "FAIL/MIN"),
            (Verdict::Major, "FAIL/MAJ"),
            (Verdict::Critical, "FAIL/CRIT"),
        ] {
            let mut l = line(1, Some("SN-001"), true);
            l.verdict = Some(v);
            let (result, color) = result_cell(&l);
            assert_eq!(result, expected);
            assert_eq!(color, DANGER_RED);
        }
    }

    /// A tampered row (measured, no verdict) is named loudly rather than
    /// rendering blank — a blank cell would read as "nothing to report".
    #[test]
    fn a_measured_line_with_no_verdict_is_named_not_blank() {
        let mut l = line(1, Some("SN-001"), true);
        l.verdict = None;
        let (result, color) = result_cell(&l);
        assert_eq!(result, "NO VERDICT");
        assert_eq!(color, DANGER_RED);
    }

    /// A non-releasing disposition says so on the page, not just in a
    /// field an operator has to know to look at.
    #[test]
    fn an_incomplete_report_states_that_it_does_not_release() {
        let mut r = report(
            QcReportKind::DimensionalInspection,
            QcReportTemplate::AbenStandard,
        );
        r.disposition = Disposition::Incomplete;
        r.characteristics_unaccounted = 1;
        let lines = vec![line(1, Some("SN-001"), false)];
        let bytes = render(&inputs(&r, &lines)).unwrap();
        // The banner text is emitted as a literal PDF string, so it is
        // findable in the uncompressed content stream this crate produces.
        let haystack = String::from_utf8_lossy(&bytes);
        assert!(
            haystack.contains("INCOMPLETE") || haystack.contains("not release"),
            "an incomplete report must say on its face that it does not release the parts"
        );
    }

    /// The FAIR is three pages (Forms 1 / 2 / 3) at minimum.
    #[test]
    fn the_fair_emits_forms_one_two_and_three() {
        let r = report(QcReportKind::As9102Fair, QcReportTemplate::As9102RevC);
        let lines = vec![line(1, Some("SN-001"), true)];
        let pages = build_fair(&inputs(&r, &lines));
        assert_eq!(pages.len(), 3, "AS9102 Rev C is Forms 1, 2 and 3");
        assert!(pages.iter().all(|p| !p.is_empty()));
    }

    /// The CoC prints NO characteristic table — a certificate cites the
    /// report that carries the numbers, it does not restate them.
    #[test]
    fn the_coc_is_one_page_with_no_characteristic_table() {
        let r = report(
            QcReportKind::CertificateOfConformance,
            QcReportTemplate::CocOnly,
        );
        let lines = vec![
            line(1, Some("SN-001"), true),
            line(2, Some("SN-001"), true),
            line(3, Some("SN-002"), true),
        ];
        let pages = build_coc(&inputs(&r, &lines));
        assert_eq!(pages.len(), 1);
        let text = String::from_utf8_lossy(&render(&inputs(&r, &lines)).unwrap()).to_string();
        assert!(
            !text.contains("CHARACTERISTICS"),
            "the CoC has no characteristic table"
        );
        assert!(text.contains("hereby certify") || text.contains("certify"));
    }

    /// A long characteristic table page-breaks instead of overflowing the
    /// footer, and stays deterministic while doing it.
    #[test]
    fn a_long_table_paginates_deterministically() {
        let r = report(
            QcReportKind::DimensionalInspection,
            QcReportTemplate::AbenStandard,
        );
        let lines: Vec<QcReportLine> = (1..=180)
            .map(|i| line(i, Some("SN-001"), i % 3 != 0))
            .collect();
        let pages = build_dimensional(&inputs(&r, &lines));
        assert!(pages.len() > 1, "180 rows must not fit on one page");
        let a = render(&inputs(&r, &lines)).unwrap();
        let b = render(&inputs(&r, &lines)).unwrap();
        assert_eq!(a, b);
    }

    /// The renderer never panics on any line shape the writer can produce
    /// — including empty reports, absent optionals and non-finite values a
    /// tampered row could carry.
    #[test]
    fn render_never_panics_on_degenerate_input() {
        let mut r = report(
            QcReportKind::DimensionalInspection,
            QcReportTemplate::AbenStandard,
        );
        r.drawing_number = None;
        r.drawing_rev = None;
        r.serial_range = None;
        r.heat_lot_reference = None;
        r.machine_id = None;
        r.program_id = None;
        r.issued_by = None;
        r.issued_at_utc = None;
        r.renderer_version = None;
        r.dsp_id = None;

        // Empty report.
        assert!(render(&inputs(&r, &[])).is_ok());

        // Every optional absent, and a non-finite actual.
        let mut l = line(1, None, true);
        l.characteristic_number = None;
        l.characteristic_designator = None;
        l.inspection_method = None;
        l.sheet_zone = None;
        l.nominal_value = None;
        l.upper_tol = None;
        l.lower_tol = None;
        l.units = None;
        l.actual_value = Some(f64::NAN);
        l.deviation = Some(f64::INFINITY);
        l.characteristic_name = "Ő".repeat(200);
        assert!(render(&inputs(&r, std::slice::from_ref(&l))).is_ok());
        assert_eq!(num(Some(f64::NAN)), "INVALID");
        assert_eq!(num(Some(f64::INFINITY)), "INVALID");
    }

    /// `truncate` cuts on CHARACTER boundaries, so a multi-byte name
    /// cannot panic the renderer or emit a broken byte sequence.
    #[test]
    fn truncate_is_character_safe() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        let hu = "őőőőőőőő";
        let cut = truncate(hu, 4);
        assert_eq!(cut.chars().count(), 4);
    }

    /// Hungarian double-acute is WinAnsi-substituted, and an unmappable
    /// char becomes a visible `?` rather than being silently dropped.
    #[test]
    fn winansi_substitution_is_visible_not_silent() {
        assert_eq!(winansi_byte_for_char('ő'), 0xF6);
        assert_eq!(winansi_byte_for_char('Ű'), 0xDC);
        assert_eq!(winansi_byte_for_char('…'), 0x85);
        assert_eq!(winansi_byte_for_char('—'), 0x97);
        assert_eq!(winansi_byte_for_char('日'), b'?');
    }

    /// The AS9102 revision the FAIR claims is Rev C — Ervin's explicit
    /// confirmation (ADR-0199 §Open Q1). A silent drift to Rev B would
    /// make every FAIR cite a standard it was not built to.
    #[test]
    fn the_fair_claims_rev_c() {
        assert_eq!(AS9102_REVISION, "Rev C");
        let r = report(QcReportKind::As9102Fair, QcReportTemplate::As9102RevC);
        let bytes = render(&inputs(&r, &[line(1, Some("SN-001"), true)])).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("Rev C"));
    }
}
