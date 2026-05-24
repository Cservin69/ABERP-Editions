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

use aberp_billing::{
    huf_equivalent_round_half_even, Currency, Huf, LineItem, RateMetadata, ReadyInvoice,
    SeriesCode,
};
use anyhow::{anyhow, Context, Result};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use rust_decimal::Decimal;

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

/// Modification chain-link reference data for
/// [`render_modification_data`] (PR-11, ADR-0024). The MODIFY-shape
/// counterpart to [`StornoReference`]: same base-invoice + chain-index
/// pair, PLUS the operator-supplied `<modificationIssueDate>` that
/// NAV requires for MODIFY but not for STORNO (ADR-0024 §3).
///
/// The XML emitter renders these into the SAME `<invoiceReference>`
/// block shape as STORNO, with the additional `<modificationIssueDate>`
/// child positioned between `<originalInvoiceNumber>` and
/// `<modifyWithoutMaster>` per ADR-0024 §1 conflict 1's chosen
/// reading of the research-doc grammar.
#[derive(Debug, Clone)]
pub struct ModificationReference {
    /// Base invoice's NAV-facing number — same shape + caller
    /// discipline as [`StornoReference::base_invoice_number`].
    pub base_invoice_number: String,
    /// `<modificationIndex>` allocated by the widened chain walker
    /// per ADR-0024 §7 — walks both `InvoiceStornoIssued` AND
    /// `InvoiceModificationIssued` entries against the same base, so
    /// the index is globally unique across the chain regardless of
    /// per-kind order.
    pub modification_index: u32,
    /// `<modificationIssueDate>` operator-supplied date the
    /// modification was issued, in canonical `YYYY-MM-DD` form
    /// (validated at the CLI boundary per
    /// `apps/aberp/src/issue_modification.rs` step 2).
    pub modification_issue_date: String,
}

const NAV_NS_DATA: &str = "http://schemas.nav.gov.hu/OSA/3.0/data";
const NAV_NS_BASE: &str = "http://schemas.nav.gov.hu/OSA/3.0/base";
/// NAV v3.0 annul namespace per ADR-0025 §"Surfaced conflict 1"'s
/// chosen reading. The `manageInvoice` body uses `OSA/3.0/data`;
/// the `manageAnnulment` counterpart by NAV's namespace convention
/// uses `OSA/3.0/annul`. Verification deferred to first NAV-testbed
/// annulment POST (the future submit-annulment PR).
const NAV_NS_ANNUL: &str = "http://schemas.nav.gov.hu/OSA/3.0/annul";

/// Technical-annulment reference data for [`render_annulment_data`]
/// (PR-12, ADR-0025). The annulment is **not** a chain operation:
/// no `<invoiceReference>` block, no `modificationIndex`. The
/// payload-side analogue is
/// `audit_payloads::InvoiceTechnicalAnnulmentRequestedPayload`;
/// this struct carries only the fields that surface on the
/// `<InvoiceAnnulment>` XML body (the audit payload additionally
/// carries the operator-decision idempotency + prior transaction id
/// for the audit-evidence bundle, which do not appear on the wire).
#[derive(Debug, Clone)]
pub struct AnnulmentReference {
    /// Base invoice's NAV-facing number — same shape + caller
    /// discipline as [`StornoReference::base_invoice_number`] /
    /// [`ModificationReference::base_invoice_number`]. Becomes the
    /// `<annulmentReference>` text content.
    pub base_invoice_number: String,
    /// NAV annulment code in canonical wire form — one of
    /// `ERRATIC_DATA` / `ERRATIC_INVOICE_NUMBER` /
    /// `ERRATIC_INVOICE_ISSUE_DATE` /
    /// `ERRATIC_ELECTRONIC_HASH_VALUE`. The caller converts the
    /// clap-ValueEnum form to the wire form via
    /// `cli::AnnulmentCode::to_wire` before constructing this
    /// struct.
    pub annulment_code: &'static str,
    /// Operator-supplied reason text — escaped by `quick_xml`'s
    /// text writer the same way every other text-element write
    /// goes through.
    pub reason: String,
}

/// Render `<InvoiceData>` to bytes. The invoice number is built from the
/// series code and the allocator-burned sequence number: `INV-default/00042`.
///
/// # Currency + rate metadata (PR-44δ / ADR-0037 §1.b)
///
/// `currency` carries the typed `Currency` (HUF or EUR per ADR-0037 §3's
/// closed vocab). `rate_metadata` MUST be `Some(_)` when `currency` is a
/// non-HUF variant (ADR-0037 §4 invariant C1's wire-side counterpart) and
/// SHOULD be `None` for HUF. The function loud-fails on
/// `Currency::Eur` + `None` rather than silently emitting a HUF-shaped
/// body for an EUR invoice — CLAUDE.md rule 12.
///
/// For HUF the wire body is byte-near-identical to the pre-PR-44δ shape
/// (`<currencyCode>HUF</currencyCode>`, all per-VAT-rate + invoice-level
/// `*HUF` amounts equal to their non-HUF siblings) with one deliberate
/// change: `<exchangeRate>` now serializes as `1.000000` (6 decimals per
/// ADR-0037 §1.c + C11) rather than the prior `1`. The NAV XSD accepts
/// both; the 6-decimal form pins the C11 precision invariant uniformly
/// across HUF and EUR.
///
/// For EUR the rate stamped at PR-44γ is read from `rate_metadata.rate`
/// (a `rust_decimal::Decimal` — full precision MNB returned). Per-VAT-rate
/// and invoice-level HUF amounts are computed via the same
/// `huf_equivalent_round_half_even` helper PR-44γ uses for the per-invoice
/// gross-total stamp; the rate is NOT re-fetched here (per the
/// session-52 brief — drift from the audit ledger's stamped rate is the
/// failure mode this design rules out).
pub fn render_invoice_data(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<Vec<u8>> {
    ensure_rate_metadata_invariant(currency, rate_metadata)?;

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
    write_invoice_detail(&mut w, &issue_date, currency, rate_metadata)?;
    w.write_event(Event::End(BytesEnd::new("invoiceHead")))?;

    // <invoiceLines>
    write_lines(&mut w, &invoice.lines, currency, rate_metadata)?;

    // <invoiceSummary>
    write_summary(&mut w, &invoice.lines, currency, rate_metadata)?;

    w.write_event(Event::End(BytesEnd::new("invoice")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceMain")))?;
    w.write_event(Event::End(BytesEnd::new("InvoiceData")))?;

    Ok(buf)
}

/// Refuse a non-HUF currency without rate metadata; pinned by the
/// `eur_render_without_rate_metadata_loud_fails` test. The mirror of
/// the issuance-side ADR-0037 §4 invariant C1 check that PR-44γ
/// enforces in `aberp_billing::allocate_in_tx`'s pre-flight.
fn ensure_rate_metadata_invariant(
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    match (currency, rate_metadata) {
        (Currency::Huf, _) => Ok(()),
        (_, Some(_)) => Ok(()),
        (other, None) => Err(anyhow!(
            "non-HUF invoice currency {} requires rate_metadata at NAV body render time \
             (ADR-0037 §4 invariant C1, NAV-body side)",
            other.iso_code()
        )),
    }
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
    // PR-44δ — storno chain stays HUF-only at this PR per the session-51
    // narrowed scope; PR-44γ.1 lifts the chain to currency-aware.
    write_invoice_detail(&mut w, &issue_date, Currency::Huf, None)?;
    w.write_event(Event::End(BytesEnd::new("invoiceHead")))?;

    // <invoiceLines> with negated amounts. Negate by constructing a
    // parallel Vec with negated unit_price; net/vat/gross cascade
    // through `LineItem::net_total` etc. unchanged.
    let negated_lines: Vec<LineItem> = invoice.lines.iter().map(negate_line).collect();
    write_lines(&mut w, &negated_lines, Currency::Huf, None)?;
    write_summary(&mut w, &negated_lines, Currency::Huf, None)?;

    w.write_event(Event::End(BytesEnd::new("invoice")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceMain")))?;
    w.write_event(Event::End(BytesEnd::new("InvoiceData")))?;

    Ok(buf)
}

/// Render the modification's `<InvoiceData>` to bytes (PR-11,
/// ADR-0024).
///
/// Structurally parallel to [`render_storno_data`] with two
/// differences that follow from ADR-0024 §3 + §4:
///
/// 1. The `<invoiceReference>` block carries the MODIFY-shape
///    children (an extra `<modificationIssueDate>` between
///    `<originalInvoiceNumber>` and `<modifyWithoutMaster>` per
///    ADR-0024 §1 conflict 1). The discriminator for the wire
///    operation (CREATE vs STORNO vs MODIFY) lives in
///    `submit_invoice::detect_operation_from_xml` (ADR-0024 §3); the
///    presence of `<modificationIssueDate>` is what flips the body
///    from STORNO-shape to MODIFY-shape.
///
/// 2. Line and summary amounts are **NOT negated.** The modification
///    is a **full-replace** body per ADR-0024 §4 — it carries the
///    new effective invoice values, not a delta. The line writers are
///    reused against the input invoice's lines directly, so this
///    function shares `write_lines` / `write_summary` with
///    [`render_invoice_data`] (and, by happenstance, with
///    [`render_storno_data`] via that storno function's negated
///    parallel `Vec`).
///
/// The `invoice` argument carries the MODIFICATION's own sequence
/// number (the modification is itself an invoice with its own
/// allocator slot per ADR-0009 §6 + ADR-0024 §5);
/// `modification_reference.base_invoice_number` names what is being
/// corrected.
pub fn render_modification_data(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    modification_reference: &ModificationReference,
) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("XML declaration")?;

    let mut root = BytesStart::new("InvoiceData");
    root.push_attribute(("xmlns", NAV_NS_DATA));
    root.push_attribute(("xmlns:common", NAV_NS_BASE));
    w.write_event(Event::Start(root))
        .context("write <InvoiceData> (modification)")?;

    // Modification's OWN invoice number — the correction is itself an invoice.
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

    // <invoiceReference> — MODIFY-shape. Position: direct child of
    // <invoice>, BEFORE <invoiceHead>, per NAV v3.0 schema (same
    // position as the STORNO block).
    write_modification_reference(&mut w, modification_reference)?;

    // <invoiceHead> reuses the standard supplier/customer/detail
    // section writers — same posture as the STORNO emitter; party +
    // detail data is the modification's own (corrected) values.
    w.write_event(Event::Start(BytesStart::new("invoiceHead")))?;
    write_supplier(&mut w, &parties.supplier)?;
    write_customer(&mut w, &parties.customer)?;
    // PR-44δ — modification chain stays HUF-only at this PR per the
    // session-51 narrowed scope; PR-44γ.1 lifts the chain to
    // currency-aware.
    write_invoice_detail(&mut w, &issue_date, Currency::Huf, None)?;
    w.write_event(Event::End(BytesEnd::new("invoiceHead")))?;

    // <invoiceLines> + <invoiceSummary> — NOT negated. Full-replace
    // per ADR-0024 §4; the modification's `invoice.lines` already
    // carry the new effective values.
    write_lines(&mut w, &invoice.lines, Currency::Huf, None)?;
    write_summary(&mut w, &invoice.lines, Currency::Huf, None)?;

    w.write_event(Event::End(BytesEnd::new("invoice")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceMain")))?;
    w.write_event(Event::End(BytesEnd::new("InvoiceData")))?;

    Ok(buf)
}

/// Render `<InvoiceAnnulment>` to bytes (PR-12, ADR-0025).
///
/// **Structurally distinct** from [`render_invoice_data`] /
/// [`render_storno_data`] / [`render_modification_data`]:
///
/// - **Different root element + namespace.** Root is
///   `<InvoiceAnnulment>` in the `OSA/3.0/annul` namespace (the
///   `manageAnnulment` endpoint's body shape) per ADR-0025 §
///   "Surfaced conflict 1". Verification deferred to first NAV-
///   testbed annulment POST.
/// - **No `<invoiceMain>` / `<invoiceHead>` / lines / summary.** A
///   technical annulment is NOT itself an invoice; it carries only
///   the four metadata fields (reference + timestamp + code +
///   reason).
/// - **`<annulmentTimestamp>` is server-clock-only.** Per ADR-0025
///   §4 — annulment timestamp is a technical not legal field; no
///   operator-supplied date arg. Captured at render time as ISO 8601
///   UTC (`YYYY-MM-DDTHH:MM:SSZ`). If NAV's testbed requires the
///   compressed `YYYYMMDDhhmmss` form (which is what
///   `requestTimestamp` uses in the SOAP header), the change is a
///   one-line formatter swap and the wire shape per ADR-0025 §
///   "Open questions" is the named trigger.
///
/// The `annulment_reference` argument carries the BASE invoice's
/// NAV-facing number (the thing being annulled), the wire-form
/// annulment code, and the operator's reason text. The caller is
/// responsible for building `base_invoice_number` the same way
/// `issue_storno.rs` / `issue_modification.rs` does.
pub fn render_annulment_data(annulment_reference: &AnnulmentReference) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("XML declaration")?;

    let mut root = BytesStart::new("InvoiceAnnulment");
    root.push_attribute(("xmlns", NAV_NS_ANNUL));
    root.push_attribute(("xmlns:common", NAV_NS_BASE));
    w.write_event(Event::Start(root))
        .context("write <InvoiceAnnulment>")?;

    text_element(
        &mut w,
        "annulmentReference",
        &annulment_reference.base_invoice_number,
    )?;
    // Server-clock-only timestamp per ADR-0025 §4. ISO 8601 UTC
    // (`YYYY-MM-DDTHH:MM:SSZ`). Formatted manually rather than
    // depending on `time::Iso8601`'s const-generic configuration —
    // same posture as `render_invoice_data`'s manual date format.
    let now = time::OffsetDateTime::now_utc();
    let timestamp = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    );
    text_element(&mut w, "annulmentTimestamp", &timestamp)?;
    text_element(
        &mut w,
        "annulmentCode",
        annulment_reference.annulment_code,
    )?;
    text_element(&mut w, "annulmentReason", &annulment_reference.reason)?;

    w.write_event(Event::End(BytesEnd::new("InvoiceAnnulment")))?;

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

/// Write the STORNO `<invoiceReference>` chain-link block. PR-10
/// always emits `modifyWithoutMaster=false`: ADR-0023 §4 names the
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

/// Write the MODIFY `<invoiceReference>` chain-link block (PR-11,
/// ADR-0024). Same shape as [`write_invoice_reference`] PLUS the
/// MODIFY-required `<modificationIssueDate>` element positioned
/// between `<originalInvoiceNumber>` and `<modifyWithoutMaster>` per
/// ADR-0024 §1 conflict 1.
///
/// **Not extracted into a shared helper with
/// [`write_invoice_reference`]** despite the heavy overlap — the two
/// blocks have different required-child sets (STORNO: three required;
/// MODIFY: same three plus one MODIFY-only required-by-NAV but
/// optional-from-validator's perspective). A shared helper taking an
/// `Option<&str>` for the modification date would couple the two
/// shapes; CLAUDE.md rule 2 (no speculative abstractions) — keep the
/// two parallel writers honest. If a third chain-shape ever appears
/// (it does not today — technical annulment uses a different
/// endpoint), the trigger to extract is named in ADR-0024 §7.
///
/// Same `modifyWithoutMaster=false` pin as STORNO; the migrated-from-
/// Billingo path that would set this `true` is deferred symmetrically
/// per ADR-0024 §7 / F23.
fn write_modification_reference(
    w: &mut Writer<&mut Vec<u8>>,
    modification_reference: &ModificationReference,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("invoiceReference")))?;
    text_element(
        w,
        "originalInvoiceNumber",
        &modification_reference.base_invoice_number,
    )?;
    text_element(
        w,
        "modificationIssueDate",
        &modification_reference.modification_issue_date,
    )?;
    text_element(w, "modifyWithoutMaster", "false")?;
    text_element(
        w,
        "modificationIndex",
        &modification_reference.modification_index.to_string(),
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

fn write_invoice_detail(
    w: &mut Writer<&mut Vec<u8>>,
    issue_date: &str,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    // ADR-0037 §1.b — `<currencyCode>` is the ISO 4217 code; `<exchangeRate>`
    // is the rate at exactly 6 decimal places per the NAV `Online Számla`
    // XSD (confirmed 2026-05-23 legal cleanup). For HUF the conceptual rate
    // is 1 (HUF-per-HUF); we serialize the same 6-decimal form
    // (`1.000000`) so the C11 precision invariant holds uniformly across
    // HUF and EUR. The validator accepts both forms (`ensure_numeric_amount`
    // is shape-agnostic on decimal-places); the uniform precision is the
    // load-bearing posture pin.
    let exchange_rate = match (currency, rate_metadata) {
        (Currency::Huf, _) => "1.000000".to_string(),
        (_, Some(meta)) => format_rate_six_decimals(&meta.rate),
        (_, None) => unreachable!("ensure_rate_metadata_invariant ruled this out"),
    };
    w.write_event(Event::Start(BytesStart::new("invoiceDetail")))?;
    text_element(w, "invoiceCategory", "NORMAL")?;
    text_element(w, "invoiceDeliveryDate", issue_date)?;
    text_element(w, "currencyCode", currency.iso_code())?;
    text_element(w, "exchangeRate", &exchange_rate)?;
    text_element(w, "paymentMethod", "TRANSFER")?;
    text_element(w, "paymentDate", issue_date)?;
    text_element(w, "invoiceAppearance", "ELECTRONIC")?;
    w.write_event(Event::End(BytesEnd::new("invoiceDetail")))?;
    Ok(())
}

/// Serialize a `rust_decimal::Decimal` rate at exactly 6 decimal places
/// per ADR-0037 §1.c + §4 invariant C11. `Decimal`'s `Display` impl
/// honours the precision specifier (`{:.6}`) and pads with trailing
/// zeros — exactly what the NAV XSD `decimal(6)` shape requires. Pinned
/// by `rate_serializes_at_six_decimals` in the round-trip test file.
fn format_rate_six_decimals(rate: &Decimal) -> String {
    format!("{:.6}", rate)
}

/// Format an `i64` of minor units (EUR cents) as a two-decimal EUR
/// amount string. Used by [`write_lines`] / [`write_summary`] on the
/// EUR branch — the wire body's `lineNetAmount` / `vatRateNetAmount` /
/// `invoiceNetAmount` etc. carry the native-currency amount; for EUR
/// that is `cents / 100` with two decimal places. Negative amounts
/// (storno chain children, future) prepend a single `-`.
fn format_minor_units_two_decimals(minor_units: i64) -> String {
    let sign = if minor_units < 0 { "-" } else { "" };
    let abs = minor_units.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// HUF equivalent of an invoice-currency minor-unit amount under the
/// supplied rate-metadata. For `Currency::Huf` the amount is already in
/// whole forints — no conversion. For non-HUF currencies the amount is
/// in cents and we apply [`huf_equivalent_round_half_even`] using the
/// PR-44γ-stamped rate (read from the persisted `RateMetadata`, NOT
/// re-fetched) per ADR-0037 §1.c + §4 invariant C11.
fn huf_equivalent_for(
    minor_units: i64,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<i64> {
    match (currency, rate_metadata) {
        (Currency::Huf, _) => Ok(minor_units),
        (_, Some(meta)) => huf_equivalent_round_half_even(minor_units, &meta.rate).ok_or_else(
            || {
                anyhow!(
                    "HUF-equivalent conversion overflowed i64 for {} cents at rate {}",
                    minor_units,
                    meta.rate
                )
            },
        ),
        (other, None) => Err(anyhow!(
            "non-HUF currency {} requires rate_metadata at HUF-equivalent computation",
            other.iso_code()
        )),
    }
}

/// Serialize a per-line / per-VAT-rate / invoice-level amount in its
/// native currency: integer forints for HUF; two-decimal EUR cents for
/// EUR. The two branches diverge at the wire boundary; the underlying
/// `i64` carries minor units in both cases (the EUR amount lives inside
/// a `Huf(cents)` wrapper today per PR-44γ's interim posture — see
/// `apps/aberp/src/issue_invoice.rs::finalize_rate`).
fn format_native_amount(minor_units: i64, currency: Currency) -> String {
    match currency {
        Currency::Huf => minor_units.to_string(),
        _ => format_minor_units_two_decimals(minor_units),
    }
}

fn write_lines(
    w: &mut Writer<&mut Vec<u8>>,
    lines: &[LineItem],
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
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
        text_element(
            w,
            "unitPrice",
            &format_native_amount(line.unit_price.as_i64(), currency),
        )?;

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
        write_line_net(w, net, currency, rate_metadata)?;
        write_line_vat_rate(w, line.vat_rate_basis_points)?;
        write_line_vat_amount(w, vat, currency, rate_metadata)?;
        write_line_gross(w, gross, currency, rate_metadata)?;
        w.write_event(Event::End(BytesEnd::new("lineAmountsNormal")))?;

        w.write_event(Event::End(BytesEnd::new("line")))?;
    }
    w.write_event(Event::End(BytesEnd::new("invoiceLines")))?;
    Ok(())
}

fn write_line_net(
    w: &mut Writer<&mut Vec<u8>>,
    net: Huf,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    let huf = huf_equivalent_for(net.as_i64(), currency, rate_metadata)?;
    w.write_event(Event::Start(BytesStart::new("lineNetAmountData")))?;
    text_element(w, "lineNetAmount", &format_native_amount(net.as_i64(), currency))?;
    text_element(w, "lineNetAmountHUF", &huf.to_string())?;
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

fn write_line_vat_amount(
    w: &mut Writer<&mut Vec<u8>>,
    vat: Huf,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    let huf = huf_equivalent_for(vat.as_i64(), currency, rate_metadata)?;
    w.write_event(Event::Start(BytesStart::new("lineVatData")))?;
    text_element(w, "lineVatAmount", &format_native_amount(vat.as_i64(), currency))?;
    text_element(w, "lineVatAmountHUF", &huf.to_string())?;
    w.write_event(Event::End(BytesEnd::new("lineVatData")))?;
    Ok(())
}

fn write_line_gross(
    w: &mut Writer<&mut Vec<u8>>,
    gross: Huf,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    let huf = huf_equivalent_for(gross.as_i64(), currency, rate_metadata)?;
    w.write_event(Event::Start(BytesStart::new("lineGrossAmountData")))?;
    text_element(
        w,
        "lineGrossAmountNormal",
        &format_native_amount(gross.as_i64(), currency),
    )?;
    text_element(w, "lineGrossAmountNormalHUF", &huf.to_string())?;
    w.write_event(Event::End(BytesEnd::new("lineGrossAmountData")))?;
    Ok(())
}

fn write_summary(
    w: &mut Writer<&mut Vec<u8>>,
    lines: &[LineItem],
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
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

    // PR-44δ — ADR-0037 §1.c invoice-level HUF totals are the sum of
    // per-VAT-rate HUF amounts (NOT a fresh round_half_even on the
    // EUR invoice total). PR-5's single-`summaryByVatRate` posture
    // collapses everything into one VAT rate today; when a future PR
    // extends to multi-rate invoices the per-rate HUF amounts here
    // need to be summed before serializing the invoice-level *HUF.
    let net_total_huf = huf_equivalent_for(net_total.as_i64(), currency, rate_metadata)?;
    let vat_total_huf = huf_equivalent_for(vat_total.as_i64(), currency, rate_metadata)?;
    let gross_total_huf = huf_equivalent_for(gross_total.as_i64(), currency, rate_metadata)?;

    w.write_event(Event::Start(BytesStart::new("invoiceSummary")))?;
    w.write_event(Event::Start(BytesStart::new("summaryNormal")))?;
    // summaryByVatRate (one entry, assuming all lines share a rate; PR-5
    // does not yet group by rate — a follow-up extends this for
    // multi-rate invoices).
    if let Some(first) = lines.first() {
        w.write_event(Event::Start(BytesStart::new("summaryByVatRate")))?;
        write_line_vat_rate(w, first.vat_rate_basis_points)?;
        w.write_event(Event::Start(BytesStart::new("vatRateNetData")))?;
        text_element(
            w,
            "vatRateNetAmount",
            &format_native_amount(net_total.as_i64(), currency),
        )?;
        text_element(w, "vatRateNetAmountHUF", &net_total_huf.to_string())?;
        w.write_event(Event::End(BytesEnd::new("vatRateNetData")))?;
        w.write_event(Event::Start(BytesStart::new("vatRateVatData")))?;
        text_element(
            w,
            "vatRateVatAmount",
            &format_native_amount(vat_total.as_i64(), currency),
        )?;
        text_element(w, "vatRateVatAmountHUF", &vat_total_huf.to_string())?;
        w.write_event(Event::End(BytesEnd::new("vatRateVatData")))?;
        w.write_event(Event::Start(BytesStart::new("vatRateGrossData")))?;
        text_element(
            w,
            "vatRateGrossAmount",
            &format_native_amount(gross_total.as_i64(), currency),
        )?;
        text_element(w, "vatRateGrossAmountHUF", &gross_total_huf.to_string())?;
        w.write_event(Event::End(BytesEnd::new("vatRateGrossData")))?;
        w.write_event(Event::End(BytesEnd::new("summaryByVatRate")))?;
    }
    text_element(
        w,
        "invoiceNetAmount",
        &format_native_amount(net_total.as_i64(), currency),
    )?;
    text_element(w, "invoiceNetAmountHUF", &net_total_huf.to_string())?;
    text_element(
        w,
        "invoiceVatAmount",
        &format_native_amount(vat_total.as_i64(), currency),
    )?;
    text_element(w, "invoiceVatAmountHUF", &vat_total_huf.to_string())?;
    w.write_event(Event::End(BytesEnd::new("summaryNormal")))?;
    w.write_event(Event::Start(BytesStart::new("summaryGrossData")))?;
    text_element(
        w,
        "invoiceGrossAmount",
        &format_native_amount(gross_total.as_i64(), currency),
    )?;
    text_element(w, "invoiceGrossAmountHUF", &gross_total_huf.to_string())?;
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
