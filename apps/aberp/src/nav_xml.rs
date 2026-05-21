//! NAV v3.0 `InvoiceData` XML serialization.
//!
//! Produces the XML body that NAV expects inside a `manageInvoice`
//! `invoiceData` element. The SOAP envelope wrapping (request signature,
//! token exchange, transactionId tracking) belongs to the NAV adapter PR;
//! this file is the structural payload only.
//!
//! # Scope (PR-5 minimum subset)
//!
//! Required elements only:
//!
//! - `invoiceNumber`, `invoiceIssueDate`
//! - `invoiceMain/invoice/invoiceHead/supplierInfo` (taxNumber, name, address)
//! - `invoiceHead/customerInfo` (customerVatStatus, customerTaxNumber, name)
//! - `invoiceLines/line[*]` with lineNumber, lineDescription,
//!   quantity, unitOfMeasure, unitPrice, lineNetAmount, lineVatRate,
//!   lineVatData
//! - `invoiceSummary/summaryNormal` (sum totals)
//!
//! # NOT in scope (deferred to follow-up PRs)
//!
//! - XSD validation at runtime (ADR-0021 deferred item, named trigger
//!   "first PR implementing schema-drift detection per ADR-0009 §1").
//! - Optional NAV fields (deliveryDate, paymentDate, currency, exchange
//!   rate, foreign-currency lines, vatRateExemption, vatRateOutOfScope,
//!   product-codes, line-modification-reference, invoiceReference for
//!   storno/modify chains).
//! - SOAP envelope with `xmlSignKey` / `xmlChangeKey` signing.
//!
//! # NAV-compatibility claim
//!
//! "NAV-compatible" here means structurally matches NAV v3.0 InvoiceData
//! by inspection. PR-5 does NOT run an XSD validator; that lands when the
//! XSD-validator crate is picked per ADR-0021 §Items deferred.

use std::io::Write;

use aberp_billing::{Huf, LineItem, ReadyInvoice, SeriesCode};
use anyhow::{Context, Result};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

/// Supplier + customer party data that the billing module does not own.
/// Supplied by the input JSON in PR-5; will move into its own module
/// when the customer module ships.
#[derive(Debug, Clone)]
pub struct NavParties {
    pub supplier: SupplierInfo,
    pub customer: CustomerInfo,
}

#[derive(Debug, Clone)]
pub struct SupplierInfo {
    pub tax_number: String,
    pub name: String,
    pub address_country_code: String,
    pub address_postal_code: String,
    pub address_city: String,
    pub address_street: String,
}

#[derive(Debug, Clone)]
pub struct CustomerInfo {
    pub tax_number: String,
    pub name: String,
}

/// Storno chain-link reference data for [`render_storno_data`] (PR-10,
/// ADR-0023). Pinpoints the base invoice and the chain index this
/// storno asserts. The XML emitter renders these into an
/// `<invoiceReference>` block inside `<invoice>` (positioned BEFORE
/// `<invoiceHead>` per NAV v3.0 schema).
#[derive(Debug, Clone)]
pub struct StornoReference {
    /// Base invoice's NAV-facing number — formatted as `<series>/<5-digit-seq>`
    /// (e.g. `INV-default/00007`). The caller constructs this from the
    /// base invoice row's series + sequence_number; see
    /// `issue_storno::run` step 10.
    pub base_invoice_number: String,
    /// `<modificationIndex>` allocated by the chain walker per
    /// ADR-0023 §4 — starts at 1, increments per chain entry.
    pub modification_index: u32,
}

const NAV_NS_DATA: &str = "http://schemas.nav.gov.hu/OSA/3.0/data";
const NAV_NS_BASE: &str = "http://schemas.nav.gov.hu/OSA/3.0/base";

/// Render `<InvoiceData>` to bytes. The invoice number is built from the
/// series code and the allocator-burned sequence number: `INV-default/00042`.
pub fn render_invoice_data(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    // <?xml version="1.0" encoding="UTF-8"?>
    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("XML declaration")?;

    // Root element with namespaces.
    let mut root = BytesStart::new("InvoiceData");
    root.push_attribute(("xmlns", NAV_NS_DATA));
    root.push_attribute(("xmlns:common", NAV_NS_BASE));
    w.write_event(Event::Start(root))
        .context("write <InvoiceData>")?;

    let invoice_number = format!("{}/{:05}", series_code.as_str(), invoice.sequence_number,);
    text_element(&mut w, "invoiceNumber", &invoice_number)?;
    // NAV InvoiceData wants `xs:date` (YYYY-MM-DD), not full RFC3339.
    // Format manually rather than depending on `time::Iso8601`'s
    // const-generic configuration.
    let date = invoice.issue_date.date();
    let issue_date = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day(),
    );
    text_element(&mut w, "invoiceIssueDate", &issue_date)?;

    // <invoiceMain>
    w.write_event(Event::Start(BytesStart::new("invoiceMain")))?;
    w.write_event(Event::Start(BytesStart::new("invoice")))?;

    // <invoiceHead>
    w.write_event(Event::Start(BytesStart::new("invoiceHead")))?;
    write_supplier(&mut w, &parties.supplier)?;
    write_customer(&mut w, &parties.customer)?;
    write_invoice_detail(&mut w, &issue_date)?;
    w.write_event(Event::End(BytesEnd::new("invoiceHead")))?;

    // <invoiceLines>
    write_lines(&mut w, &invoice.lines)?;

    // <invoiceSummary>
    write_summary(&mut w, &invoice.lines)?;

    w.write_event(Event::End(BytesEnd::new("invoice")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceMain")))?;
    w.write_event(Event::End(BytesEnd::new("InvoiceData")))?;

    Ok(buf)
}

/// Render the storno's `<InvoiceData>` to bytes (PR-10, ADR-0023).
///
/// Two differences from [`render_invoice_data`]:
///
/// 1. An `<invoiceReference>` block appears inside `<invoice>`,
///    positioned BEFORE `<invoiceHead>` per NAV v3.0 schema. Carries
///    `originalInvoiceNumber` + `modifyWithoutMaster` (always `false`
///    for PR-10 — ADR-0023 §4 names the migrated-base path that
///    would set this `true` and explicitly defers it) +
///    `modificationIndex`.
///
/// 2. Line and summary amounts are **negated** per NAV's storno
///    convention. Negation is done by constructing a parallel
///    `Vec<LineItem>` with negated `unit_price` (`Huf` wraps `i64`,
///    so negative is representable); `net_total` /  `vat_amount` /
///    `gross_total` cascade to negative naturally because the same
///    multiplications now run against a negative `unit_price`. This
///    keeps the line-writer logic shared with [`render_invoice_data`]
///    instead of forking a parallel `write_storno_lines` — CLAUDE.md
///    rule 2 (no speculative abstractions).
///
/// The `invoice` argument carries the STORNO's own sequence number
/// (the storno is itself an invoice with its own allocator slot per
/// ADR-0009 §6 / ADR-0023 §3); `storno_reference.base_invoice_number`
/// names what is being cancelled.
pub fn render_storno_data(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    storno_reference: &StornoReference,
) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("XML declaration")?;

    let mut root = BytesStart::new("InvoiceData");
    root.push_attribute(("xmlns", NAV_NS_DATA));
    root.push_attribute(("xmlns:common", NAV_NS_BASE));
    w.write_event(Event::Start(root))
        .context("write <InvoiceData> (storno)")?;

    // Storno's OWN invoice number — the cancellation is itself an invoice.
    let invoice_number = format!("{}/{:05}", series_code.as_str(), invoice.sequence_number);
    text_element(&mut w, "invoiceNumber", &invoice_number)?;
    let date = invoice.issue_date.date();
    let issue_date = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day(),
    );
    text_element(&mut w, "invoiceIssueDate", &issue_date)?;

    w.write_event(Event::Start(BytesStart::new("invoiceMain")))?;
    w.write_event(Event::Start(BytesStart::new("invoice")))?;

    // <invoiceReference> — STORNO-only. Position: direct child of
    // <invoice>, BEFORE <invoiceHead>, per NAV v3.0 schema.
    write_invoice_reference(&mut w, storno_reference)?;

    // <invoiceHead> reuses the standard supplier/customer/detail
    // section writers — the storno's parties and detail block are
    // identical in shape to a fresh invoice's. The NAV-side operation
    // (CREATE vs STORNO vs MODIFY) is set on the SOAP envelope at
    // submit time, not inside <InvoiceData>; submit_invoice.rs
    // detects the storno shape by the presence of <invoiceReference>
    // (PR-10 F20).
    w.write_event(Event::Start(BytesStart::new("invoiceHead")))?;
    write_supplier(&mut w, &parties.supplier)?;
    write_customer(&mut w, &parties.customer)?;
    write_invoice_detail(&mut w, &issue_date)?;
    w.write_event(Event::End(BytesEnd::new("invoiceHead")))?;

    // <invoiceLines> with negated amounts. Negate by constructing a
    // parallel Vec with negated unit_price; net/vat/gross cascade
    // through `LineItem::net_total` etc. unchanged.
    let negated_lines: Vec<LineItem> = invoice.lines.iter().map(negate_line).collect();
    write_lines(&mut w, &negated_lines)?;
    write_summary(&mut w, &negated_lines)?;

    w.write_event(Event::End(BytesEnd::new("invoice")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceMain")))?;
    w.write_event(Event::End(BytesEnd::new("InvoiceData")))?;

    Ok(buf)
}

/// Negate a `LineItem` for storno emission. Quantities stay positive
/// (`u32` cannot represent negative); the negation lives in
/// `unit_price`, which is `Huf(i64)` and can be negative. The
/// cascading `net_total` / `vat_amount` / `gross_total` are all
/// negative as a result, which matches NAV's storno convention.
fn negate_line(line: &LineItem) -> LineItem {
    LineItem {
        description: line.description.clone(),
        quantity: line.quantity,
        unit_price: Huf(line.unit_price.as_i64().saturating_neg()),
        vat_rate_basis_points: line.vat_rate_basis_points,
    }
}

/// Write the `<invoiceReference>` chain-link block. PR-10 always
/// emits `modifyWithoutMaster=false`: ADR-0023 §4 names the
/// `queryInvoiceChainDigest` path for migrated-from-Billingo bases
/// (the case where `modifyWithoutMaster=true` would be the right
/// value) and explicitly defers it. When the migrated-base path
/// lands, this function gains a `modify_without_master: bool` field
/// on `StornoReference`; for PR-10 the constant-false value is
/// loud-pinned here.
fn write_invoice_reference(
    w: &mut Writer<&mut Vec<u8>>,
    storno_reference: &StornoReference,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("invoiceReference")))?;
    text_element(w, "originalInvoiceNumber", &storno_reference.base_invoice_number)?;
    text_element(w, "modifyWithoutMaster", "false")?;
    text_element(
        w,
        "modificationIndex",
        &storno_reference.modification_index.to_string(),
    )?;
    w.write_event(Event::End(BytesEnd::new("invoiceReference")))?;
    Ok(())
}

// ── Section writers ───────────────────────────────────────────────────

fn write_supplier(w: &mut Writer<&mut Vec<u8>>, s: &SupplierInfo) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("supplierInfo")))?;
    text_element(w, "supplierTaxNumber", &s.tax_number)?;
    text_element(w, "supplierName", &s.name)?;
    write_address(w, "supplierAddress", s)?;
    w.write_event(Event::End(BytesEnd::new("supplierInfo")))?;
    Ok(())
}

fn write_customer(w: &mut Writer<&mut Vec<u8>>, c: &CustomerInfo) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("customerInfo")))?;
    text_element(w, "customerVatStatus", "DOMESTIC")?;
    w.write_event(Event::Start(BytesStart::new("customerVatData")))?;
    w.write_event(Event::Start(BytesStart::new("customerTaxNumber")))?;
    text_element(w, "taxpayerId", &c.tax_number)?;
    w.write_event(Event::End(BytesEnd::new("customerTaxNumber")))?;
    w.write_event(Event::End(BytesEnd::new("customerVatData")))?;
    text_element(w, "customerName", &c.name)?;
    w.write_event(Event::End(BytesEnd::new("customerInfo")))?;
    Ok(())
}

fn write_address(w: &mut Writer<&mut Vec<u8>>, tag: &str, s: &SupplierInfo) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag.to_string())))?;
    w.write_event(Event::Start(BytesStart::new("simpleAddress")))?;
    text_element(w, "countryCode", &s.address_country_code)?;
    text_element(w, "postalCode", &s.address_postal_code)?;
    text_element(w, "city", &s.address_city)?;
    text_element(w, "additionalAddressDetail", &s.address_street)?;
    w.write_event(Event::End(BytesEnd::new("simpleAddress")))?;
    w.write_event(Event::End(BytesEnd::new(tag.to_string())))?;
    Ok(())
}

fn write_invoice_detail(w: &mut Writer<&mut Vec<u8>>, issue_date: &str) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("invoiceDetail")))?;
    text_element(w, "invoiceCategory", "NORMAL")?;
    text_element(w, "invoiceDeliveryDate", issue_date)?;
    text_element(w, "currencyCode", "HUF")?;
    text_element(w, "exchangeRate", "1")?;
    text_element(w, "paymentMethod", "TRANSFER")?;
    text_element(w, "paymentDate", issue_date)?;
    text_element(w, "invoiceAppearance", "ELECTRONIC")?;
    w.write_event(Event::End(BytesEnd::new("invoiceDetail")))?;
    Ok(())
}

fn write_lines(w: &mut Writer<&mut Vec<u8>>, lines: &[LineItem]) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("invoiceLines")))?;
    text_element(w, "mergedItemIndicator", "false")?;
    for (ordinal, line) in lines.iter().enumerate() {
        let line_number = (ordinal + 1) as u32;
        w.write_event(Event::Start(BytesStart::new("line")))?;
        text_element(w, "lineNumber", &line_number.to_string())?;
        text_element(w, "lineExpressionIndicator", "false")?;
        text_element(w, "lineDescription", &line.description)?;
        text_element(w, "quantity", &line.quantity.to_string())?;
        text_element(w, "unitOfMeasure", "PIECE")?;
        text_element(w, "unitPrice", &line.unit_price.as_i64().to_string())?;

        let net = line
            .net_total()
            .context("line net_total overflow during XML render")?;
        let vat = line
            .vat_amount()
            .context("line vat_amount overflow during XML render")?;
        let gross = line
            .gross_total()
            .context("line gross_total overflow during XML render")?;

        w.write_event(Event::Start(BytesStart::new("lineAmountsNormal")))?;
        write_line_net(w, net)?;
        write_line_vat_rate(w, line.vat_rate_basis_points)?;
        write_line_vat_amount(w, vat)?;
        write_line_gross(w, gross)?;
        w.write_event(Event::End(BytesEnd::new("lineAmountsNormal")))?;

        w.write_event(Event::End(BytesEnd::new("line")))?;
    }
    w.write_event(Event::End(BytesEnd::new("invoiceLines")))?;
    Ok(())
}

fn write_line_net(w: &mut Writer<&mut Vec<u8>>, net: Huf) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("lineNetAmountData")))?;
    text_element(w, "lineNetAmount", &net.as_i64().to_string())?;
    text_element(w, "lineNetAmountHUF", &net.as_i64().to_string())?;
    w.write_event(Event::End(BytesEnd::new("lineNetAmountData")))?;
    Ok(())
}

fn write_line_vat_rate(w: &mut Writer<&mut Vec<u8>>, vat_rate_basis_points: u16) -> Result<()> {
    let rate_decimal = format!("{:.2}", vat_rate_basis_points as f64 / 10000.0);
    w.write_event(Event::Start(BytesStart::new("lineVatRate")))?;
    text_element(w, "vatPercentage", &rate_decimal)?;
    w.write_event(Event::End(BytesEnd::new("lineVatRate")))?;
    Ok(())
}

fn write_line_vat_amount(w: &mut Writer<&mut Vec<u8>>, vat: Huf) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("lineVatData")))?;
    text_element(w, "lineVatAmount", &vat.as_i64().to_string())?;
    text_element(w, "lineVatAmountHUF", &vat.as_i64().to_string())?;
    w.write_event(Event::End(BytesEnd::new("lineVatData")))?;
    Ok(())
}

fn write_line_gross(w: &mut Writer<&mut Vec<u8>>, gross: Huf) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("lineGrossAmountData")))?;
    text_element(w, "lineGrossAmountNormal", &gross.as_i64().to_string())?;
    text_element(w, "lineGrossAmountNormalHUF", &gross.as_i64().to_string())?;
    w.write_event(Event::End(BytesEnd::new("lineGrossAmountData")))?;
    Ok(())
}

fn write_summary(w: &mut Writer<&mut Vec<u8>>, lines: &[LineItem]) -> Result<()> {
    let mut net_total = Huf::ZERO;
    let mut vat_total = Huf::ZERO;
    let mut gross_total = Huf::ZERO;
    for (i, line) in lines.iter().enumerate() {
        net_total = net_total
            .checked_add(line.net_total().unwrap_or(Huf::ZERO))
            .with_context(|| format!("net overflow at line {i}"))?;
        vat_total = vat_total
            .checked_add(line.vat_amount().unwrap_or(Huf::ZERO))
            .with_context(|| format!("vat overflow at line {i}"))?;
        gross_total = gross_total
            .checked_add(line.gross_total().unwrap_or(Huf::ZERO))
            .with_context(|| format!("gross overflow at line {i}"))?;
    }

    w.write_event(Event::Start(BytesStart::new("invoiceSummary")))?;
    w.write_event(Event::Start(BytesStart::new("summaryNormal")))?;
    // summaryByVatRate (one entry, assuming all lines share a rate; PR-5
    // does not yet group by rate — a follow-up extends this for
    // multi-rate invoices).
    if let Some(first) = lines.first() {
        w.write_event(Event::Start(BytesStart::new("summaryByVatRate")))?;
        write_line_vat_rate(w, first.vat_rate_basis_points)?;
        w.write_event(Event::Start(BytesStart::new("vatRateNetData")))?;
        text_element(w, "vatRateNetAmount", &net_total.as_i64().to_string())?;
        text_element(w, "vatRateNetAmountHUF", &net_total.as_i64().to_string())?;
        w.write_event(Event::End(BytesEnd::new("vatRateNetData")))?;
        w.write_event(Event::Start(BytesStart::new("vatRateVatData")))?;
        text_element(w, "vatRateVatAmount", &vat_total.as_i64().to_string())?;
        text_element(w, "vatRateVatAmountHUF", &vat_total.as_i64().to_string())?;
        w.write_event(Event::End(BytesEnd::new("vatRateVatData")))?;
        w.write_event(Event::Start(BytesStart::new("vatRateGrossData")))?;
        text_element(w, "vatRateGrossAmount", &gross_total.as_i64().to_string())?;
        text_element(
            w,
            "vatRateGrossAmountHUF",
            &gross_total.as_i64().to_string(),
        )?;
        w.write_event(Event::End(BytesEnd::new("vatRateGrossData")))?;
        w.write_event(Event::End(BytesEnd::new("summaryByVatRate")))?;
    }
    text_element(w, "invoiceNetAmount", &net_total.as_i64().to_string())?;
    text_element(w, "invoiceNetAmountHUF", &net_total.as_i64().to_string())?;
    text_element(w, "invoiceVatAmount", &vat_total.as_i64().to_string())?;
    text_element(w, "invoiceVatAmountHUF", &vat_total.as_i64().to_string())?;
    w.write_event(Event::End(BytesEnd::new("summaryNormal")))?;
    w.write_event(Event::Start(BytesStart::new("summaryGrossData")))?;
    text_element(w, "invoiceGrossAmount", &gross_total.as_i64().to_string())?;
    text_element(
        w,
        "invoiceGrossAmountHUF",
        &gross_total.as_i64().to_string(),
    )?;
    w.write_event(Event::End(BytesEnd::new("summaryGrossData")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceSummary")))?;
    Ok(())
}

fn text_element(w: &mut Writer<&mut Vec<u8>>, tag: &str, value: &str) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag.to_string())))?;
    w.write_event(Event::Text(BytesText::new(value)))?;
    w.write_event(Event::End(BytesEnd::new(tag.to_string())))?;
    Ok(())
}

/// Write the rendered XML to a file path.
pub fn write_to_path(path: &std::path::Path, xml: &[u8]) -> Result<()> {
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("create output XML file at {}", path.display()))?;
    file.write_all(xml)
        .with_context(|| format!("write XML to {}", path.display()))?;
    Ok(())
}
