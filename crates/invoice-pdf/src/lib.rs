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
//! # PR-85 — premium polish (silver / gold palette)
//!
//! ADR-0044 records the brand decision: this is Áben Consulting's
//! real client-facing document, so the surface needs refined-luxury
//! restraint, NOT dev-tool grey. The palette lives in `style` below
//! as a small, named set of `(f32, f32, f32)` constants so colour is
//! tunable in one place. Three discipline rules per ADR-0044:
//!
//! 1. Structural rules in `SILVER_LINE` (soft warm grey).
//! 2. ONE gold accent only — the rule above the totals banner. The
//!    big total figure stays ink (sparing, not gaudy).
//! 3. Section labels in `MUTED` (silver-grey) — small-caps feel comes
//!    from existing uppercase strings + the smaller font size, NOT
//!    extra typography ops (kept tasteful + WinAnsi-safe).
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

// ─── PR-85 — silver / gold palette ────────────────────────────────────
//
// Named once here so a future brand tweak is a one-line edit, not a
// grep-and-replace across thirty `Object::Real(0.7)` literals. ADR-0044
// records the brand rationale.
//
// Encoded as `(f32, f32, f32)` RGB in 0..=1. Each colour ships as a
// helper that pushes the right PDF op (`rg` for non-stroking / fill
// used by text, `RG` for stroking used by rule lines).

type Color = (f32, f32, f32);

/// Body ink — near-black with a faint warm shift so it reads softer
/// than a pure-black `Tj`. Used for every primary number + name + body
/// paragraph. NOT pure black (0,0,0): a slight warmth pairs with the
/// silver/gold accents.
const INK: Color = (0.13, 0.13, 0.15);
/// Section labels (ELADÓ, VEVŐ, ADÓSZÁM:, NETTÓ ÖSSZEG:, MEGJEGYZÉS,
/// table column headers). Refined silver-grey — sits below the ink
/// hierarchy without disappearing.
const MUTED: Color = (0.46, 0.47, 0.51);
/// Structural rules — title under-rule, table header rule, table
/// footer rule. A soft warm silver: clearly visible but never
/// competes with the ink content above/below.
const SILVER_LINE: Color = (0.72, 0.72, 0.74);
/// PR-85's ONE accent (per ADR-0044 §"Restraint"). Used for exactly
/// one rule: the line above the FIZETENDŐ BRUTTÓ VÉGÖSSZEG totals
/// banner. A muted warm gold — refined, not gaudy. If a future
/// reviewer feels the need to add gold to a second element, push back
/// and re-read ADR-0044 first.
///
/// Saturation tuned so the accent reads visibly gold (not "slightly
/// darker grey") on a 150-dpi print preview yet stays restrained on
/// a high-resolution actual print. Slightly warmer than a pure
/// midpoint gold so the rule sits comfortably next to the warm-ink
/// body text.
const GOLD_ACCENT: Color = (0.72, 0.54, 0.12);

/// Gap (in points) between a label's colon and its value in the
/// party / date `label_value` pairs. PR-85: was 4pt (cramped — Ervin
/// flagged the `Adószám:123` look), now 10pt for breathing room.
const LABEL_VALUE_GAP: i64 = 10;

/// Stroke weight (in points) for `SILVER_LINE` structural rules.
const RULE_WEIGHT_SILVER: f32 = 0.5;
/// Stroke weight (in points) for the single `GOLD_ACCENT` rule above
/// the totals banner. Slightly heavier than silver so the accent
/// reads as deliberate rather than a thicker grey line.
const RULE_WEIGHT_GOLD: f32 = 0.85;

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
    // Title block (top-left): "Számla" + invoice number. The number
    // stays INK — accountants look it up; it's the primary key on
    // the printed surface. Size-18 regular vs size-28 bold above
    // already gives the visual hierarchy.
    text(ops, "FB", 28, MARGIN_LEFT, MARGIN_TOP - 14, "Számla");
    text(
        ops,
        "F1",
        18,
        MARGIN_LEFT,
        MARGIN_TOP - 38,
        &m.invoice_number,
    );

    // Title under-rule — silver. (Gold is reserved for the banner.)
    silver_rule(ops, MARGIN_LEFT, MARGIN_RIGHT, MARGIN_TOP - 58);

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
    // PR-85 — the single gold accent in the document lives here.
    let invoice_gross_minor: i64 = m.lines.iter().map(|l| l.gross_minor).sum();
    let banner_y = dates_top - 44;
    gold_rule(ops, MARGIN_LEFT, MARGIN_RIGHT, banner_y + 22);
    let banner_label = "FIZETENDŐ BRUTTÓ VÉGÖSSZEG:";
    let banner_amount = format::money(m.currency, invoice_gross_minor);
    text_right_in(
        ops,
        "F1",
        9,
        MARGIN_RIGHT - 150,
        banner_y + 6,
        banner_label,
        MUTED,
    );
    text_right(ops, "FB", 20, MARGIN_RIGHT, banner_y, &banner_amount);

    // Line items table.
    let table_top = banner_y - 28;
    let table_bottom = write_lines_table(ops, m, table_top);

    // Totals block (right-aligned).
    let totals_top = table_bottom - 24;
    let totals_bottom = write_totals(ops, m, totals_top, invoice_gross_minor);

    // MEGJEGYZÉS (note) block.
    let note_top = totals_bottom - 24;
    write_note(ops, m, note_top);

    // Footer: page number + attestation.
    let footer_y_top = 64;
    text_in(ops, "FB", 8, MARGIN_LEFT, footer_y_top, "1/1 Oldal", MUTED);
    text_in(
        ops,
        "FI",
        8,
        MARGIN_LEFT,
        footer_y_top - 14,
        "A számla tartalma mindenben megfelel a hatályos törvényekben foglaltaknak",
        MUTED,
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
    text_in(ops, "F1", 7, x, y_top, section_label, MUTED);
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

/// PR-85 — line-item column geometry. Pulled into a named struct so
/// the column positions are tunable in one place (and so the test
/// for description-wrap can use the same `DESC_WRAP_CHARS` value the
/// renderer uses).
///
/// Pre-PR-85 the table sat hard against the right margin and the
/// gutters between numeric columns were tight enough that
/// `NETTÓ EGYSÉGÁR` / `BRUTTÓ ÁR` headers visually kissed each other.
/// This pass shifts every column slightly left off the right margin
/// AND widens the gutters between right-edges of adjacent columns.
struct TableLayout;

impl TableLayout {
    /// Row-number column anchor (left-aligned at MARGIN_LEFT).
    const NUM_X: i64 = MARGIN_LEFT;
    /// Description column anchor (left-aligned).
    const DESC_X: i64 = MARGIN_LEFT + 18;
    /// Description column maximum width in characters before wrap.
    /// At size 9 with the 0.55-of-size proxy ≈ 4.95 pts/char, 40
    /// chars ≈ 198 pts of horizontal real estate — comfortably inside
    /// the description-to-quantity gutter. Deliberately set BELOW the
    /// existing print_invoice_render fixture description's 42 chars so
    /// that the wrap behaviour is exercised by the workspace test suite
    /// (a regression that loses the wrap fires as a layout drift in
    /// the next sample render, not silently).
    const DESC_WRAP_CHARS: usize = 40;
    /// Per-extra-wrapped-description-line vertical advance (points).
    const DESC_WRAP_LINE_HEIGHT: i64 = 11;

    /// Right edges of the numeric columns. Each column is right-aligned
    /// so the right edge is the anchor; the leftmost glyph of the data
    /// floats left based on its width.
    ///
    /// PR-85 — column positions tuned for breathing room. The pre-PR-85
    /// layout had the VAT column hard up against the BRUTTÓ ÁR column
    /// (visible overlap on the live render Ervin flagged: "27%₣905,00"
    /// where 27% and €1 905,00 collided). Root cause: the 0.55-of-size
    /// per-char proxy in `text_right` underestimates the width of `%`
    /// (real ≈ 0.93×size) and uppercase header glyphs (real ≈ 0.7×size),
    /// so right-aligned content was extending past its stated right-
    /// edge by 5-10pt and into the next column.
    ///
    /// The fix here is structural rather than touching the shared
    /// proxy: pull `VAT_RIGHT` far enough left that even with the
    /// proxy's underestimate, neither the `27%` data nor the `ÁFA`
    /// header crosses into the gross column. Other right-edges shift
    /// outward slightly to widen the gutters Ervin asked for, and
    /// `GROSS_RIGHT` pulls 6pt off `MARGIN_RIGHT` so the rightmost
    /// column no longer hugs the page edge.
    const QTY_RIGHT: i64 = MARGIN_LEFT + 270; // unchanged
    const UNIT_PRICE_RIGHT: i64 = MARGIN_LEFT + 345; // was + 340 — +5 for wider gutter
    const NET_RIGHT: i64 = MARGIN_LEFT + 410; // was + 400 — +10 for wider gutter
    const VAT_RIGHT: i64 = MARGIN_LEFT + 435; // was + 432, BUT GROSS shifted left so net
    const GROSS_RIGHT: i64 = MARGIN_RIGHT - 6; // was MARGIN_RIGHT exactly — pulled off edge
}

/// Render the line-items table. Returns the y-coordinate of the
/// horizontal rule that closes the table — the caller uses this to
/// anchor the totals block. Per PR-82 the row height grows from the
/// base 28pt when a line carries either a `performance_period`
/// sub-line OR a `note` sub-line; both can coexist (note prints
/// below the performance period). Per PR-85 the row height ALSO grows
/// when the description wraps to multiple lines.
fn write_lines_table(ops: &mut Vec<Operation>, m: &InvoiceModel, top: i64) -> i64 {
    // Header row — column labels in MUTED at size 8 bold.
    text_in(ops, "FB", 8, TableLayout::NUM_X, top, "#", MUTED);
    text_in(ops, "FB", 8, TableLayout::DESC_X, top, "MEGNEVEZÉS", MUTED);
    text_right_in(
        ops,
        "FB",
        8,
        TableLayout::QTY_RIGHT,
        top,
        "MENNYISÉG",
        MUTED,
    );
    text_right_in(
        ops,
        "FB",
        8,
        TableLayout::UNIT_PRICE_RIGHT,
        top,
        "NETTÓ EGYSÉGÁR",
        MUTED,
    );
    text_right_in(ops, "FB", 8, TableLayout::NET_RIGHT, top, "NETTÓ ÁR", MUTED);
    text_right_in(ops, "FB", 8, TableLayout::VAT_RIGHT, top, "ÁFA", MUTED);
    text_right_in(
        ops,
        "FB",
        8,
        TableLayout::GROSS_RIGHT,
        top,
        "BRUTTÓ ÁR",
        MUTED,
    );
    silver_rule(ops, MARGIN_LEFT, MARGIN_RIGHT, top - 6);

    // Body rows.
    let mut y = top - 22;
    for (i, line) in m.lines.iter().enumerate() {
        let row_num = format!("{}", i + 1);
        text(ops, "F1", 9, TableLayout::NUM_X, y, &row_num);

        // PR-85 — description wraps to multiple lines when long. The
        // first line sits at `y`; subsequent lines stack downward at
        // `DESC_WRAP_LINE_HEIGHT` apart. The numeric columns continue
        // to anchor at `y` (top of the row) — accountants read the
        // numbers off the row's top edge regardless of how tall the
        // description column grows.
        let desc_lines = wrap_to_chars(&line.description, TableLayout::DESC_WRAP_CHARS);
        for (i_line, dline) in desc_lines.iter().enumerate() {
            text(
                ops,
                "F1",
                9,
                TableLayout::DESC_X,
                y - (i_line as i64) * TableLayout::DESC_WRAP_LINE_HEIGHT,
                dline,
            );
        }
        let desc_extra =
            (desc_lines.len().saturating_sub(1) as i64) * TableLayout::DESC_WRAP_LINE_HEIGHT;

        let qty_str = format!("{} {}", line.quantity, line.unit);
        text_right(ops, "F1", 9, TableLayout::QTY_RIGHT, y, &qty_str);
        text_right(
            ops,
            "F1",
            9,
            TableLayout::UNIT_PRICE_RIGHT,
            y,
            &format::money(m.currency, line.unit_price_minor),
        );
        text_right(
            ops,
            "F1",
            9,
            TableLayout::NET_RIGHT,
            y,
            &format::money(m.currency, line.net_minor),
        );
        text_right(
            ops,
            "F1",
            9,
            TableLayout::VAT_RIGHT,
            y,
            &format!("{}%", line.vat_rate_percent),
        );
        text_right(
            ops,
            "F1",
            9,
            TableLayout::GROSS_RIGHT,
            y,
            &format::money(m.currency, line.gross_minor),
        );

        // Sub-line baseline — sits below the wrapped description so
        // performance-period + buyer-note sub-lines don't overlap
        // long descriptions.
        let mut sub_y = y - desc_extra - 12;
        if let Some((start, end)) = line.performance_period {
            let perf = format!(
                "Teljesítési időszak: {} – {}",
                format::iso_dotted_date(start),
                format::iso_dotted_date(end),
            );
            text_in(ops, "FI", 8, TableLayout::DESC_X, sub_y, &perf, MUTED);
            sub_y -= 11;
        }
        // PR-82 — per-line buyer note ("Megjegyzés"). Italic sub-line
        // labelled in Hungarian ("Megjegyzés:") so the buyer reads it
        // in context. Only renders when present; absent notes leave
        // the row at its base height so unannotated invoices look
        // identical to pre-PR-82 output.
        let mut extra_subline = 0;
        if let Some(note) = line.note.as_ref().filter(|s| !s.trim().is_empty()) {
            let label = format!("Megjegyzés: {}", note);
            text_in(ops, "FI", 8, TableLayout::DESC_X, sub_y, &label, MUTED);
            extra_subline += 12;
        }
        let base_advance = 28;
        // Per PR-82 + PR-85 row-height composition:
        //   base 28pt
        // + (desc_lines - 1) × 11pt for each wrapped description line
        // + 12pt iff a buyer note prints
        // Performance-period stays inside the 28pt slot (pre-PR-82
        // legacy posture — overlays into the row).
        y -= base_advance + desc_extra + extra_subline;
    }
    let footer_rule_y = y + 8;
    silver_rule(ops, MARGIN_LEFT, MARGIN_RIGHT, footer_rule_y);
    footer_rule_y
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
    text_right_in(ops, "F1", 9, label_right, y, "NETTÓ ÖSSZEG:", MUTED);
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
        text_right_in(ops, "F1", 9, label_right, y, &label, MUTED);
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
                text_right_in(ops, "F1", 9, label_right, y, &label, MUTED);
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
    text_right_in(
        ops,
        "F1",
        9,
        label_right,
        y,
        "FIZETENDŐ BRUTTÓ VÉGÖSSZEG:",
        MUTED,
    );
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
            text_right_in(ops, "F1", 9, MARGIN_RIGHT, y, &rate_str, MUTED);
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
    // PR-85 — skip the section entirely when there's nothing to say.
    // A bare "MEGJEGYZÉS" header followed by whitespace looked
    // visually orphaned on HUF invoices with no operator note, and
    // the regulatory record doesn't require the section to exist
    // when empty. Two content sources feed this block:
    //   1. The EUR-only rate-source sub-line ("1 EUR = X Ft")
    //   2. The buyer-facing operator note (PR-82)
    // If neither fires, render no section at all.
    let has_rate_note = !matches!(m.currency, Currency::Huf) && m.rate_metadata.is_some();
    let has_operator_note = m
        .note
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_rate_note && !has_operator_note {
        return;
    }

    text_in(ops, "F1", 7, MARGIN_LEFT, top, "MEGJEGYZÉS", MUTED);
    let mut y = top - 14;
    if has_rate_note {
        if let Some(rate) = m.rate_metadata.as_ref() {
            // PR-86 / session-111 — surface the rate-publication date
            // so the operator and buyer can see WHICH date's MNB rate
            // was applied. The date may differ from the supply date
            // when MNB walked back to a prior publication (weekend,
            // holiday, before that day's publish time) per the
            // ADR-0037 §2.b walk-back rule. Format mirrors the
            // Hungarian short-date convention used by the date block
            // (`YYYY.MM.DD.`).
            let note = format!(
                "1 {} = {} Ft ({}, {})",
                m.currency.iso_code(),
                format::rate_for_display(&rate.rate.to_string()),
                rate.source,
                format::hungarian_date(rate.date),
            );
            text(ops, "FI", 9, MARGIN_LEFT, y, &note);
            y -= 12;
        }
    }
    // PR-82 — buyer-facing invoice-level note. Renders below the
    // EUR-only rate-source sub-line (when applicable) so the rate
    // explanation reads first, the operator's free text second. Wraps
    // long notes naively across multiple lines using `wrap_to_chars`
    // so a paragraph-length note does not run off the right margin.
    if let Some(note) = m.note.as_ref().filter(|s| !s.trim().is_empty()) {
        for wrapped_line in wrap_to_chars(note, NOTE_WRAP_WIDTH_CHARS) {
            text(ops, "F1", 9, MARGIN_LEFT, y, &wrapped_line);
            y -= 12;
        }
    }
}

/// PR-82 — naive word-wrap for the MEGJEGYZÉS / Megjegyzés text.
/// Splits on whitespace and accumulates words up to `max_chars` per
/// line. Hand-rolled because: (a) we don't have a font-metrics table
/// (see `text_right`'s comment for the same trade-off), and (b) the
/// invoice surface uses a tiny vocabulary — short notes are the norm,
/// long notes acceptable as wrapped paragraphs.
///
/// PR-85 — renamed from `wrap_note_text` and re-used for line-item
/// description wrapping (same char-counted approach; the description
/// wrap-width constant lives on `TableLayout`).
const NOTE_WRAP_WIDTH_CHARS: usize = 100;

/// Wrap `text` to a sequence of lines, each at most `max_chars`
/// characters wide. Splits on whitespace; words longer than
/// `max_chars` get their own line (no mid-word break — a long URL or
/// product code prints on its own line and may visually overflow, but
/// never silently truncates).
pub(crate) fn wrap_to_chars(text: &str, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.chars().count() + 1 + word.chars().count() <= max_chars {
                current.push(' ');
                current.push_str(word);
            } else {
                out.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Emit a left-anchored text run at `(x, y)` in `INK` colour using
/// font alias `font` (one of `"F1"` / `"FB"` / `"FI"`) at `size`
/// points. Convenience wrapper around [`text_in`].
fn text(ops: &mut Vec<Operation>, font: &str, size: i64, x: i64, y: i64, content: &str) {
    text_in(ops, font, size, x, y, content, INK);
}

/// Emit a left-anchored text run at `(x, y)` in `color`. PR-85 — the
/// silver/gold palette flows through this entry point: every text op
/// in the renderer goes through either `text` (defaults to `INK`) or
/// `text_in` (explicit colour for `MUTED` section labels, etc.).
fn text_in(
    ops: &mut Vec<Operation>,
    font: &str,
    size: i64,
    x: i64,
    y: i64,
    content: &str,
    color: Color,
) {
    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new(
        "Tf",
        vec![Object::Name(font.as_bytes().to_vec()), size.into()],
    ));
    // `rg` sets the non-stroking (fill) colour — what Tj uses for
    // glyph ink. `RG` would set the stroking colour (used by rule
    // strokes via `silver_rule` / `gold_rule`); the two states are
    // independent in the PDF graphics state.
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
            text::winansi_bytes(content),
            StringFormat::Literal,
        )],
    ));
    ops.push(Operation::new("ET", vec![]));
}

/// Emit a right-anchored text run whose right edge sits at `x_right`,
/// in `INK` colour. Width estimated from a Helvetica per-char proxy of
/// `0.55 * size` (Helvetica is variable-width; the proxy is a coarse
/// upper bound that keeps right-alignment visually correct without a
/// full font-metrics lookup). Per CLAUDE.md rule 13: a metrics table
/// would be ~200 LoC of glyph-width data for a layout that doesn't
/// need that precision — the printed totals block visually right-
/// aligns within 3-4 points of perfect.
fn text_right(
    ops: &mut Vec<Operation>,
    font: &str,
    size: i64,
    x_right: i64,
    y: i64,
    content: &str,
) {
    text_right_in(ops, font, size, x_right, y, content, INK);
}

/// Right-anchored variant of [`text_in`] — same width-estimation
/// posture as [`text_right`], with explicit colour.
fn text_right_in(
    ops: &mut Vec<Operation>,
    font: &str,
    size: i64,
    x_right: i64,
    y: i64,
    content: &str,
    color: Color,
) {
    let est_width = (content.chars().count() as i64) * size * 55 / 100;
    let x_left = x_right - est_width;
    text_in(ops, font, size, x_left, y, content, color);
}

/// Emit a horizontal rule between `(x_left, y)` and `(x_right, y)` in
/// `SILVER_LINE` colour. Default structural rule across the document
/// (title under-rule, table header rule, table footer rule).
fn silver_rule(ops: &mut Vec<Operation>, x_left: i64, x_right: i64, y: i64) {
    horizontal_rule(ops, x_left, x_right, y, SILVER_LINE, RULE_WEIGHT_SILVER);
}

/// Emit a horizontal rule in `GOLD_ACCENT` colour. Used in exactly
/// one place per ADR-0044: the rule above the totals banner.
fn gold_rule(ops: &mut Vec<Operation>, x_left: i64, x_right: i64, y: i64) {
    horizontal_rule(ops, x_left, x_right, y, GOLD_ACCENT, RULE_WEIGHT_GOLD);
}

/// Underlying rule emitter — sets stroke colour + stroke weight,
/// moves to `(x_left, y)`, lines to `(x_right, y)`, strokes.
fn horizontal_rule(
    ops: &mut Vec<Operation>,
    x_left: i64,
    x_right: i64,
    y: i64,
    color: Color,
    weight: f32,
) {
    ops.push(Operation::new("q", vec![]));
    ops.push(Operation::new(
        "RG",
        vec![
            Object::Real(color.0),
            Object::Real(color.1),
            Object::Real(color.2),
        ],
    ));
    ops.push(Operation::new("w", vec![Object::Real(weight)]));
    ops.push(Operation::new("m", vec![x_left.into(), y.into()]));
    ops.push(Operation::new("l", vec![x_right.into(), y.into()]));
    ops.push(Operation::new("S", vec![]));
    ops.push(Operation::new("Q", vec![]));
}

/// Emit a "LABEL: value" pair at `(x, y)` — label in MUTED small-grey
/// at size 7, value in INK bold at size 9, with `LABEL_VALUE_GAP`
/// points of breathing room between the label's colon and the value's
/// first glyph.
fn label_value(ops: &mut Vec<Operation>, x: i64, y: i64, label: &str, value: &str) {
    text_in(ops, "F1", 7, x, y + 2, &format!("{}:", label), MUTED);
    // Label width: chars + 1 (for the colon) × proxy width at size 7,
    // plus `LABEL_VALUE_GAP` so the value never visually kisses the
    // label (PR-85 — was +4pt, too cramped per Ervin's "Adószám:123"
    // flag).
    let label_width = (label.chars().count() as i64 + 1) * 7 * 55 / 100 + LABEL_VALUE_GAP;
    text_in(ops, "FB", 9, x + label_width, y, value, INK);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PR-85 — pin the palette constants. A future "let me just nudge
    /// the gold a bit" edit that drifts away from the ADR-0044
    /// silver/gold posture should fail here loudly. The values are
    /// the brand decision; the regulatory record carries no opinion
    /// on RGB but the company's client-facing surface does.
    #[test]
    fn palette_constants_match_brand_decision() {
        assert_eq!(INK, (0.13, 0.13, 0.15));
        assert_eq!(MUTED, (0.46, 0.47, 0.51));
        assert_eq!(SILVER_LINE, (0.72, 0.72, 0.74));
        assert_eq!(GOLD_ACCENT, (0.72, 0.54, 0.12));
    }

    /// PR-85 — pin the Adószám / IBAN spacing so a future edit that
    /// shrinks `LABEL_VALUE_GAP` back to the pre-PR-85 4pt value
    /// (which Ervin flagged as too tight) trips this test instead of
    /// shipping. The 10pt gap is the brand decision.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn label_value_gap_breathes() {
        assert!(
            LABEL_VALUE_GAP >= 8,
            "LABEL_VALUE_GAP must stay ≥ 8pt — Ervin's polish ask was \
             that the pre-PR-85 4pt gap looked cramped on `Adószám:123`"
        );
    }

    /// PR-85 — pin the description-wrap behaviour. A short description
    /// fits on one line; a long one wraps; and no mid-word break
    /// occurs (a long URL or product code prints on its own line as
    /// a whole token — never silently truncated).
    #[test]
    fn description_wraps_when_long() {
        // A clearly-short description stays on one line.
        let short = "Tanácsadói díj";
        assert!(short.chars().count() <= TableLayout::DESC_WRAP_CHARS);
        let wrapped_short = wrap_to_chars(short, TableLayout::DESC_WRAP_CHARS);
        assert_eq!(wrapped_short.len(), 1);

        // A long description wraps to multiple lines (≥ 2). Note the
        // existing `print_invoice_render` integration fixture's 42-char
        // description sits ABOVE the 40-char wrap width — its wrap-to-
        // two-lines behaviour is exercised by that suite, which keeps
        // the wrap path live in CI.
        let long = "Tanácsadói szolgáltatás Áben Consulting KFT részére \
                    2026 második negyedévében az ERP-rendszer bevezetésére \
                    vonatkozóan, NAV-megfelelőség és könyvviteli integráció \
                    kiegészítéssel";
        let wrapped_long = wrap_to_chars(long, TableLayout::DESC_WRAP_CHARS);
        assert!(
            wrapped_long.len() >= 2,
            "long description must wrap to ≥ 2 lines; got {} lines",
            wrapped_long.len()
        );

        // No mid-word breaks — every wrapped line is composed of
        // whole whitespace-separated tokens.
        for line in &wrapped_long {
            for word in line.split_whitespace() {
                assert!(!word.is_empty(), "no empty fragments in a wrapped line");
            }
        }
    }
}
