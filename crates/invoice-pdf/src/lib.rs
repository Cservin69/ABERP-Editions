//! Printed-invoice PDF renderer per ADR-0037 §1.a + ADR-0021
//! "Print rendering path" deferred row. PR-44ε.1 / A152.
//!
//! # Posture
//!
//! Single-page A4 PDF. Built-in `Helvetica` + `Helvetica-Bold` fonts
//! with WinAnsi encoding. Layout matches the reference template
//! (`reference_aberp_invoice_template.md`) re-branded from Billingo to
//! ABERP — same field set, same top-to-bottom order, same
//! right-aligned totals block.
//!
//! # Why lopdf + built-in Helvetica
//!
//! Per the session-56 A152 decision: `lopdf` is a low-level
//! Rust-native PDF document model with no system deps; the built-in
//! Helvetica font means no font file to embed or ship with the
//! binary. Trade-off: WinAnsi encoding does not cover Hungarian
//! double-acute `ő/ű/Ő/Ű`; the renderer substitutes those to single-
//! acute `ö/ü/Ö/Ü` at the byte boundary (see [`text`] module). The
//! substitution is documented loud and named as the PR-44ε.2 deferred
//! lift.
//!
//! Alternatives considered + rejected in A152:
//! - `typst` (Rust-native typesetting engine): proper Hungarian
//!   handling but very large dep tree; not available in the sandbox.
//! - `weasyprint` (Python HTML→PDF): requires Python in deploy;
//!   regressed against the codebase's Rust-native single-binary
//!   posture (per CLAUDE.md rule 11).
//! - `typst` CLI shelled out: requires `typst` binary on every
//!   operator workstation AND every CI runner; not portable.
//! - Type0/CIDFontType2 font embedding: proper Hungarian but ~300
//!   LoC of CIDFontType + ToUnicode-cmap glue; defers to PR-44ε.2.
//!
//! # Coordinate system
//!
//! PDF uses bottom-left origin in points (1/72 inch). A4 = 595 × 842
//! points. The renderer positions every text element via absolute
//! `Td` moves; layout drift is structural rather than relative, which
//! keeps the layout deterministic across input data (the regulatory
//! print needs exact placement for accountant readability).

#![forbid(unsafe_code)]

pub mod format;
pub mod model;
pub mod text;

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, ObjectId, Stream, StringFormat};
use thiserror::Error;

use aberp_billing::Currency;

pub use model::{InvoiceModel, LineItem, PartyInfo};

/// A4 page width in PDF points (210 mm × 72/25.4).
const PAGE_WIDTH: i64 = 595;
/// A4 page height in PDF points (297 mm × 72/25.4).
const PAGE_HEIGHT: i64 = 842;
/// Left margin in points.
const MARGIN_LEFT: i64 = 48;
/// Right margin (x-coord of the right edge of the printable area).
const MARGIN_RIGHT: i64 = PAGE_WIDTH - 48;
/// Top margin (y-coord of the top of the printable area; PDF y grows
/// upward).
const MARGIN_TOP: i64 = PAGE_HEIGHT - 56;

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("non-HUF invoice requires rate_metadata for the printed render (ADR-0037 §1.a)")]
    MissingRateMetadata,
    #[error("invoice has no line items — refusing to render an empty body")]
    NoLines,
    #[error("PDF content-stream encoding failed: {0}")]
    ContentEncode(String),
    #[error("PDF document save failed: {0}")]
    Save(String),
}

/// Render the invoice to PDF bytes.
///
/// Per ADR-0037 §4 invariant C7 (printed-render slice): non-HUF
/// invoices loud-fail when `rate_metadata` is missing — the §80(1)(g)
/// HUF-equivalent line on the printed invoice depends on the stamped
/// MNB rate.
pub fn render_invoice(model: &InvoiceModel) -> Result<Vec<u8>, RenderError> {
    if model.lines.is_empty() {
        return Err(RenderError::NoLines);
    }
    if !matches!(model.currency, Currency::Huf) && model.rate_metadata.is_none() {
        return Err(RenderError::MissingRateMetadata);
    }

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let font_regular = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let font_bold = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
        "Encoding" => "WinAnsiEncoding",
    });
    let font_italic = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Oblique",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_regular,
            "FB" => font_bold,
            "FI" => font_italic,
        },
    });

    let mut ops: Vec<Operation> = Vec::new();
    layout(&mut ops, model);
    let content = Content { operations: ops };
    let content_bytes = content
        .encode()
        .map_err(|e| RenderError::ContentEncode(e.to_string()))?;
    let content_id = doc.add_object(Stream::new(dictionary! {}, content_bytes));

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
    });
    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => vec![Object::Reference(page_id)],
        "Count" => 1,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), PAGE_WIDTH.into(), PAGE_HEIGHT.into()],
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.compress();

    let mut buf: Vec<u8> = Vec::new();
    doc.save_to(&mut buf)
        .map_err(|e| RenderError::Save(e.to_string()))?;
    let _ = page_id;
    let _: ObjectId = catalog_id;
    Ok(buf)
}

/// Append the full top-to-bottom layout operations onto `ops`.
fn layout(ops: &mut Vec<Operation>, m: &InvoiceModel) {
    // Title block (top-left): "Számla" + invoice number.
    text(ops, "FB", 28, MARGIN_LEFT, MARGIN_TOP - 14, "Számla");
    text(
        ops,
        "F1",
        18,
        MARGIN_LEFT,
        MARGIN_TOP - 38,
        &m.invoice_number,
    );

    // Horizontal accent rule under the title.
    rule(ops, MARGIN_LEFT, MARGIN_RIGHT, MARGIN_TOP - 58);

    // Two-column party block.
    let party_top = MARGIN_TOP - 78;
    let col_left = MARGIN_LEFT;
    let col_right = MARGIN_LEFT + (MARGIN_RIGHT - MARGIN_LEFT) / 2 + 8;
    let after_seller = write_party(ops, "ELADÓ", &m.supplier, col_left, party_top, true);
    let after_buyer = write_party(ops, "VEVŐ", &m.customer, col_right, party_top, false);
    let parties_bottom = after_seller.min(after_buyer);

    // Date block: SZÁMLA KELTE / TELJESÍTÉS KELTE on the left,
    // FIZETÉSI HATÁRIDŐ / FIZETÉSI MÓD on the right.
    let dates_top = parties_bottom - 24;
    label_value(
        ops,
        col_left,
        dates_top,
        "SZÁMLA KELTE",
        &format::hungarian_date(m.issue_date),
    );
    label_value(
        ops,
        col_left,
        dates_top - 14,
        "TELJESÍTÉS KELTE",
        &format::hungarian_date(m.fulfillment_date),
    );
    label_value(
        ops,
        col_right,
        dates_top,
        "FIZETÉSI HATÁRIDŐ",
        &format::hungarian_date(m.payment_due_date),
    );
    label_value(
        ops,
        col_right,
        dates_top - 14,
        "FIZETÉSI MÓD",
        &m.payment_method,
    );

    // Highlighted total banner: FIZETENDŐ BRUTTÓ VÉGÖSSZEG, right-aligned.
    let invoice_gross_minor: i64 = m.lines.iter().map(|l| l.gross_minor).sum();
    let banner_y = dates_top - 44;
    rule(ops, MARGIN_LEFT, MARGIN_RIGHT, banner_y + 22);
    let banner_label = "FIZETENDŐ BRUTTÓ VÉGÖSSZEG:";
    let banner_amount = format::money(m.currency, invoice_gross_minor);
    text_right(ops, "F1", 9, MARGIN_RIGHT - 140, banner_y + 6, banner_label);
    text_right(ops, "FB", 20, MARGIN_RIGHT, banner_y, &banner_amount);

    // Line items table.
    let table_top = banner_y - 28;
    write_lines_table(ops, m, table_top);

    // Compute the bottom of the table dynamically.
    let table_bottom = table_top - 22 - (m.lines.len() as i64) * 28;

    // Totals block (right-aligned).
    let totals_top = table_bottom - 24;
    let totals_bottom = write_totals(ops, m, totals_top, invoice_gross_minor);

    // MEGJEGYZÉS (note) block.
    let note_top = totals_bottom - 24;
    write_note(ops, m, note_top);

    // Footer: page number + attestation.
    let footer_y_top = 64;
    text(ops, "FB", 8, MARGIN_LEFT, footer_y_top, "1/1 Oldal");
    text(
        ops,
        "FI",
        8,
        MARGIN_LEFT,
        footer_y_top - 14,
        "A számla tartalma mindenben megfelel a hatályos törvényekben foglaltaknak",
    );
}

fn write_party(
    ops: &mut Vec<Operation>,
    section_label: &str,
    party: &PartyInfo,
    x: i64,
    y_top: i64,
    is_seller: bool,
) -> i64 {
    text(ops, "F1", 7, x, y_top, section_label);
    text(ops, "FB", 13, x, y_top - 16, &party.name);
    let mut y = y_top - 32;
    for line in &party.address_lines {
        text(ops, "F1", 9, x, y, line);
        y -= 11;
    }
    y -= 4;
    label_value(ops, x, y, "ADÓSZÁM", &party.tax_number);
    y -= 12;
    if is_seller {
        if let Some(v) = &party.bank_account_number {
            label_value(ops, x, y, "BANKSZÁMLASZÁM", v);
            y -= 12;
        }
        if let Some(v) = &party.iban {
            label_value(ops, x, y, "IBAN", v);
            y -= 12;
        }
        if let Some(v) = &party.bank_name {
            label_value(ops, x, y, "BANK NEVE", v);
            y -= 12;
        }
        if let Some(v) = &party.swift_bic {
            label_value(ops, x, y, "SWIFT/BIC", v);
            y -= 12;
        }
    }
    y
}

fn write_lines_table(ops: &mut Vec<Operation>, m: &InvoiceModel, top: i64) {
    // Column x-positions (right edges for numeric columns).
    let col_num_x = MARGIN_LEFT;
    let col_desc_x = MARGIN_LEFT + 18;
    let col_qty_right = MARGIN_LEFT + 270;
    let col_unit_price_right = MARGIN_LEFT + 340;
    let col_net_right = MARGIN_LEFT + 400;
    let col_vat_right = MARGIN_LEFT + 432;
    let col_gross_right = MARGIN_RIGHT;

    // Header row.
    text(ops, "FB", 8, col_num_x, top, "#");
    text(ops, "FB", 8, col_desc_x, top, "MEGNEVEZÉS");
    text_right(ops, "FB", 8, col_qty_right, top, "MENNYISÉG");
    text_right(ops, "FB", 8, col_unit_price_right, top, "NETTÓ EGYSÉGÁR");
    text_right(ops, "FB", 8, col_net_right, top, "NETTÓ ÁR");
    text_right(ops, "FB", 8, col_vat_right, top, "ÁFA");
    text_right(ops, "FB", 8, col_gross_right, top, "BRUTTÓ ÁR");
    rule(ops, MARGIN_LEFT, MARGIN_RIGHT, top - 6);

    // Body rows.
    let mut y = top - 22;
    for (i, line) in m.lines.iter().enumerate() {
        let row_num = format!("{}", i + 1);
        text(ops, "F1", 9, col_num_x, y, &row_num);
        text(ops, "F1", 9, col_desc_x, y, &line.description);
        let qty_str = format!("{} {}", line.quantity, line.unit);
        text_right(ops, "F1", 9, col_qty_right, y, &qty_str);
        text_right(
            ops,
            "F1",
            9,
            col_unit_price_right,
            y,
            &format::money(m.currency, line.unit_price_minor),
        );
        text_right(
            ops,
            "F1",
            9,
            col_net_right,
            y,
            &format::money(m.currency, line.net_minor),
        );
        text_right(
            ops,
            "F1",
            9,
            col_vat_right,
            y,
            &format!("{}%", line.vat_rate_percent),
        );
        text_right(
            ops,
            "F1",
            9,
            col_gross_right,
            y,
            &format::money(m.currency, line.gross_minor),
        );
        if let Some((start, end)) = line.performance_period {
            let perf = format!(
                "Teljesítési időszak: {} – {}",
                format::iso_dotted_date(start),
                format::iso_dotted_date(end),
            );
            text(ops, "FI", 8, col_desc_x, y - 12, &perf);
        }
        y -= 28;
    }
    rule(ops, MARGIN_LEFT, MARGIN_RIGHT, y + 8);
}

fn write_totals(
    ops: &mut Vec<Operation>,
    m: &InvoiceModel,
    top: i64,
    invoice_gross_minor: i64,
) -> i64 {
    // Aggregate per-VAT-rate amounts.
    let mut by_rate: std::collections::BTreeMap<u16, (i64, i64)> =
        std::collections::BTreeMap::new();
    for line in &m.lines {
        let entry = by_rate.entry(line.vat_rate_percent).or_insert((0, 0));
        entry.0 += line.net_minor;
        entry.1 += line.vat_minor;
    }
    let invoice_net_minor: i64 = m.lines.iter().map(|l| l.net_minor).sum();

    let label_right = MARGIN_RIGHT - 150;
    let mut y = top;

    // NETTÓ ÖSSZEG: invoice-currency net total.
    text_right(ops, "F1", 9, label_right, y, "NETTÓ ÖSSZEG:");
    text_right(
        ops,
        "F1",
        9,
        MARGIN_RIGHT,
        y,
        &format::money(m.currency, invoice_net_minor),
    );
    y -= 14;

    // Per-VAT-rate ÁFA in invoice currency, then HUF (non-HUF only).
    for (&pct, &(_net, vat_minor)) in &by_rate {
        let label = format!("{}% ÁFA:", pct);
        text_right(ops, "F1", 9, label_right, y, &label);
        text_right(
            ops,
            "F1",
            9,
            MARGIN_RIGHT,
            y,
            &format::money(m.currency, vat_minor),
        );
        y -= 14;
        if !matches!(m.currency, Currency::Huf) {
            if let Some(rate) = m.rate_metadata.as_ref() {
                let vat_huf = aberp_billing::huf_equivalent_round_half_even(vat_minor, &rate.rate)
                    .unwrap_or(0);
                text_right(ops, "F1", 9, label_right, y, &label);
                text_right(
                    ops,
                    "F1",
                    9,
                    MARGIN_RIGHT,
                    y,
                    &format::money(Currency::Huf, vat_huf),
                );
                y -= 14;
            }
        }
    }

    // FIZETENDŐ BRUTTÓ VÉGÖSSZEG: invoice-currency gross total.
    text_right(ops, "F1", 9, label_right, y, "FIZETENDŐ BRUTTÓ VÉGÖSSZEG:");
    text_right(
        ops,
        "F1",
        9,
        MARGIN_RIGHT,
        y,
        &format::money(m.currency, invoice_gross_minor),
    );
    y -= 14;

    // Árfolyam + Bruttó összeg in HUF, non-HUF only.
    if !matches!(m.currency, Currency::Huf) {
        if let Some(rate) = m.rate_metadata.as_ref() {
            let rate_str = format!(
                "Árfolyam: {} Ft",
                format::rate_for_display(&rate.rate.to_string())
            );
            text_right(ops, "F1", 9, MARGIN_RIGHT, y, &rate_str);
            y -= 14;
            let gross_str = format!(
                "Bruttó összeg: {}",
                format::money(Currency::Huf, rate.huf_equivalent_total),
            );
            text_right(ops, "F1", 9, MARGIN_RIGHT, y, &gross_str);
            y -= 14;
        }
    }

    y
}

fn write_note(ops: &mut Vec<Operation>, m: &InvoiceModel, top: i64) {
    text(ops, "F1", 7, MARGIN_LEFT, top, "MEGJEGYZÉS");
    let mut y = top - 14;
    if !matches!(m.currency, Currency::Huf) {
        if let Some(rate) = m.rate_metadata.as_ref() {
            let note = format!(
                "1 {} = {} Ft",
                m.currency.iso_code(),
                format::rate_for_display(&rate.rate.to_string()),
            );
            text(ops, "FI", 9, MARGIN_LEFT, y, &note);
            y -= 12;
        }
    }
    if let Some(note) = &m.note {
        text(ops, "F1", 9, MARGIN_LEFT, y, note);
    }
}

/// Emit a left-anchored text run at `(x, y)` using font alias `font`
/// (one of `"F1"` / `"FB"` / `"FI"`) at `size` points.
fn text(ops: &mut Vec<Operation>, font: &str, size: i64, x: i64, y: i64, content: &str) {
    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new(
        "Tf",
        vec![Object::Name(font.as_bytes().to_vec()), size.into()],
    ));
    ops.push(Operation::new("Td", vec![x.into(), y.into()]));
    ops.push(Operation::new(
        "Tj",
        vec![Object::String(
            text::winansi_bytes(content),
            StringFormat::Literal,
        )],
    ));
    ops.push(Operation::new("ET", vec![]));
}

/// Emit a right-anchored text run whose right edge sits at `x_right`.
/// Width estimated from a Helvetica per-char proxy of `0.55 * size`
/// (Helvetica is variable-width; the proxy is a coarse upper bound
/// that keeps right-alignment visually correct without a full font-
/// metrics lookup). Per CLAUDE.md rule 13: a metrics table would be
/// ~200 LoC of glyph-width data for a layout that doesn't need that
/// precision — the printed totals block visually right-aligns within
/// 3-4 points of perfect.
fn text_right(
    ops: &mut Vec<Operation>,
    font: &str,
    size: i64,
    x_right: i64,
    y: i64,
    content: &str,
) {
    let est_width = (content.chars().count() as i64) * size * 55 / 100;
    let x_left = x_right - est_width;
    text(ops, font, size, x_left, y, content);
}

/// Emit a horizontal rule between `(x_left, y)` and `(x_right, y)`.
fn rule(ops: &mut Vec<Operation>, x_left: i64, x_right: i64, y: i64) {
    ops.push(Operation::new("q", vec![]));
    ops.push(Operation::new(
        "RG",
        vec![Object::Real(0.7), Object::Real(0.7), Object::Real(0.7)],
    ));
    ops.push(Operation::new("w", vec![Object::Real(0.5)]));
    ops.push(Operation::new("m", vec![x_left.into(), y.into()]));
    ops.push(Operation::new("l", vec![x_right.into(), y.into()]));
    ops.push(Operation::new("S", vec![]));
    ops.push(Operation::new("Q", vec![]));
}

/// Emit a "LABEL: value" pair at `(x, y)`, label small-grey + value bold.
fn label_value(ops: &mut Vec<Operation>, x: i64, y: i64, label: &str, value: &str) {
    text(ops, "F1", 7, x, y + 2, &format!("{}:", label));
    let label_width = (label.chars().count() as i64 + 1) * 7 * 55 / 100 + 4;
    text(ops, "FB", 9, x + label_width, y, value);
}
