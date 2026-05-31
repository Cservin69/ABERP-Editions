//! NAV `queryInvoiceDigest` operation — S178 / PR-178.
//!
//! Paginated digest list of invoices NAV holds for a tenant,
//! filtered by issue-date range + direction. The AP-side
//! auto-sync daemon (`apps/aberp/src/ap_sync.rs`) calls it with
//! `InvoiceDirection::Inbound` to discover supplier-issued
//! invoices for ABERP to mirror locally.
//!
//! # Flow (mirror of `super::query_invoice_data::call`)
//!
//!   1. Render the `<QueryInvoiceDigestRequest>` envelope via
//!      [`crate::soap::render_query_invoice_digest_request`].
//!      Non-`manageInvoice` request signature per ADR-0009 §4.
//!   2. POST to `<endpoint base url>/queryInvoiceDigest`.
//!   3. Capture the response body verbatim BEFORE parsing
//!      (ADR-0009 §8) — even though no audit-payload pins these
//!      bytes today, the verbatim capture protects against a
//!      parser bug eating evidence the operator may later
//!      inspect via the `aberp` traces.
//!   4. On non-success HTTP status: loud-fail.
//!   5. Parse `<common:result>`. On `ERROR`, classify per
//!      [`super::is_non_retryable`].
//!   6. On `OK`, walk `<invoiceDigest>` blocks and collect them
//!      into a typed [`InvoiceDigest`] vector. Return
//!      [`QueryInvoiceDigestPage`] with the page's digests +
//!      pagination metadata.
//!
//! # What this module deliberately does NOT do
//!
//!   - It does NOT loop over pages. Pagination is the caller's
//!     concern (the AP-sync daemon iterates with a safety cap).
//!   - It does NOT consume an `exchangeToken`. NAV *query*
//!     operations authenticate via the per-request `<user>` block.
//!   - It does NOT fetch the full invoice XML — that is what
//!     `query_invoice_data` is for. The AP-sync daemon calls
//!     this operation to enumerate, then per-digest calls
//!     `query_invoice_data` to fetch the full bytes.
//!   - It does NOT parse the full XSD shape — only the fields
//!     the AP-sync daemon needs for dedup + display. NAV adds
//!     occasional new fields; missing them surfaces as `None`
//!     on the typed struct rather than a parse failure.

use crate::credentials::NavCredentials;
use crate::error::NavTransportError;
use crate::soap::{self, InvoiceDirection};
use crate::NavTransport;

use quick_xml::events::Event;
use quick_xml::Reader;

use super::{is_non_retryable, parse_result_block, NavResultBlock};

/// One row of NAV's digest response — the fields the AP-side
/// auto-sync daemon uses to (a) decide whether it already has
/// the invoice (by `(supplier_tax_number, invoice_number)`),
/// and (b) fetch the full XML via
/// [`super::query_invoice_data`] when needed.
///
/// NAV's actual `<invoiceDigest>` XSD names many more fields
/// (insertion date, totals in HUF, payment date, etc.). The
/// daemon only NEEDS the four below for dedup + the fifth for
/// the per-digest follow-up `queryInvoiceData` call. Adding
/// fields later is additive — a future contributor extending
/// this struct only needs to add a parse arm in
/// [`parse_digest_page`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceDigest {
    /// `<invoiceNumber>` — the supplier's invoice number, used
    /// as half of the dedup key against the `ap_invoice` table.
    pub invoice_number: String,
    /// `<supplierTaxNumber>` — the 8-digit tax number head.
    /// The other half of the dedup key.
    pub supplier_tax_number: String,
    /// `<supplierName>` — the supplier's legal name as NAV
    /// holds it. Stamped on the ingested row's `supplier_name`
    /// column.
    pub supplier_name: Option<String>,
    /// `<invoiceIssueDate>` — `YYYY-MM-DD` per NAV v3.0.
    pub issue_date: Option<String>,
    /// `<transactionId>` — the NAV-side tracking id of the
    /// original supplier submission. Capturing it makes a future
    /// "fetch by transactionId for evidence parity" path trivial
    /// (the AP-sync daemon does not need it today — the digest
    /// alone has the typed fields the operator wants to see).
    pub transaction_id: Option<String>,
    /// `<currency>` — ISO 4217 code as NAV holds it. The
    /// AP-sync daemon enforces the closed vocab (`HUF` / `EUR`
    /// per `ap_invoice.currency`) at ingest time; unknown
    /// codes loud-fail rather than coerce to a default.
    pub currency: Option<String>,
    /// `<invoiceNetAmount>` — sum of net amounts in the invoice
    /// currency, as the verbatim NAV string (e.g. `"12345.67"`).
    /// String-typed to avoid floating-point coercion at the parser
    /// boundary; the AP-sync daemon parses it through `Decimal`
    /// then converts to minor units for `ap_invoice.total_net_minor`.
    pub invoice_net_amount: Option<String>,
    /// `<invoiceVatAmount>` — sum of VAT amounts in the invoice
    /// currency. Same string-then-Decimal posture as `invoice_net_amount`.
    pub invoice_vat_amount: Option<String>,
}

/// One page of digest results. The AP-sync daemon iterates from
/// `page = 1` until `current_page >= available_page` (or until
/// the safety cap fires).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryInvoiceDigestPage {
    pub current_page: u32,
    pub available_page: u32,
    /// NAV's published OSA 3.0 `InvoiceDigestResultType` does NOT
    /// declare `<availableLine>`; real prod responses omit it
    /// (session-189 bug). The parser defaults to 0 when absent
    /// rather than loud-failing. No code reads this today —
    /// kept as the verbatim value NAV sends if a future schema
    /// revision adds the element back. **Do not turn this into
    /// an `Option<u64>`** without first re-verifying no callers
    /// rely on the u64 typing.
    pub available_line: u64,
    pub digests: Vec<InvoiceDigest>,
}

/// Call `queryInvoiceDigest` against `transport` for ONE page.
/// The caller paginates by re-calling with successive `page`
/// values until `current_page >= available_page`.
///
/// `date_from` / `date_to` are `YYYY-MM-DD` strings. NAV's cap
/// is 35 days per request; the AP-sync daemon passes 30 to
/// leave operator margin (the conservative-flag from S178's
/// brief).
pub async fn call(
    transport: &NavTransport,
    credentials: &NavCredentials,
    tax_number_8: &str,
    page: u32,
    invoice_direction: InvoiceDirection,
    date_from: &str,
    date_to: &str,
) -> Result<QueryInvoiceDigestPage, NavTransportError> {
    let request_id = soap::parts::new_request_id();
    let request_timestamp = soap::parts::request_timestamp(time::OffsetDateTime::now_utc())?;

    let request_xml = soap::render_query_invoice_digest_request(
        credentials,
        tax_number_8,
        &request_id,
        &request_timestamp,
        page,
        invoice_direction,
        date_from,
        date_to,
    )?;

    let url = format!("{}queryInvoiceDigest", transport.endpoint().base_url());

    let response = transport
        .client()
        .post(&url)
        .header("Content-Type", "application/xml")
        .header("Accept", "application/xml")
        .body(request_xml)
        .send()
        .await
        .map_err(NavTransportError::QueryInvoiceDigestHttp)?;

    let status = response.status();
    let response_xml = response
        .bytes()
        .await
        .map_err(NavTransportError::QueryInvoiceDigestHttp)?
        .to_vec();

    if !status.is_success() {
        return Err(NavTransportError::QueryInvoiceDigestHttpStatus {
            status: status.as_u16(),
        });
    }

    match parse_result_block(
        &response_xml,
        NavTransportError::QueryInvoiceDigestResponseParse,
    )? {
        NavResultBlock::Ok => {}
        NavResultBlock::Error { code, message } => {
            if is_non_retryable(&code) {
                return Err(NavTransportError::QueryInvoiceDigestNonRetryable { code, message });
            }
            return Err(NavTransportError::QueryInvoiceDigestRetryable { code, message });
        }
    }

    parse_digest_page(&response_xml)
}

/// Walk `<QueryInvoiceDigestResponse>` and collect every
/// `<invoiceDigest>` block into [`InvoiceDigest`] entries, plus
/// extract the three pagination scalars from `<invoiceDigestResult>`.
///
/// Namespace-blind local-name match per the same convention
/// [`super::find_all_technical_validations`] uses. Direct-child
/// text only — text inside grandchildren does not pollute the
/// digest's typed fields (defence against a future NAV schema
/// extension that nests one of the named child elements).
pub(crate) fn parse_digest_page(xml: &[u8]) -> Result<QueryInvoiceDigestPage, NavTransportError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut current_page: Option<u32> = None;
    let mut available_page: Option<u32> = None;
    let mut available_line: Option<u64> = None;
    let mut digests: Vec<InvoiceDigest> = Vec::new();

    // Walker state:
    //   block_depth == 0: outside any <invoiceDigest>.
    //   block_depth == 1: directly inside <invoiceDigest>.
    //   block_depth >= 2: inside a sub-element collecting text.
    // We also track whether we are inside <invoiceDigestResult>'s
    // direct children for the three pagination scalars; same
    // namespace-blind local-name match.
    let mut block_depth: u32 = 0;
    let mut current = InvoiceDigest::default();
    let mut active_sub: Option<DigestField> = None;
    let mut active_result_sub: Option<ResultField> = None;
    let mut in_result_block: bool = false;
    let mut in_result_sub_depth: u32 = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qualified = e.name();
                let qualified = qualified.as_ref();
                // <invoiceDigest> open at any depth — start a fresh row.
                if block_depth == 0 && local_name_matches(qualified, "invoiceDigest") {
                    block_depth = 1;
                    current = InvoiceDigest::default();
                    active_sub = None;
                } else if block_depth == 1 {
                    block_depth = 2;
                    active_sub = digest_field_for(qualified);
                } else if block_depth >= 2 {
                    block_depth += 1;
                }

                // Pagination scalars: <invoiceDigestResult> direct
                // children. Tracked separately from the digest walker
                // so the two streams of children do not interleave.
                if !in_result_block && local_name_matches(qualified, "invoiceDigestResult") {
                    in_result_block = true;
                } else if in_result_block && in_result_sub_depth == 0 {
                    active_result_sub = result_field_for(qualified);
                    in_result_sub_depth = 1;
                } else if in_result_block {
                    in_result_sub_depth += 1;
                }
            }
            Ok(Event::End(e)) => {
                let qualified = e.name();
                let qualified = qualified.as_ref();
                if block_depth == 1 && local_name_matches(qualified, "invoiceDigest") {
                    digests.push(std::mem::take(&mut current));
                    block_depth = 0;
                    active_sub = None;
                } else if block_depth >= 2 {
                    block_depth -= 1;
                    if block_depth == 1 {
                        active_sub = None;
                    }
                }
                if in_result_block && local_name_matches(qualified, "invoiceDigestResult") {
                    in_result_block = false;
                    active_result_sub = None;
                    in_result_sub_depth = 0;
                } else if in_result_block && in_result_sub_depth > 0 {
                    in_result_sub_depth -= 1;
                    if in_result_sub_depth == 0 {
                        active_result_sub = None;
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if block_depth == 2 {
                    if let Some(field) = active_sub {
                        let s = t
                            .unescape()
                            .map_err(|e| {
                                NavTransportError::QueryInvoiceDigestResponseParse(format!(
                                    "XML text unescape failed: {e}"
                                ))
                            })?
                            .into_owned();
                        assign_digest_field(&mut current, field, s);
                    }
                }
                if in_result_block && in_result_sub_depth == 1 {
                    if let Some(field) = active_result_sub {
                        let s = t
                            .unescape()
                            .map_err(|e| {
                                NavTransportError::QueryInvoiceDigestResponseParse(format!(
                                    "XML text unescape failed: {e}"
                                ))
                            })?
                            .into_owned();
                        assign_result_field(
                            &mut current_page,
                            &mut available_page,
                            &mut available_line,
                            field,
                            &s,
                        )?;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(NavTransportError::QueryInvoiceDigestResponseParse(format!(
                    "XML parse failed at position {}: {e}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
        buf.clear();
    }

    // Pagination posture, pinned against the published NAV OSA 3.0
    // `InvoiceDigestResultType` (`invoiceApi.xsd`):
    //
    //   - `<currentPage>` and `<availablePage>` are REQUIRED in the
    //     XSD (no `minOccurs="0"`). A response missing either is
    //     schema drift and loud-fails per CLAUDE.md rule 12 —
    //     silently defaulting to 1 would let the daemon think it
    //     had exhausted pagination after page 1 and silently drop
    //     pages 2..N.
    //   - `<availableLine>` is NOT in the NAV XSD at all (the field
    //     was carried into this parser from an earlier draft, never
    //     emitted by NAV). The session-189 prod bug was the
    //     `.ok_or(...)` here firing on every empty-INBOUND tenant
    //     boot. Default to 0 when absent; if NAV ever does emit it
    //     in a future schema revision, we still capture the value.
    let current_page = current_page.ok_or_else(|| {
        NavTransportError::QueryInvoiceDigestResponseParse(
            "queryInvoiceDigest response missing <currentPage>".to_string(),
        )
    })?;
    let available_page = available_page.ok_or_else(|| {
        NavTransportError::QueryInvoiceDigestResponseParse(
            "queryInvoiceDigest response missing <availablePage>".to_string(),
        )
    })?;
    let available_line = available_line.unwrap_or(0);

    // A digest with empty `invoice_number` or `supplier_tax_number`
    // is unusable for the AP-sync daemon's dedup key. Loud-fail per
    // CLAUDE.md rule 12 — silent skip would mask a NAV schema drift.
    for d in &digests {
        if d.invoice_number.is_empty() {
            return Err(NavTransportError::QueryInvoiceDigestResponseParse(
                "queryInvoiceDigest entry missing <invoiceNumber>".to_string(),
            ));
        }
        if d.supplier_tax_number.is_empty() {
            return Err(NavTransportError::QueryInvoiceDigestResponseParse(
                "queryInvoiceDigest entry missing <supplierTaxNumber>".to_string(),
            ));
        }
    }

    Ok(QueryInvoiceDigestPage {
        current_page,
        available_page,
        available_line,
        digests,
    })
}

impl Default for InvoiceDigest {
    fn default() -> Self {
        Self {
            invoice_number: String::new(),
            supplier_tax_number: String::new(),
            supplier_name: None,
            issue_date: None,
            transaction_id: None,
            currency: None,
            invoice_net_amount: None,
            invoice_vat_amount: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DigestField {
    InvoiceNumber,
    SupplierTaxNumber,
    SupplierName,
    IssueDate,
    TransactionId,
    Currency,
    InvoiceNetAmount,
    InvoiceVatAmount,
}

#[derive(Debug, Clone, Copy)]
enum ResultField {
    CurrentPage,
    AvailablePage,
    AvailableLine,
}

fn digest_field_for(qualified: &[u8]) -> Option<DigestField> {
    if local_name_matches(qualified, "invoiceNumber") {
        Some(DigestField::InvoiceNumber)
    } else if local_name_matches(qualified, "supplierTaxNumber") {
        Some(DigestField::SupplierTaxNumber)
    } else if local_name_matches(qualified, "supplierName") {
        Some(DigestField::SupplierName)
    } else if local_name_matches(qualified, "invoiceIssueDate") {
        Some(DigestField::IssueDate)
    } else if local_name_matches(qualified, "transactionId") {
        Some(DigestField::TransactionId)
    } else if local_name_matches(qualified, "currency") {
        Some(DigestField::Currency)
    } else if local_name_matches(qualified, "invoiceNetAmount") {
        Some(DigestField::InvoiceNetAmount)
    } else if local_name_matches(qualified, "invoiceVatAmount") {
        Some(DigestField::InvoiceVatAmount)
    } else {
        None
    }
}

fn result_field_for(qualified: &[u8]) -> Option<ResultField> {
    if local_name_matches(qualified, "currentPage") {
        Some(ResultField::CurrentPage)
    } else if local_name_matches(qualified, "availablePage") {
        Some(ResultField::AvailablePage)
    } else if local_name_matches(qualified, "availableLine") {
        Some(ResultField::AvailableLine)
    } else {
        None
    }
}

fn assign_digest_field(current: &mut InvoiceDigest, field: DigestField, value: String) {
    match field {
        DigestField::InvoiceNumber => current.invoice_number.push_str(&value),
        DigestField::SupplierTaxNumber => current.supplier_tax_number.push_str(&value),
        DigestField::SupplierName => append_optional(&mut current.supplier_name, value),
        DigestField::IssueDate => append_optional(&mut current.issue_date, value),
        DigestField::TransactionId => append_optional(&mut current.transaction_id, value),
        DigestField::Currency => append_optional(&mut current.currency, value),
        DigestField::InvoiceNetAmount => append_optional(&mut current.invoice_net_amount, value),
        DigestField::InvoiceVatAmount => append_optional(&mut current.invoice_vat_amount, value),
    }
}

fn append_optional(slot: &mut Option<String>, value: String) {
    match slot {
        Some(s) => s.push_str(&value),
        None => *slot = Some(value),
    }
}

fn assign_result_field(
    current_page: &mut Option<u32>,
    available_page: &mut Option<u32>,
    available_line: &mut Option<u64>,
    field: ResultField,
    value: &str,
) -> Result<(), NavTransportError> {
    match field {
        ResultField::CurrentPage => {
            let n: u32 = value.trim().parse().map_err(|e| {
                NavTransportError::QueryInvoiceDigestResponseParse(format!(
                    "queryInvoiceDigest <currentPage> not a u32 (`{value}`): {e}"
                ))
            })?;
            *current_page = Some(n);
        }
        ResultField::AvailablePage => {
            let n: u32 = value.trim().parse().map_err(|e| {
                NavTransportError::QueryInvoiceDigestResponseParse(format!(
                    "queryInvoiceDigest <availablePage> not a u32 (`{value}`): {e}"
                ))
            })?;
            *available_page = Some(n);
        }
        ResultField::AvailableLine => {
            let n: u64 = value.trim().parse().map_err(|e| {
                NavTransportError::QueryInvoiceDigestResponseParse(format!(
                    "queryInvoiceDigest <availableLine> not a u64 (`{value}`): {e}"
                ))
            })?;
            *available_line = Some(n);
        }
    }
    Ok(())
}

/// Local-name match against a qualified element name. Copied from
/// `super::local_name_matches` (which is private to the parent
/// module) — duplicating a 5-line helper avoids widening the
/// crate-internal surface for one extra caller.
fn local_name_matches(qualified: &[u8], target: &str) -> bool {
    let local = match qualified.iter().rposition(|&b| b == b':') {
        Some(i) => &qualified[i + 1..],
        None => qualified,
    };
    local == target.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Happy path: a two-digest fixture parses both rows + the
    /// pagination scalars. Loose-shape verification — namespaces
    /// and elements ordered as NAV's v3.0 doc names them.
    /// Hungarian supplier names round-trip verbatim — NAV's
    /// localized strings must survive the parser losslessly.
    #[test]
    fn parse_digest_page_extracts_two_rows_and_pagination() {
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<QueryInvoiceDigestResponse xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\"\n\
                            xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\">\n\
  <common:result>\n\
    <common:funcCode>OK</common:funcCode>\n\
  </common:result>\n\
  <invoiceDigestResult>\n\
    <currentPage>1</currentPage>\n\
    <availablePage>3</availablePage>\n\
    <availableLine>57</availableLine>\n\
    <invoiceDigest>\n\
      <invoiceNumber>SUP-2026/0001</invoiceNumber>\n\
      <invoiceDirection>INBOUND</invoiceDirection>\n\
      <supplierTaxNumber>12345678</supplierTaxNumber>\n\
      <supplierName>Példa Beszállító Kft.</supplierName>\n\
      <invoiceIssueDate>2026-05-10</invoiceIssueDate>\n\
      <transactionId>TXN-AAA-001</transactionId>\n\
      <currency>HUF</currency>\n\
      <invoiceNetAmount>100000.00</invoiceNetAmount>\n\
      <invoiceVatAmount>27000.00</invoiceVatAmount>\n\
    </invoiceDigest>\n\
    <invoiceDigest>\n\
      <invoiceNumber>SUP-2026/0002</invoiceNumber>\n\
      <invoiceDirection>INBOUND</invoiceDirection>\n\
      <supplierTaxNumber>87654321</supplierTaxNumber>\n\
      <supplierName>Másik Szállító Bt.</supplierName>\n\
      <invoiceIssueDate>2026-05-11</invoiceIssueDate>\n\
      <transactionId>TXN-BBB-002</transactionId>\n\
      <currency>EUR</currency>\n\
      <invoiceNetAmount>50.00</invoiceNetAmount>\n\
      <invoiceVatAmount>13.50</invoiceVatAmount>\n\
    </invoiceDigest>\n\
  </invoiceDigestResult>\n\
</QueryInvoiceDigestResponse>";
        let page = parse_digest_page(body.as_bytes()).expect("parse");
        assert_eq!(page.current_page, 1);
        assert_eq!(page.available_page, 3);
        assert_eq!(page.available_line, 57);
        assert_eq!(page.digests.len(), 2);
        let d0 = &page.digests[0];
        assert_eq!(d0.invoice_number, "SUP-2026/0001");
        assert_eq!(d0.supplier_tax_number, "12345678");
        assert_eq!(d0.supplier_name.as_deref(), Some("Példa Beszállító Kft."));
        assert_eq!(d0.issue_date.as_deref(), Some("2026-05-10"));
        assert_eq!(d0.transaction_id.as_deref(), Some("TXN-AAA-001"));
        assert_eq!(d0.currency.as_deref(), Some("HUF"));
        assert_eq!(d0.invoice_net_amount.as_deref(), Some("100000.00"));
        assert_eq!(d0.invoice_vat_amount.as_deref(), Some("27000.00"));
        let d1 = &page.digests[1];
        assert_eq!(d1.invoice_number, "SUP-2026/0002");
        assert_eq!(d1.supplier_tax_number, "87654321");
        assert_eq!(d1.currency.as_deref(), Some("EUR"));
        assert_eq!(d1.invoice_net_amount.as_deref(), Some("50.00"));
        assert_eq!(d1.invoice_vat_amount.as_deref(), Some("13.50"));
    }

    /// An empty result page (zero invoices in range) parses
    /// cleanly to a page with `digests: []` and zeroed scalars.
    #[test]
    fn parse_digest_page_handles_empty_page() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDigestResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                            xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <invoiceDigestResult>
    <currentPage>1</currentPage>
    <availablePage>1</availablePage>
    <availableLine>0</availableLine>
  </invoiceDigestResult>
</QueryInvoiceDigestResponse>"#;
        let page = parse_digest_page(body).expect("parse");
        assert_eq!(page.current_page, 1);
        assert_eq!(page.available_page, 1);
        assert_eq!(page.available_line, 0);
        assert_eq!(page.digests.len(), 0);
    }

    /// A digest missing `<invoiceNumber>` loud-fails — the AP-sync
    /// daemon needs the dedup key and silent-skip would mask schema
    /// drift per CLAUDE.md rule 12.
    #[test]
    fn parse_digest_page_loud_fails_on_missing_invoice_number() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDigestResponse>
  <invoiceDigestResult>
    <currentPage>1</currentPage>
    <availablePage>1</availablePage>
    <availableLine>1</availableLine>
    <invoiceDigest>
      <supplierTaxNumber>12345678</supplierTaxNumber>
      <supplierName>X Kft.</supplierName>
    </invoiceDigest>
  </invoiceDigestResult>
</QueryInvoiceDigestResponse>"#;
        let err = parse_digest_page(body).expect_err("must loud-fail");
        match err {
            NavTransportError::QueryInvoiceDigestResponseParse(msg) => {
                assert!(msg.contains("invoiceNumber"), "{msg}");
            }
            other => panic!("expected QueryInvoiceDigestResponseParse, got {other:?}"),
        }
    }

    /// `<currentPage>` is REQUIRED per the NAV OSA 3.0 XSD; absence
    /// is schema drift and must loud-fail. Pinned because a silent
    /// 1-default would mask NAV regressing on a required field, and
    /// could let the daemon stall on page 1 forever.
    #[test]
    fn parse_digest_page_loud_fails_on_missing_current_page() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDigestResponse>
  <invoiceDigestResult>
    <availablePage>1</availablePage>
    <invoiceDigest>
      <invoiceNumber>X</invoiceNumber>
      <supplierTaxNumber>1</supplierTaxNumber>
    </invoiceDigest>
  </invoiceDigestResult>
</QueryInvoiceDigestResponse>"#;
        let err = parse_digest_page(body).expect_err("must loud-fail");
        match err {
            NavTransportError::QueryInvoiceDigestResponseParse(msg) => {
                assert!(msg.contains("currentPage"), "{msg}");
            }
            other => panic!("expected QueryInvoiceDigestResponseParse, got {other:?}"),
        }
    }

    /// `<availablePage>` is REQUIRED per the NAV OSA 3.0 XSD; absence
    /// is schema drift and must loud-fail. Pinned because a silent
    /// zero-default would let the daemon think it had exhausted
    /// pagination after page 1 and silently drop pages 2..N.
    #[test]
    fn parse_digest_page_loud_fails_on_missing_available_page() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDigestResponse>
  <invoiceDigestResult>
    <currentPage>1</currentPage>
    <invoiceDigest>
      <invoiceNumber>X</invoiceNumber>
      <supplierTaxNumber>1</supplierTaxNumber>
    </invoiceDigest>
  </invoiceDigestResult>
</QueryInvoiceDigestResponse>"#;
        let err = parse_digest_page(body).expect_err("must loud-fail");
        match err {
            NavTransportError::QueryInvoiceDigestResponseParse(msg) => {
                assert!(msg.contains("availablePage"), "{msg}");
            }
            other => panic!("expected QueryInvoiceDigestResponseParse, got {other:?}"),
        }
    }

    /// **Session-189 prod regression pin.** NAV's real INBOUND digest
    /// response for a tenant with zero supplier-issued invoices in the
    /// window OMITS `<availableLine>` entirely (the element was never
    /// in the official NAV OSA 3.0 XSD — see
    /// https://github.com/nav-gov-hu/Online-Invoice `invoiceApi.xsd`,
    /// `InvoiceDigestResultType`). Before PR-189 this loud-failed at
    /// boot on every empty-INBOUND tenant. After PR-189 the field
    /// defaults to 0 and the daemon proceeds.
    #[test]
    fn parse_digest_page_tolerates_missing_available_line_empty_inbound() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDigestResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                            xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <invoiceDigestResult>
    <currentPage>1</currentPage>
    <availablePage>1</availablePage>
  </invoiceDigestResult>
</QueryInvoiceDigestResponse>"#;
        let page = parse_digest_page(body).expect("empty INBOUND must parse cleanly");
        assert_eq!(page.current_page, 1);
        assert_eq!(page.available_page, 1);
        assert_eq!(
            page.available_line, 0,
            "absent <availableLine> defaults to 0"
        );
        assert!(page.digests.is_empty(), "empty INBOUND has zero digests");
    }

    /// Mirror of the empty-INBOUND fixture but with one real digest
    /// row, still without `<availableLine>`. Pins that the daemon's
    /// dedup path is not blocked by the missing scalar when the
    /// payload otherwise carries usable data — defence-in-depth on
    /// the same XSD-conformance posture as the empty case.
    #[test]
    fn parse_digest_page_tolerates_missing_available_line_with_digest() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDigestResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                            xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <invoiceDigestResult>
    <currentPage>1</currentPage>
    <availablePage>1</availablePage>
    <invoiceDigest>
      <invoiceNumber>SUP-2026/0042</invoiceNumber>
      <invoiceDirection>INBOUND</invoiceDirection>
      <supplierTaxNumber>12345678</supplierTaxNumber>
      <supplierName>Supplier Kft.</supplierName>
      <invoiceIssueDate>2026-05-20</invoiceIssueDate>
      <currency>HUF</currency>
    </invoiceDigest>
  </invoiceDigestResult>
</QueryInvoiceDigestResponse>"#;
        let page = parse_digest_page(body).expect("digest+no-availableLine must parse");
        assert_eq!(page.available_line, 0);
        assert_eq!(page.digests.len(), 1);
        assert_eq!(page.digests[0].invoice_number, "SUP-2026/0042");
        assert_eq!(page.digests[0].supplier_tax_number, "12345678");
    }

    /// Multi-page result: `availablePage=3, currentPage=1`. Pins
    /// that the daemon's pagination loop sees `current < available`
    /// and will continue (the parser surfaces both scalars truthfully;
    /// the daemon's `if page >= available_page` check is the gate).
    #[test]
    fn parse_digest_page_reports_multi_page_truthfully() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDigestResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                            xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <invoiceDigestResult>
    <currentPage>1</currentPage>
    <availablePage>3</availablePage>
    <invoiceDigest>
      <invoiceNumber>SUP-PAGE1-A</invoiceNumber>
      <supplierTaxNumber>11111111</supplierTaxNumber>
    </invoiceDigest>
  </invoiceDigestResult>
</QueryInvoiceDigestResponse>"#;
        let page = parse_digest_page(body).expect("multi-page parse");
        assert_eq!(page.current_page, 1);
        assert_eq!(page.available_page, 3);
        assert!(
            page.current_page < page.available_page,
            "daemon continuation gate must remain `<`"
        );
    }

    /// Malformed XML must loud-fail — defence against the opposite
    /// class of bug. The tolerance added in PR-189 is for `<availableLine>`
    /// only; corrupt bytes are not "tolerated as empty" by the parser.
    #[test]
    fn parse_digest_page_loud_fails_on_malformed_xml() {
        let body = br#"<QueryInvoiceDigestResponse><invoiceDigestResult><currentPage>1</currentPage><availablePage>1</availabl"#;
        let err = parse_digest_page(body).expect_err("corrupt XML must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::QueryInvoiceDigestResponseParse(_)
        ));
    }

    /// Shared retry classification — pin that the new variant routes
    /// through the same `is_non_retryable` set every other operation
    /// uses (defence-in-depth on the shared classifier behaviour).
    #[test]
    fn query_invoice_digest_inherits_shared_retry_classification() {
        assert!(is_non_retryable("INVALID_SECURITY_USER"));
        assert!(is_non_retryable("SCHEMA_VIOLATION"));
        assert!(!is_non_retryable("OPERATION_FAILED"));
    }

    /// Parse-error constructor routes — verifies the `parse_result_block`
    /// constructor lands the malformed-body error in the right
    /// `QueryInvoiceDigest*` variant (defence-in-depth mirror of the
    /// existing per-operation routing pins).
    #[test]
    fn parse_error_block_routes_to_query_invoice_digest_variant_on_malformed() {
        let body = br#"<X><common:result/></X>"#;
        let err = parse_result_block(body, NavTransportError::QueryInvoiceDigestResponseParse)
            .expect_err("missing funcCode must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::QueryInvoiceDigestResponseParse(_)
        ));
    }

    /// S192 — defensive-tolerance pin against a NAV response that
    /// carries an `<availablePage>0</availablePage>` AND non-empty
    /// `<invoiceDigest>` children. The XSD doesn't actually forbid
    /// this combination (the scalars are independent of digest count),
    /// and a corrupt-but-structurally-honest response from a NAV bug
    /// or an upstream proxy could surface this shape. PR-182 review's
    /// S178 🟢 named this as defensive coverage — silently dropping
    /// the digests OR loud-failing the whole batch would both be wrong.
    /// The contract pinned here: surface BOTH the absurd scalar AND
    /// the digest rows verbatim. The pagination terminator at
    /// `apps/aberp/src/ap_sync.rs:363` (`if page >= available_page`)
    /// stops the daemon cleanly after page 1 since `1 >= 0`.
    #[test]
    fn parse_digest_page_accepts_available_page_zero_with_non_empty_digests() {
        // Plain `&str` rather than `br#"..."#` — supplier names carry
        // accented HU characters (Á/é) which are non-ASCII and cannot
        // appear in raw byte-string literals. `.as_bytes()` at the
        // call site keeps the parser surface unchanged.
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDigestResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                            xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <invoiceDigestResult>
    <currentPage>1</currentPage>
    <availablePage>0</availablePage>
    <availableLine>2</availableLine>
    <invoiceDigest>
      <invoiceNumber>SUP-ABSURD/0001</invoiceNumber>
      <supplierTaxNumber>12345678</supplierTaxNumber>
      <supplierName>Példa Kft.</supplierName>
      <invoiceIssueDate>2026-05-15</invoiceIssueDate>
      <currency>HUF</currency>
    </invoiceDigest>
    <invoiceDigest>
      <invoiceNumber>SUP-ABSURD/0002</invoiceNumber>
      <supplierTaxNumber>87654321</supplierTaxNumber>
      <supplierName>Másik Bt.</supplierName>
      <invoiceIssueDate>2026-05-16</invoiceIssueDate>
      <currency>EUR</currency>
    </invoiceDigest>
  </invoiceDigestResult>
</QueryInvoiceDigestResponse>"#
            .as_bytes();
        let page = parse_digest_page(body)
            .expect("availablePage=0 with non-empty digests must parse permissively");
        // Scalar surfaces verbatim — no silent re-clamping to ≥1.
        assert_eq!(page.available_page, 0);
        assert_eq!(page.current_page, 1);
        assert_eq!(page.available_line, 2);
        // BOTH digest rows survive — no silent drop on the absurd scalar.
        assert_eq!(page.digests.len(), 2);
        assert_eq!(page.digests[0].invoice_number, "SUP-ABSURD/0001");
        assert_eq!(page.digests[1].invoice_number, "SUP-ABSURD/0002");
        // The daemon's continuation gate (`page >= available_page`)
        // terminates on the very first tick when `available_page=0`,
        // so this absurd shape is effectively a one-page result. Pin
        // the gate's evaluation here so a future refactor that flips
        // `>=` to `>` does not silently spin the daemon forever.
        let current_page: u32 = page.current_page;
        let available_page: u32 = page.available_page;
        assert!(
            current_page >= available_page,
            "daemon's continuation gate must terminate on availablePage=0; \
             current_page={current_page}, available_page={available_page}"
        );
    }
}
