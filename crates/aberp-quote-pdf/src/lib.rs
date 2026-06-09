//! S279 / PR-265 — `aberp-quote-pdf`, the indicative-quote PDF
//! renderer for the auto-quoting pipeline.
//!
//! ## What this crate is
//!
//! A single pure function: [`render`]. Given a [`QuoteInputs`]
//! (FeatureGraph + QuoteBreakdown + customer info + quote id +
//! valid_until + extractor/engine version stamps), it returns the
//! PDF bytes.
//!
//! ## Invariants
//!
//! Pure per design doc §13 — no clock, no I/O, no RNG, no async. Same
//! inputs ⇒ byte-identical output. Lets a property test prove the
//! renderer never panics on the engine's output and lets the priced-
//! writeback's idempotency key (`feature_graph_hash`) prove the
//! storefront-side PDF on disk matches.
//!
//! ## Layout (storefront design doc §13)
//!
//! Single-page A4. Header band with ABERP wordmark + "Indicative
//! Quote". Customer block. Part summary (material grade, qty, bbox,
//! volume). Price-breakdown table (material / labor / setup /
//! overhead / margin / TOTAL). Top-5 reasoning_log lines.
//! Addendum-1 visibility band: "Routing: 5-axis machine" if
//! `requires_5_axis`, "Thin walls present (tight-tolerance surcharge
//! applied)" if `thin_wall_present && target_tolerance>=Tight`. Footer
//! with valid_until + extractor/engine versions + EUR-only money.
//!
//! ## Pushback against the brief
//!
//! The brief offered "build on invoice-pdf if it generalizes,
//! otherwise sibling crate." `aberp-invoice-pdf` is structured around
//! NAV §169/§172 invariants (party blocks, VAT rate breakdown,
//! HUF/EUR rate-stamping, per-line tax columns) that have no
//! correspondence in an indicative quote. A sibling crate is the
//! honest call — no shared helper module today; the day a second
//! caller needs Helvetica-WinAnsi byte tables, a `pdf-style-helpers`
//! crate is the right factor-out.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};
use thiserror::Error;

pub use aberp_quote_engine::{FeatureGraph, QuoteBreakdown, ToleranceRange};

/// Crate version stamp — used in the PDF footer.
pub const QUOTE_PDF_RENDERER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// EVE addendum 2 — Hungarian stock-status banner line (primary, the
/// customer market is HU). Mirrors the storefront customer HTML banner
/// at `q/[id]/+page.svelte` so the PDF and the web view say the same
/// thing. Double-acute `ő` is WinAnsi-substituted by [`winansi_bytes`].
pub const STOCK_ALERT_BANNER_HU: &str =
    "Készletállapot megváltozott — a DEAL frissíti az árképzést";
/// EVE addendum 2 — English stock-status banner line (secondary), the
/// exact wording the S318 brief pins.
pub const STOCK_ALERT_BANNER_EN: &str =
    "Stock status changed since this quote was issued — DEAL will refresh pricing";

/// A4 page width in PDF points.
const PAGE_WIDTH: i64 = 595;
/// A4 page height in PDF points.
const PAGE_HEIGHT: i64 = 842;
const MARGIN_LEFT: i64 = 56;
const MARGIN_RIGHT: i64 = PAGE_WIDTH - 56;
const MARGIN_TOP: i64 = PAGE_HEIGHT - 56;

/// What the renderer needs to produce one indicative quote PDF.
#[derive(Debug, Clone)]
pub struct QuoteInputs<'a> {
    /// Storefront-side UUID. Surfaces in the header so the customer
    /// can quote it back to the operator.
    pub quote_id: &'a str,
    /// Customer email (always set on a storefront submission).
    pub customer_email: &'a str,
    /// Customer name (always set).
    pub customer_name: &'a str,
    /// Optional company string from the submission form. Empty string
    /// means "no company"; the renderer omits the row.
    pub customer_company: &'a str,
    /// Optional quantity from the submission. Defaults to 1 if absent.
    pub quantity: u32,
    /// Optional customer-typed notes from the submission. Truncated
    /// to the first 400 chars on render.
    pub notes: &'a str,
    /// `YYYY-MM-DD` indicative-quote expiry per ADR-0004.
    pub valid_until_iso: &'a str,
    /// Stamp on the PDF footer + `Authorization`-side cross-walk.
    pub extractor_version: &'a str,
    /// Same posture as `extractor_version`.
    pub engine_version: &'a str,
    /// FeatureGraph the engine consumed. The renderer reads
    /// `bounding_box_mm`, `volume_mm3`, `material_grade`,
    /// `requires_5_axis`, `thin_wall_present` for the customer-visible
    /// surfaces; addendum-1 visibility is driven from these flags.
    pub feature_graph: &'a FeatureGraph,
    /// What the engine returned. Every monetary line is rendered.
    /// Top 5 `reasoning_log` lines surface under "How we priced this."
    pub breakdown: &'a QuoteBreakdown,
    /// What the engine was called with. Required for addendum-1's
    /// "tight-tolerance surcharge applied" line: that line only shows
    /// when `thin_wall_present && target_tolerance >= Tight`.
    pub target_tolerance: ToleranceRange,
    /// EVE addendum 2 (customer-facing half). When `true`, the renderer
    /// draws a red stock-status band at the top of the page (see
    /// [`STOCK_ALERT_BANNER_HU`] / [`STOCK_ALERT_BANNER_EN`]). Defaults
    /// to `false`; the sticky downgrade that flips it is detected in the
    /// `quote_intake_log` subsystem (`recompute_stock_alert`), NOT here.
    ///
    /// The banner string is baked into this crate as a literal — the
    /// same posture as the addendum-1 "Routing: 5-axis machine" lines —
    /// rather than caller-provided, because the PDF crate carries no
    /// i18n parameterisation and the customer market is single-locale.
    pub stock_alert: bool,
}

/// Failure taxonomy for [`render`]. The PDF emit is straight-line
/// `lopdf` ops with no fallible I/O; the only realistic failure is
/// `lopdf` itself rejecting the document on save (would mean a bug
/// in this crate, not bad input).
#[derive(Debug, Error)]
pub enum QuotePdfError {
    /// `lopdf` rejected the document on `save_to`. Indicates a bug in
    /// this crate; surfaced loud per CLAUDE.md rule 12.
    #[error("lopdf save failed: {0}")]
    LopdfSave(String),
}

/// Render an indicative-quote PDF. Pure: no clock, no I/O, no RNG.
///
/// The output bytes are ready for the storefront's priced-writeback
/// `pdf` multipart field per ADR-0004. Size is typically ≤ 50 KB for
/// a quote with ~20 reasoning lines.
pub fn render(inputs: &QuoteInputs<'_>) -> Result<Vec<u8>, QuotePdfError> {
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

    let ops = build_content(inputs);

    let content = Content { operations: ops };
    let content_stream = Stream::new(
        dictionary! {},
        content
            .encode()
            .map_err(|e| QuotePdfError::LopdfSave(format!("encode content: {e}")))?,
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

    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => vec![Object::Reference(page_id)],
        "Count" => 1,
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let mut out = Vec::with_capacity(8192);
    doc.save_to(&mut out)
        .map_err(|e| QuotePdfError::LopdfSave(e.to_string()))?;
    Ok(out)
}

/// Walk the page top-to-bottom building the content stream.
fn build_content(inputs: &QuoteInputs<'_>) -> Vec<Operation> {
    let mut ops: Vec<Operation> = Vec::with_capacity(128);
    let mut y = MARGIN_TOP;

    // ── Header band ────────────────────────────────────────────────
    push_text(
        &mut ops,
        MARGIN_LEFT,
        y,
        "F2",
        22,
        "ABERP — Indicative Quote",
    );
    y -= 22;
    push_text(
        &mut ops,
        MARGIN_LEFT,
        y,
        "F1",
        10,
        &format!("Quote ref: {}", inputs.quote_id),
    );
    y -= 14;
    push_text(
        &mut ops,
        MARGIN_LEFT,
        y,
        "F1",
        10,
        &format!("Valid until: {}", inputs.valid_until_iso),
    );
    y -= 24;
    push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y);
    y -= 18;

    // ── Stock-alert band (EVE addendum 2, customer-facing) ─────────
    //
    // Drawn at the TOP of the page (before the customer/pricing blocks)
    // so a stock-status downgrade is the first thing the customer sees.
    // Red rules above + below with bold red text match the addendum-1
    // band's visual weight using the existing primitives. Bilingual
    // (HU primary / EN secondary) to mirror the storefront HTML banner.
    if inputs.stock_alert {
        push_rule_red(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y + 6);
        y -= 4;
        push_text_red(&mut ops, MARGIN_LEFT, y, "F2", 11, STOCK_ALERT_BANNER_HU);
        y -= 14;
        push_text_red(&mut ops, MARGIN_LEFT, y, "F1", 10, STOCK_ALERT_BANNER_EN);
        y -= 10;
        push_rule_red(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y);
        y -= 18;
    }

    // ── Customer block ─────────────────────────────────────────────
    push_text(&mut ops, MARGIN_LEFT, y, "F2", 12, "CUSTOMER");
    y -= 16;
    push_text(&mut ops, MARGIN_LEFT, y, "F1", 11, inputs.customer_name);
    y -= 14;
    if !inputs.customer_company.is_empty() {
        push_text(&mut ops, MARGIN_LEFT, y, "F1", 11, inputs.customer_company);
        y -= 14;
    }
    push_text(&mut ops, MARGIN_LEFT, y, "F1", 11, inputs.customer_email);
    y -= 24;

    // ── Part summary ───────────────────────────────────────────────
    push_text(&mut ops, MARGIN_LEFT, y, "F2", 12, "PART SUMMARY");
    y -= 16;
    let fg = inputs.feature_graph;
    push_kv(
        &mut ops,
        MARGIN_LEFT,
        y,
        "Material grade:",
        &fg.material_grade,
    );
    y -= 14;
    push_kv(
        &mut ops,
        MARGIN_LEFT,
        y,
        "Quantity:",
        &format!("{} pcs", inputs.quantity),
    );
    y -= 14;
    push_kv(
        &mut ops,
        MARGIN_LEFT,
        y,
        "Bounding box (mm):",
        &format!(
            "{:.1} x {:.1} x {:.1}",
            fg.bounding_box_mm[0], fg.bounding_box_mm[1], fg.bounding_box_mm[2]
        ),
    );
    y -= 14;
    push_kv(
        &mut ops,
        MARGIN_LEFT,
        y,
        "Volume (cm3):",
        &format!("{:.2}", fg.volume_mm3 / 1000.0),
    );
    y -= 14;
    push_kv(
        &mut ops,
        MARGIN_LEFT,
        y,
        "Features extracted:",
        &fg.features.len().to_string(),
    );
    y -= 24;

    // ── Addendum-1 visibility band ─────────────────────────────────
    //
    // Per EVE-addenda 1 + storefront design §13, these two lines are
    // the customer-visible surface of the FeatureGraph booleans the
    // S269 extractor populates. They appear here unconditionally on
    // their "true" condition; absence is silence (no "Routing: 3-axis"
    // affirmative — uninformative for the customer).
    let mut addendum_shown = false;
    if fg.requires_5_axis {
        push_text(
            &mut ops,
            MARGIN_LEFT,
            y,
            "F2",
            11,
            "Routing: 5-axis machine",
        );
        y -= 14;
        addendum_shown = true;
    }
    if fg.thin_wall_present && inputs.target_tolerance >= ToleranceRange::Tight {
        push_text(
            &mut ops,
            MARGIN_LEFT,
            y,
            "F2",
            11,
            "Thin walls present (tight-tolerance surcharge applied)",
        );
        y -= 14;
        addendum_shown = true;
    }
    if addendum_shown {
        y -= 10;
    }

    // ── Price-breakdown table ──────────────────────────────────────
    push_text(&mut ops, MARGIN_LEFT, y, "F2", 12, "PRICE BREAKDOWN");
    y -= 16;
    let b = inputs.breakdown;
    push_money_row(&mut ops, y, "Material", b.material_cost);
    y -= 14;
    push_money_row(&mut ops, y, "Labor", b.labor_cost);
    y -= 14;
    push_money_row(&mut ops, y, "Setup", b.setup_cost);
    y -= 14;
    push_money_row(&mut ops, y, "Overhead", b.overhead);
    y -= 14;
    push_money_row(&mut ops, y, "Margin", b.margin);
    y -= 16;
    push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y + 6);
    push_money_row_bold(&mut ops, y, "TOTAL (EUR)", b.total_price);
    y -= 28;

    // ── Reasoning log (top 5) ──────────────────────────────────────
    push_text(&mut ops, MARGIN_LEFT, y, "F2", 12, "HOW WE PRICED THIS");
    y -= 16;
    let show_n = b.reasoning_log.len().min(5);
    for line in b.reasoning_log.iter().take(show_n) {
        let truncated = truncate(line, 110);
        push_text(&mut ops, MARGIN_LEFT, y, "F1", 9, &truncated);
        y -= 12;
    }
    if b.reasoning_log.len() > 5 {
        push_text(
            &mut ops,
            MARGIN_LEFT,
            y,
            "F1",
            9,
            &format!(
                "... {} further line(s) on the operator's breakdown",
                b.reasoning_log.len() - 5
            ),
        );
        y -= 12;
    }
    y -= 10;

    // ── Customer notes (if any) ────────────────────────────────────
    if !inputs.notes.is_empty() {
        push_text(&mut ops, MARGIN_LEFT, y, "F2", 12, "YOUR NOTES");
        y -= 16;
        for chunk in wrap_chunks(inputs.notes, 96) {
            push_text(&mut ops, MARGIN_LEFT, y, "F1", 9, &chunk);
            y -= 12;
            if y < 120 {
                break;
            }
        }
    }

    // ── Footer ─────────────────────────────────────────────────────
    let footer_y = 64;
    push_rule(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, footer_y + 24);
    push_text(
        &mut ops,
        MARGIN_LEFT,
        footer_y + 8,
        "F1",
        8,
        &format!(
            "Indicative quote — prices in EUR. Engine: {} | Extractor: {} | Renderer: {}",
            inputs.engine_version, inputs.extractor_version, QUOTE_PDF_RENDERER_VERSION
        ),
    );
    push_text(
        &mut ops,
        MARGIN_LEFT,
        footer_y - 4,
        "F1",
        8,
        "This is a non-binding indicative quote. Final pricing confirmed on order acceptance.",
    );

    ops
}

/// Push a single text run at `(x, y)` with font `font_key` (`F1` or
/// `F2`) at `size`.
fn push_text(ops: &mut Vec<Operation>, x: i64, y: i64, font_key: &str, size: i64, s: &str) {
    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new(
        "Tf",
        vec![Object::Name(font_key.as_bytes().to_vec()), size.into()],
    ));
    ops.push(Operation::new("Td", vec![x.into(), y.into()]));
    let bytes = winansi_bytes(s);
    ops.push(Operation::new(
        "Tj",
        vec![Object::String(bytes, lopdf::StringFormat::Literal)],
    ));
    ops.push(Operation::new("ET", vec![]));
}

/// Red-fill text run — same shape as [`push_text`] but sets the
/// nonstroking colour to the danger red before the text object and
/// resets to black after, so later runs stay black. The crate had no
/// prior fill-colour token (rules use a stroking grey); this danger red
/// (`0.75, 0.0, 0.0`) is introduced for the addendum-2 stock band and
/// matches the spirit of the storefront's `--color-danger` banner.
fn push_text_red(ops: &mut Vec<Operation>, x: i64, y: i64, font_key: &str, size: i64, s: &str) {
    ops.push(Operation::new(
        "rg",
        vec![0.75.into(), 0.0.into(), 0.0.into()],
    ));
    push_text(ops, x, y, font_key, size, s);
    ops.push(Operation::new(
        "rg",
        vec![0.0.into(), 0.0.into(), 0.0.into()],
    ));
}

/// Horizontal rule in the danger red, used to bracket the stock-alert
/// band. Mirrors [`push_rule`] but with a 1pt red stroke for weight.
fn push_rule_red(ops: &mut Vec<Operation>, x0: i64, x1: i64, y: i64) {
    ops.push(Operation::new(
        "RG",
        vec![0.75.into(), 0.0.into(), 0.0.into()],
    ));
    ops.push(Operation::new("w", vec![1.0.into()]));
    ops.push(Operation::new("m", vec![x0.into(), y.into()]));
    ops.push(Operation::new("l", vec![x1.into(), y.into()]));
    ops.push(Operation::new("S", vec![]));
}

/// Key-value row: left-aligned label + value indented to a fixed
/// column. Keeps the part-summary block aligned for skimmability.
fn push_kv(ops: &mut Vec<Operation>, x: i64, y: i64, label: &str, value: &str) {
    push_text(ops, x, y, "F1", 10, label);
    push_text(ops, x + 160, y, "F1", 10, value);
}

/// Money row — label on the left, value right-aligned at the right
/// margin. EUR formatting matches the locale ABERP runs in
/// (`<amount> EUR`, period-decimal).
fn push_money_row(ops: &mut Vec<Operation>, y: i64, label: &str, amount: f64) {
    push_text(ops, MARGIN_LEFT, y, "F1", 10, label);
    let s = format!("{:.2} EUR", amount);
    push_text_right(ops, MARGIN_RIGHT, y, "F1", 10, &s);
}

/// Bold money row — same shape as `push_money_row` but bold (F2),
/// used for the TOTAL line under the rule.
fn push_money_row_bold(ops: &mut Vec<Operation>, y: i64, label: &str, amount: f64) {
    push_text(ops, MARGIN_LEFT, y, "F2", 12, label);
    let s = format!("{:.2} EUR", amount);
    push_text_right(ops, MARGIN_RIGHT, y, "F2", 12, &s);
}

/// Approximate right-align: shift left by `≈ 0.55 * size * len`
/// chars before placing. Good enough for the totals column at v1
/// (only one bold line at size 12, six regular lines at size 10).
fn push_text_right(ops: &mut Vec<Operation>, x_right: i64, y: i64, font: &str, size: i64, s: &str) {
    let bytes = winansi_bytes(s);
    let approx_width = (bytes.len() as i64 * size * 55) / 100;
    let x = x_right - approx_width;
    push_text(ops, x, y, font, size, s);
}

/// Horizontal rule at y, full content-area width. 0.5pt soft grey
/// per design-doc §13 aesthetic.
fn push_rule(ops: &mut Vec<Operation>, x0: i64, x1: i64, y: i64) {
    ops.push(Operation::new(
        "RG",
        vec![0.72.into(), 0.72.into(), 0.74.into()],
    ));
    ops.push(Operation::new("w", vec![0.5.into()]));
    ops.push(Operation::new("m", vec![x0.into(), y.into()]));
    ops.push(Operation::new("l", vec![x1.into(), y.into()]));
    ops.push(Operation::new("S", vec![]));
}

/// Truncate a string by char count, appending `...` if cut. Avoids
/// splitting WinAnsi multi-byte sequences mid-rune.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }
}

/// Chunk a long string into ≤ `width` char rows by word boundary
/// where possible. Used for the customer-notes block (which can be
/// up to 400 chars).
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

/// WinAnsi byte-encode `s`. Hungarian double-acute `ő/ű/Ő/Ű` fall
/// back to single-acute `ö/ü/Ö/Ü`; unknown chars emit `?` (visible
/// loud per CLAUDE.md rule 12, not silent loss).
fn winansi_bytes(s: &str) -> Vec<u8> {
    s.chars().map(winansi_byte_for_char).collect()
}

fn winansi_byte_for_char(c: char) -> u8 {
    match c {
        c if (c as u32) < 0x80 => c as u8,
        // HU substitutions.
        '\u{0150}' => 0xD6, // Ő → Ö
        '\u{0151}' => 0xF6, // ő → ö
        '\u{0170}' => 0xDC, // Ű → Ü
        '\u{0171}' => 0xFC, // ű → ü
        // WinAnsi supplement: only the EUR sign and em dash matter for
        // the v1 surface; rest fall through to '?'.
        '\u{20AC}' => 0x80, // €
        '\u{2014}' => 0x97, // — em dash (CP1252 0x97); used by the
        // stock-alert banner and the footer "Indicative quote —" line.
        c if (c as u32) >= 0xA0 && (c as u32) <= 0xFF => c as u8,
        _ => b'?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_quote_engine::{Feature, FeatureType, ToleranceRange};

    fn sample_inputs<'a>(
        graph: &'a FeatureGraph,
        breakdown: &'a QuoteBreakdown,
    ) -> QuoteInputs<'a> {
        QuoteInputs {
            quote_id: "00000000-0000-0000-0000-000000000001",
            customer_email: "test@example.com",
            customer_name: "Test Customer",
            customer_company: "",
            quantity: 5,
            notes: "",
            valid_until_iso: "2026-07-06",
            extractor_version: "aberp-cad-extract-wrapper@0.0.0",
            engine_version: "aberp-quote-engine@0.0.0",
            feature_graph: graph,
            breakdown,
            target_tolerance: ToleranceRange::Standard,
            stock_alert: false,
        }
    }

    fn fake_graph(requires_5_axis: bool, thin_wall: bool) -> FeatureGraph {
        FeatureGraph {
            schema_version: 1,
            bounding_box_mm: [50.0, 30.0, 20.0],
            volume_mm3: 12345.6,
            material_grade: "AL_6061_T6".to_string(),
            features: vec![Feature {
                feature_type: FeatureType::Hole,
                count: 4,
                representative_size_mm: 8.0,
            }],
            requires_5_axis,
            thin_wall_present: thin_wall,
        }
    }

    fn fake_breakdown() -> QuoteBreakdown {
        QuoteBreakdown {
            material_cost: 1.23,
            labor_cost: 9.87,
            setup_cost: 4.56,
            overhead: 1.50,
            margin: 3.84,
            total_price: 21.00,
            machining_minutes: 11.25,
            inspection_minutes: 2.0,
            route_to_5_axis: false,
            engine_version: "aberp-quote-engine@0.0.0".to_string(),
            reasoning_log: vec![
                "[material] volume_mm3=12345.6 * (1 + scrap_factor=0.05) = scrap=12962.88"
                    .to_string(),
                "[machining] sum machining_minutes=11.2500".to_string(),
                "[totals] total_price = 21.00 EUR".to_string(),
            ],
        }
    }

    #[test]
    fn render_produces_pdf_with_header_magic() {
        let g = fake_graph(false, false);
        let b = fake_breakdown();
        let inputs = sample_inputs(&g, &b);
        let bytes = render(&inputs).expect("render");
        assert!(bytes.starts_with(b"%PDF-"), "expected PDF magic bytes");
        assert!(bytes.len() > 500, "PDF unexpectedly small: {}", bytes.len());
    }

    #[test]
    fn render_extracts_text_for_total_and_quote_id() {
        let g = fake_graph(false, false);
        let b = fake_breakdown();
        let inputs = sample_inputs(&g, &b);
        let bytes = render(&inputs).expect("render");
        let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");
        assert!(text.contains("Indicative Quote"), "missing title: {text}");
        assert!(text.contains("00000000-0000-0000-0000-000000000001"));
        assert!(text.contains("2026-07-06"));
        assert!(text.contains("21.00 EUR"));
        assert!(text.contains("AL_6061_T6"));
    }

    /// EVE addendum 1 — when `requires_5_axis=true` the customer-visible
    /// "Routing: 5-axis machine" line MUST appear on the PDF.
    #[test]
    fn addendum_1_five_axis_line_appears_when_required() {
        let g = fake_graph(true, false);
        let b = fake_breakdown();
        let inputs = sample_inputs(&g, &b);
        let bytes = render(&inputs).expect("render");
        let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");
        assert!(
            text.contains("5-axis"),
            "addendum-1 5-axis line missing: {text}"
        );
    }

    /// EVE addendum 1 — the thin-wall line only appears when BOTH the
    /// FeatureGraph flag is true AND the target tolerance is Tight or
    /// higher. A loose-tolerance thin-wall part is not a surcharged
    /// part (engine `THIN_WALL_TIGHT_TOL_BUMP` matches this gate).
    #[test]
    fn addendum_1_thin_wall_line_gated_on_tolerance() {
        let g = fake_graph(false, true);
        let b = fake_breakdown();
        let mut inputs = sample_inputs(&g, &b);
        inputs.target_tolerance = ToleranceRange::Standard;
        let bytes = render(&inputs).expect("render");
        let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");
        assert!(
            !text.contains("Thin walls present"),
            "thin-wall line must NOT appear at Standard tolerance: {text}"
        );

        inputs.target_tolerance = ToleranceRange::Tight;
        let bytes = render(&inputs).expect("render");
        let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");
        assert!(
            text.contains("Thin walls present"),
            "thin-wall line MUST appear at Tight tolerance: {text}"
        );
    }

    /// EVE addendum 2 (customer-facing half) — when `stock_alert=true`
    /// the customer-visible stock-status banner MUST appear on the PDF.
    /// The EN line is asserted because it is pure-ASCII and survives
    /// `pdf_extract` cleanly; the HU line renders alongside it.
    #[test]
    fn s318_addendum2_banner_renders_when_stock_alert_true() {
        let g = fake_graph(false, false);
        let b = fake_breakdown();
        let mut inputs = sample_inputs(&g, &b);
        inputs.stock_alert = true;
        let bytes = render(&inputs).expect("render");
        let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");
        assert!(
            text.contains("Stock status changed since this quote was issued"),
            "addendum-2 stock banner missing when stock_alert=true: {text}"
        );
    }

    /// EVE addendum 2 back-compat — the default (`stock_alert=false`,
    /// what every existing caller produces) MUST NOT render the banner.
    #[test]
    fn s318_addendum2_no_banner_when_stock_alert_false() {
        let g = fake_graph(false, false);
        let b = fake_breakdown();
        let inputs = sample_inputs(&g, &b); // stock_alert defaults false
        let bytes = render(&inputs).expect("render");
        let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");
        assert!(
            !text.contains("Stock status changed"),
            "stock banner must NOT appear at stock_alert=false: {text}"
        );
    }

    /// Purity check — two calls on the same inputs produce identical
    /// byte sequences. The renderer is pure (no clock, no RNG, no
    /// counters) so this is the load-bearing invariant for using the
    /// engine's `feature_graph_hash` as the priced-writeback
    /// idempotency key (ADR-0004).
    #[test]
    fn render_is_deterministic() {
        let g = fake_graph(false, false);
        let b = fake_breakdown();
        let inputs = sample_inputs(&g, &b);
        let a = render(&inputs).expect("a");
        let b2 = render(&inputs).expect("b");
        assert_eq!(a, b2, "render must be byte-deterministic");
    }

    #[test]
    fn winansi_substitutes_hu_double_acute() {
        assert_eq!(winansi_byte_for_char('ő'), 0xF6);
        assert_eq!(winansi_byte_for_char('Ű'), 0xDC);
        assert_eq!(winansi_byte_for_char('€'), 0x80);
        assert_eq!(winansi_byte_for_char('A'), b'A');
        // Unknown char falls back to '?' loud per CLAUDE.md rule 12.
        assert_eq!(winansi_byte_for_char('日'), b'?');
    }

    #[test]
    fn wrap_chunks_splits_on_word_boundary() {
        let s = "the quick brown fox jumps over the lazy dog";
        let chunks = wrap_chunks(s, 10);
        // Every chunk except possibly the last is ≤ 10 chars.
        for c in &chunks {
            assert!(c.chars().count() <= 12, "chunk too long: {c:?}");
        }
        let joined = chunks.join(" ");
        assert_eq!(joined, s);
    }
}
