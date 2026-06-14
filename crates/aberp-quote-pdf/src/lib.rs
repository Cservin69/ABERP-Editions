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
//! A4, paginated. Header band with ABERP wordmark + "Indicative
//! Quote". Customer block. Part summary (material grade, qty, bbox,
//! volume). Price-breakdown table (material / labor / setup /
//! overhead / margin / TOTAL). The FULL reasoning_log (S404 — every
//! line, flowing onto continuation pages, never truncated).
//! Addendum-1 visibility band: "Routing: 5-axis machine" if
//! `requires_5_axis`, "Thin walls present (tight-tolerance surcharge
//! applied)" if `thin_wall_present && target_tolerance>=Tight`. Footer
//! with the Áben Consulting identity block + extractor/engine versions
//! + EUR-only money.
//!
//! ## Visual style (S396)
//!
//! Brought to printed-invoice parity: the `invoice-pdf` ADR-0044
//! silver/gold palette is ported verbatim ([`INK`] body, [`MUTED`]
//! section labels, [`SILVER_LINE`] structural rules, ONE [`GOLD_ACCENT`]
//! rule above the TOTAL) plus the invoice's footer identity grammar
//! (legal name + ADÓSZÁM). The goal: an operator forwarding this quote
//! to a customer sees the same refined surface as the invoice — no
//! "looks half-done" gap between the two client-facing documents.
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
/// S404 — flowing-content floor. The footer identity block occupies up
/// to `footer_y + 30 = 94`; any body line drawn at or below this `y`
/// would collide with it, so the reasoning log / notes page-break here.
/// A line just above the floor still has clear air above the footer.
const CONTENT_BOTTOM: i64 = 120;

// ─── S396 — invoice-parity palette (ADR-0044 silver / gold) ────────────
//
// Ported verbatim from `invoice-pdf`'s ADR-0044 brand decision so an
// operator who forwards this indicative quote to a customer sees the
// same refined surface as the printed invoice: warm ink body, soft
// silver structural rules, and ONE restrained gold accent (the rule
// above the TOTAL line). Named once here so a future brand tweak is a
// one-line edit, exactly as the invoice crate does. Pre-S396 the quote
// rendered pure-black text on a default-grey rule — dev-tool plain; this
// brings it to client-facing parity (CLAUDE.md rule 11: match the
// reference's conventions, don't fork a new look).

/// RGB triple in 0..=1, encoded for `lopdf`'s `Object::Real` (f32).
type Color = (f32, f32, f32);

/// Body ink — warm near-black. Every customer name, value, body line.
const INK: Color = (0.13, 0.13, 0.15);
/// Section labels (CUSTOMER, PART SUMMARY, …). Refined silver-grey.
const MUTED: Color = (0.46, 0.47, 0.51);
/// Structural rules — header under-rule, footer rule. Soft warm silver;
/// identical to the crate's pre-S396 rule grey (the value was already
/// invoice-silver, so the rules are byte-stable on colour).
const SILVER_LINE: Color = (0.72, 0.72, 0.74);
/// The ONE gold accent per ADR-0044's restraint rule: the rule above
/// the TOTAL (EUR) line. Mirrors the invoice's totals-banner accent.
const GOLD_ACCENT: Color = (0.72, 0.54, 0.12);
/// Danger red for the stock-alert band (pre-S396 literal, retained).
const DANGER_RED: Color = (0.75, 0.0, 0.0);

/// Footer identity — single-tenant. Áben Consulting is the only issuer
/// of these indicative quotes, so the legal name + tax number are baked
/// as literals here (the same posture as the banner/addendum literals
/// this crate already carries; the pure renderer has no app-config
/// access). Both values are the documented prod identity owned by
/// `apps/aberp/src/build_profile.rs::expected_tenant_identity` and must
/// stay in sync with it. Mirrors the invoice's ELADÓ name + ADÓSZÁM.
const SELLER_LEGAL_NAME: &str = "Áben Consulting KFT.";
const SELLER_TAX_NUMBER: &str = "24904362-2-41";

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
    /// What the engine returned. Every monetary line is rendered, and
    /// (S404) every `reasoning_log` line surfaces in full under "How we
    /// priced this." — flowing onto continuation pages, never truncated.
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

    // S404 — `build_content` returns one operation stream per page. The
    // fixed header→breakdown content lives on page 1; a long reasoning
    // log (and notes) flow onto continuation pages rather than being
    // truncated. All pages share the one font `Resources` dictionary.
    let page_streams = build_content(inputs);
    let mut kids: Vec<Object> = Vec::with_capacity(page_streams.len());
    for ops in page_streams {
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

    let mut out = Vec::with_capacity(8192);
    doc.save_to(&mut out)
        .map_err(|e| QuotePdfError::LopdfSave(e.to_string()))?;
    Ok(out)
}

/// Walk the document top-to-bottom building one content stream per page.
///
/// S404 — returns `Vec<Vec<Operation>>` (one entry per page). The fixed
/// header→price-breakdown content always fits on page 1; the FULL
/// reasoning log and the customer notes flow onto continuation pages via
/// [`page_break`] when `y` drops to [`CONTENT_BOTTOM`]. Nothing is
/// truncated — the operator and the customer see every line the engine
/// produced (CLAUDE.md rule 12 / hulye-biztos).
fn build_content(inputs: &QuoteInputs<'_>) -> Vec<Vec<Operation>> {
    let mut pages: Vec<Vec<Operation>> = Vec::new();
    let mut ops: Vec<Operation> = Vec::with_capacity(128);
    let mut y = MARGIN_TOP;

    // ── Header band ────────────────────────────────────────────────
    //
    // Title in warm ink at invoice-title weight; meta lines (ref +
    // validity) in MUTED below it; a silver structural under-rule
    // separates the header from the body — the same header grammar as
    // the printed invoice ("Számla" + number + under-rule).
    push_text_c(
        &mut ops,
        MARGIN_LEFT,
        y,
        "F2",
        26,
        INK,
        "ABERP — Indicative Quote",
    );
    y -= 26;
    push_text_c(
        &mut ops,
        MARGIN_LEFT,
        y,
        "F1",
        9,
        MUTED,
        &format!("Quote ref: {}", inputs.quote_id),
    );
    y -= 14;
    push_text_c(
        &mut ops,
        MARGIN_LEFT,
        y,
        "F1",
        9,
        MUTED,
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
    push_text_c(&mut ops, MARGIN_LEFT, y, "F2", 12, MUTED, "CUSTOMER");
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
    push_text_c(&mut ops, MARGIN_LEFT, y, "F2", 12, MUTED, "PART SUMMARY");
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
    push_text_c(&mut ops, MARGIN_LEFT, y, "F2", 12, MUTED, "PRICE BREAKDOWN");
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
    // The single gold accent (ADR-0044 restraint) — the rule above the
    // TOTAL, exactly as the invoice golds the line above its totals
    // banner. Every other rule in this document stays silver.
    push_rule_gold(&mut ops, MARGIN_LEFT, MARGIN_RIGHT, y + 6);
    push_money_row_bold(&mut ops, y, "TOTAL (EUR)", b.total_price);
    y -= 28;

    // ── Reasoning log (FULL — every line, no cap) ──────────────────
    //
    // S404 — the operator complaint: this block used to print the top 5
    // lines then "... N further line(s) on the operator's breakdown",
    // hiding the rest of the pricing logic. Now EVERY line renders;
    // overflow flows onto continuation pages. Long lines wrap (96 chars
    // at font 9 fits the content width) rather than truncate, so nothing
    // the engine reasoned is dropped (hulye-biztos — no tribal knowledge,
    // operator and customer see exactly what the engine computed).
    push_text_c(
        &mut ops,
        MARGIN_LEFT,
        y,
        "F2",
        12,
        MUTED,
        "HOW WE PRICED THIS",
    );
    y -= 16;
    for line in b.reasoning_log.iter() {
        // A reasoning line may wrap to several rows; keep all rows of a
        // line together with the page-break check applied per row.
        for chunk in wrap_chunks(line, 96) {
            if y < CONTENT_BOTTOM {
                y = page_break(&mut pages, &mut ops, inputs);
                push_text_c(
                    &mut ops,
                    MARGIN_LEFT,
                    y,
                    "F2",
                    12,
                    MUTED,
                    "HOW WE PRICED THIS (cont.)",
                );
                y -= 16;
            }
            push_text(&mut ops, MARGIN_LEFT, y, "F1", 9, &chunk);
            y -= 12;
        }
    }
    y -= 10;

    // ── Customer notes (if any) ────────────────────────────────────
    if !inputs.notes.is_empty() {
        // Page-break before the section header if the reasoning log left
        // no room, so "YOUR NOTES" never collides with the footer.
        if y < CONTENT_BOTTOM + 28 {
            y = page_break(&mut pages, &mut ops, inputs);
        }
        push_text_c(&mut ops, MARGIN_LEFT, y, "F2", 12, MUTED, "YOUR NOTES");
        y -= 16;
        for chunk in wrap_chunks(inputs.notes, 96) {
            if y < CONTENT_BOTTOM {
                y = page_break(&mut pages, &mut ops, inputs);
                push_text_c(
                    &mut ops,
                    MARGIN_LEFT,
                    y,
                    "F2",
                    12,
                    MUTED,
                    "YOUR NOTES (cont.)",
                );
                y -= 16;
            }
            push_text(&mut ops, MARGIN_LEFT, y, "F1", 9, &chunk);
            y -= 12;
        }
    }

    // ── Footer on the final page ───────────────────────────────────
    push_footer(&mut ops, inputs);
    pages.push(ops);
    pages
}

/// Finalize the current page (stamp its footer), hand it to `pages`, and
/// return the fresh top-of-page `y` for a continuation page. S404 — the
/// single seam through which the reasoning log / notes overflow, so the
/// footer is stamped consistently on every page including the last.
fn page_break(
    pages: &mut Vec<Vec<Operation>>,
    ops: &mut Vec<Operation>,
    inputs: &QuoteInputs<'_>,
) -> i64 {
    push_footer(ops, inputs);
    pages.push(std::mem::take(ops));
    MARGIN_TOP
}

/// Áben Consulting identity block above a silver rule, then the version +
/// non-binding disclaimer in MUTED. Mirrors the invoice's footer grammar
/// (seller identity + attestation line) so a forwarded quote carries the
/// same legal-identity weight as the printed invoice. Legal name in ink
/// (the brand reads strongest), tax number alongside it like the
/// invoice's ELADÓ ADÓSZÁM. S404 — stamped on every page by `page_break`.
fn push_footer(ops: &mut Vec<Operation>, inputs: &QuoteInputs<'_>) {
    let footer_y = 64;
    push_text_c(
        ops,
        MARGIN_LEFT,
        footer_y + 30,
        "F2",
        9,
        INK,
        &format!("{}  ·  Adószám: {}", SELLER_LEGAL_NAME, SELLER_TAX_NUMBER),
    );
    push_rule(ops, MARGIN_LEFT, MARGIN_RIGHT, footer_y + 24);
    push_text_c(
        ops,
        MARGIN_LEFT,
        footer_y + 8,
        "F1",
        8,
        MUTED,
        &format!(
            "Indicative quote — prices in EUR. Engine: {} | Extractor: {} | Renderer: {}",
            inputs.engine_version, inputs.extractor_version, QUOTE_PDF_RENDERER_VERSION
        ),
    );
    push_text_c(
        ops,
        MARGIN_LEFT,
        footer_y - 4,
        "F1",
        8,
        MUTED,
        "This is a non-binding indicative quote. Final pricing confirmed on order acceptance.",
    );
}

/// Push a single text run at `(x, y)` in `color` with font `font_key`
/// (`F1` / `F2`) at `size`. The `rg` (non-stroking / fill) op is set
/// per run, so every run is self-contained — no colour state bleeds
/// into the next run, and rule strokes (`RG`) stay independent.
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
    let bytes = winansi_bytes(s);
    ops.push(Operation::new(
        "Tj",
        vec![Object::String(bytes, lopdf::StringFormat::Literal)],
    ));
    ops.push(Operation::new("ET", vec![]));
}

/// Ink-coloured text run — the body-text default. Convenience wrapper
/// over [`push_text_c`] with [`INK`].
fn push_text(ops: &mut Vec<Operation>, x: i64, y: i64, font_key: &str, size: i64, s: &str) {
    push_text_c(ops, x, y, font_key, size, INK, s);
}

/// Danger-red text run for the stock-alert band. Thin wrapper over
/// [`push_text_c`] with [`DANGER_RED`]; since every run sets its own
/// fill, no manual reset-to-black is needed afterwards.
fn push_text_red(ops: &mut Vec<Operation>, x: i64, y: i64, font_key: &str, size: i64, s: &str) {
    push_text_c(ops, x, y, font_key, size, DANGER_RED, s);
}

/// Horizontal rule in the danger red, used to bracket the stock-alert
/// band. 1pt red stroke for weight.
fn push_rule_red(ops: &mut Vec<Operation>, x0: i64, x1: i64, y: i64) {
    push_rule_c(ops, x0, x1, y, DANGER_RED, 1.0);
}

/// Key-value row: MUTED label + INK value indented to a fixed column.
/// Matches the invoice's label-in-grey / value-in-ink pairing so the
/// part-summary block reads with the same hierarchy.
fn push_kv(ops: &mut Vec<Operation>, x: i64, y: i64, label: &str, value: &str) {
    push_text_c(ops, x, y, "F1", 10, MUTED, label);
    push_text_c(ops, x + 160, y, "F1", 10, INK, value);
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
/// used for the TOTAL line under the gold rule.
fn push_money_row_bold(ops: &mut Vec<Operation>, y: i64, label: &str, amount: f64) {
    push_text(ops, MARGIN_LEFT, y, "F2", 12, label);
    let s = format!("{:.2} EUR", amount);
    push_text_right(ops, MARGIN_RIGHT, y, "F2", 12, &s);
}

/// Approximate right-align: shift left by `≈ 0.55 * size * len`
/// chars before placing. Good enough for the totals column at v1
/// (only one bold line at size 12, six regular lines at size 10).
/// Always ink — the money column is body data.
fn push_text_right(ops: &mut Vec<Operation>, x_right: i64, y: i64, font: &str, size: i64, s: &str) {
    let bytes = winansi_bytes(s);
    let approx_width = (bytes.len() as i64 * size * 55) / 100;
    let x = x_right - approx_width;
    push_text(ops, x, y, font, size, s);
}

/// Structural horizontal rule at y, full content-area width. 0.5pt
/// soft silver ([`SILVER_LINE`]) — the document's default rule weight,
/// matching the invoice's structural rules.
fn push_rule(ops: &mut Vec<Operation>, x0: i64, x1: i64, y: i64) {
    push_rule_c(ops, x0, x1, y, SILVER_LINE, 0.5);
}

/// The ONE gold accent rule (above the TOTAL line), per ADR-0044's
/// restraint. Slightly heavier stroke (0.85pt) than the silver rules so
/// the accent reads as deliberate — mirrors the invoice's gold weight.
fn push_rule_gold(ops: &mut Vec<Operation>, x0: i64, x1: i64, y: i64) {
    push_rule_c(ops, x0, x1, y, GOLD_ACCENT, 0.85);
}

/// Underlying rule emitter — sets stroke colour (`RG`) + weight (`w`),
/// moves to `(x0, y)`, lines to `(x1, y)`, strokes (`S`).
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

    /// S396 customer-journey e2e — the polish pass MUST NOT drop any
    /// customer-facing surface. Render a fully-populated quote (company +
    /// notes + multi-line reasoning) and assert every load-bearing
    /// section's text still extracts. This is the regression gate: a
    /// future style edit that accidentally deletes a section fires here,
    /// not on a customer's screen (CLAUDE.md rule 9 — fails when the
    /// intent regresses, not just the bytes).
    #[test]
    fn s396_all_customer_facing_sections_present_after_polish() {
        let g = fake_graph(true, true);
        let b = fake_breakdown();
        let mut inputs = sample_inputs(&g, &b);
        inputs.customer_name = "Példa Ügyfél";
        inputs.customer_company = "Acme Manufacturing Ltd.";
        inputs.customer_email = "buyer@acme.example";
        inputs.notes = "Please prioritise lead time over cost.";
        inputs.target_tolerance = ToleranceRange::Tight;
        let bytes = render(&inputs).expect("render");
        let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");

        // Header + meta.
        assert!(text.contains("Indicative Quote"), "title: {text}");
        assert!(text.contains(inputs.quote_id), "quote ref: {text}");
        assert!(text.contains("2026-07-06"), "valid until: {text}");
        // Section headers.
        for section in [
            "CUSTOMER",
            "PART SUMMARY",
            "PRICE BREAKDOWN",
            "HOW WE PRICED THIS",
            "YOUR NOTES",
        ] {
            assert!(text.contains(section), "missing section {section}: {text}");
        }
        // Customer block (company + email are ASCII-safe to assert).
        assert!(text.contains("Acme Manufacturing Ltd."), "company: {text}");
        assert!(text.contains("buyer@acme.example"), "email: {text}");
        // Part summary + addenda + breakdown + total.
        assert!(text.contains("AL_6061_T6"), "material grade: {text}");
        assert!(text.contains("5-axis"), "addendum 5-axis: {text}");
        assert!(
            text.contains("Thin walls present"),
            "addendum thin-wall: {text}"
        );
        assert!(text.contains("TOTAL (EUR)"), "total label: {text}");
        assert!(text.contains("21.00 EUR"), "total amount: {text}");
        // Notes body.
        assert!(text.contains("prioritise lead time"), "notes body: {text}");
        // Footer identity block (ASCII-safe substrings per the
        // stock-banner test's pdf_extract precedent).
        assert!(
            text.contains("Consulting KFT."),
            "footer legal name: {text}"
        );
        assert!(
            text.contains(SELLER_TAX_NUMBER),
            "footer tax number: {text}"
        );
        // Disclaimer survives the footer restructure.
        assert!(
            text.contains("non-binding indicative quote"),
            "disclaimer: {text}"
        );
    }

    /// S396 sanity smoke — the branded render is non-trivial and at least
    /// as large as the pre-polish output (the polish only ADDS ops:
    /// per-run fill colours + the footer identity line). Byte length, not
    /// a pixel compare, per the visual-validation-live-only posture. The
    /// crate emits uncompressed PDF, so 2000 is a comfortable floor for a
    /// branded single-page quote.
    #[test]
    fn s396_branded_render_byte_length_sanity() {
        let g = fake_graph(false, false);
        let b = fake_breakdown();
        let inputs = sample_inputs(&g, &b);
        let bytes = render(&inputs).expect("render");
        assert!(
            bytes.len() > 2000,
            "branded quote PDF unexpectedly small: {} bytes",
            bytes.len()
        );
    }

    /// S396 palette pin — the silver/gold values are the brand decision
    /// (ADR-0044), ported from `invoice-pdf`. A "let me just nudge the
    /// gold" drift away from invoice parity fails here loudly. Mirrors
    /// the invoice crate's `palette_constants_match_brand_decision`.
    #[test]
    fn s396_palette_matches_invoice_brand_decision() {
        assert_eq!(INK, (0.13, 0.13, 0.15));
        assert_eq!(MUTED, (0.46, 0.47, 0.51));
        assert_eq!(SILVER_LINE, (0.72, 0.72, 0.74));
        assert_eq!(GOLD_ACCENT, (0.72, 0.54, 0.12));
    }

    /// S396 footer identity pin — the baked legal name + tax number must
    /// match the documented prod identity owned by
    /// `apps/aberp/src/build_profile.rs::expected_tenant_identity`. If
    /// that source of truth changes, this pin forces this literal to be
    /// updated in lockstep (the pure crate can't import it).
    #[test]
    fn s396_footer_identity_matches_prod_tenant() {
        assert_eq!(SELLER_LEGAL_NAME, "Áben Consulting KFT.");
        assert_eq!(SELLER_TAX_NUMBER, "24904362-2-41");
    }

    /// S404 — build a breakdown whose reasoning_log has `n` ASCII-marked
    /// lines so each is individually assertable in the extracted text.
    fn breakdown_with_reasoning(n: usize) -> QuoteBreakdown {
        let mut b = fake_breakdown();
        b.reasoning_log = (0..n)
            .map(|i| format!("RLINE_{i}_MARK pricing step number {i}"))
            .collect();
        b
    }

    fn page_count(bytes: &[u8]) -> usize {
        let doc = lopdf::Document::load_mem(bytes).expect("load pdf");
        doc.get_pages().len()
    }

    /// S404 core regression — the old renderer printed the top 5 lines
    /// then "... N further line(s) on the operator's breakdown". That
    /// truncation MUST be gone: every reasoning line renders, and the
    /// "further line(s)" tail string is never emitted. Asserts the FIRST,
    /// a MIDDLE, and the LAST marker all extract (rule 9 — fails the
    /// moment a cap is reintroduced, not just on a byte nudge).
    #[test]
    fn s404_full_reasoning_log_rendered_no_truncation() {
        let g = fake_graph(false, false);
        for n in [3usize, 12, 50, 100] {
            let b = breakdown_with_reasoning(n);
            let inputs = sample_inputs(&g, &b);
            let bytes = render(&inputs).expect("render");
            let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");
            assert!(
                !text.contains("further line"),
                "n={n}: truncation tail must be gone: {text}"
            );
            for i in [0usize, n / 2, n - 1] {
                assert!(
                    text.contains(&format!("RLINE_{i}_MARK")),
                    "n={n}: reasoning line {i} missing from PDF"
                );
            }
        }
    }

    /// S404 — a long reasoning log spills onto continuation pages rather
    /// than overrunning the footer. 100 lines must produce >1 page; a
    /// short log stays single-page (no gratuitous blank pages).
    #[test]
    fn s404_long_reasoning_log_paginates() {
        let g = fake_graph(false, false);

        let short = breakdown_with_reasoning(3);
        let short_inputs = sample_inputs(&g, &short);
        assert_eq!(
            page_count(&render(&short_inputs).expect("render")),
            1,
            "a 3-line log must stay single-page"
        );

        let long = breakdown_with_reasoning(100);
        let long_inputs = sample_inputs(&g, &long);
        assert!(
            page_count(&render(&long_inputs).expect("render")) > 1,
            "a 100-line reasoning log must span multiple pages"
        );
    }

    /// S404 — every page carries the footer identity block (so a printed
    /// continuation page is still legally attributable). The seller tax
    /// number must appear once per page.
    #[test]
    fn s404_footer_on_every_page() {
        let g = fake_graph(false, false);
        let b = breakdown_with_reasoning(100);
        let inputs = sample_inputs(&g, &b);
        let bytes = render(&inputs).expect("render");
        let pages = page_count(&bytes);
        assert!(pages > 1, "expected multipage for this test");
        let text = pdf_extract::extract_text_from_mem(&bytes).expect("extract");
        let footers = text.matches(SELLER_TAX_NUMBER).count();
        assert_eq!(
            footers, pages,
            "footer tax number must appear once per page ({pages} pages, {footers} footers)"
        );
    }

    /// S404 — render stays byte-deterministic with the multi-page path.
    /// The pagination loop must not introduce any clock/RNG/iteration
    /// nondeterminism (load-bearing for the writeback idempotency key).
    #[test]
    fn s404_multipage_render_is_deterministic() {
        let g = fake_graph(false, false);
        let b = breakdown_with_reasoning(80);
        let inputs = sample_inputs(&g, &b);
        let a = render(&inputs).expect("a");
        let b2 = render(&inputs).expect("b");
        assert_eq!(a, b2, "multi-page render must be byte-deterministic");
    }
}
