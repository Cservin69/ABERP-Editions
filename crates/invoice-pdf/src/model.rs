//! Input shapes the renderer consumes.
//!
//! Per CLAUDE.md rule 8 (read before write): the model mirrors what the
//! NAV `<InvoiceData>` body + the `RateMetadata` audit stamp + the
//! per-tenant seller TOML can supply between them. Anything not on one
//! of those three sources is not on the model — no speculative fields
//! per CLAUDE.md rule 2.

use aberp_billing::{Currency, RateMetadata};
use rust_decimal::Decimal;
use time::Date;

/// The full set of data the renderer needs to produce one printed
/// invoice. Built by the binary's `print_invoice` orchestration from
/// the NAV XML on disk + the audit-ledger rate stamp + the tenant
/// seller-info TOML.
///
/// HUF invoices set `rate_metadata` to `None` and `currency` to
/// `Currency::Huf`; the renderer's HUF back-compat branch omits the
/// Árfolyam line, the HUF-equivalent totals, and the MEGJEGYZÉS rate
/// note (matching the reference template's HUF posture per
/// `reference_aberp_invoice_template.md` §"HUF invoices").
#[derive(Debug, Clone)]
pub struct InvoiceModel {
    pub invoice_number: String,
    pub issue_date: Date,
    /// Per Hungarian regulatory convention the fulfillment date drives
    /// the §80(2) rate-date lookup. May equal `issue_date` for cash-
    /// transaction invoices; differs for periodic-billing chains.
    pub fulfillment_date: Date,
    /// Per the reference template's FIZETÉSI HATÁRIDŐ slot.
    pub payment_due_date: Date,
    /// Free-form per the reference template's FIZETÉSI MÓD slot. NAV's
    /// `<paymentMethod>` element body, conventionally "Átutalás"
    /// (bank transfer) or "Készpénz" (cash). The renderer prints the
    /// string verbatim.
    pub payment_method: String,
    pub currency: Currency,
    /// Frozen per-invoice MNB rate stamp per ADR-0037 §1.a + §2. `None`
    /// for HUF invoices (the C10 byte-identical invariant prerequisite
    /// — HUF invoices carry no rate metadata).
    pub rate_metadata: Option<RateMetadata>,
    pub supplier: PartyInfo,
    pub customer: PartyInfo,
    pub lines: Vec<LineItem>,
    /// Free-form note per the reference template's MEGJEGYZÉS slot.
    /// The renderer prefixes the rate-source note for EUR invoices
    /// (e.g., "1 EUR = 356,69 Ft") above the operator's free text.
    pub note: Option<String>,
}

/// Seller or buyer party data. Bank fields are SELLER-only; for the
/// buyer they stay `None`. The renderer hides empty rows so a buyer
/// PartyInfo prints only name / address / tax_number.
#[derive(Debug, Clone, Default)]
pub struct PartyInfo {
    pub name: String,
    /// Bare-string lines printed top-to-bottom in the address block.
    /// Conventional order: street, postal-code, city, country — but the
    /// renderer prints them verbatim, so the caller chooses the order.
    pub address_lines: Vec<String>,
    /// Hungarian tax number — printed as `ADÓSZÁM: <value>`.
    pub tax_number: String,
    /// Seller-only bank account fields. The renderer prints each
    /// labelled row only when its field is `Some(_)`.
    pub bank_account_number: Option<String>,
    pub iban: Option<String>,
    pub bank_name: Option<String>,
    pub swift_bic: Option<String>,
}

/// One printed-invoice line.
///
/// The renderer reads `net_minor`, `vat_minor`, `gross_minor` from the
/// model directly rather than recomputing — the orchestrator already
/// fetched them from the NAV XML (the regulatory record), and the
/// renderer is a pure transformation per CLAUDE.md rule 5 ("code can
/// answer, code answers"). The unit_price field is also pre-computed.
///
/// Amounts are in minor units (cents for EUR, whole forints for HUF).
/// Negative amounts permitted — storno lines carry negative totals.
#[derive(Debug, Clone)]
pub struct LineItem {
    pub description: String,
    /// S157 — decimal quantity (1.5 days, 0.25 hours), rendered with the
    /// Hungarian comma via [`crate::format::quantity`]. Pre-S157 this was
    /// `u32`, which truncated fractional quantities off the printed PDF.
    pub quantity: Decimal,
    pub unit: String,
    pub unit_price_minor: i64,
    pub net_minor: i64,
    pub vat_rate_percent: u16,
    pub vat_minor: i64,
    pub gross_minor: i64,
    /// Optional sub-line printed under the description as
    /// "Teljesítési időszak: YYYY.MM.DD – YYYY.MM.DD". The reference
    /// template carries this for periodic-billing invoices; the
    /// renderer hides the row when `None`.
    pub performance_period: Option<(Date, Date)>,
    /// PR-82 — buyer-facing per-line note ("Megjegyzés"). Optional;
    /// rendered as an italic sub-line under the description column
    /// when present, prefixed with the "Megjegyzés:" label so the
    /// buyer sees it clearly. `None` → no sub-line (the line renders
    /// at its standard height). Recipient-facing only — comes off the
    /// DuckDB `invoice_line.note` column, NOT the NAV XML.
    pub note: Option<String>,
}
