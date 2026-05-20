//! NAV Online Számla v3.0 SOAP envelope assembly.
//!
//! Hand-rolled with `quick-xml` per ADR-0021 §A8 ("codegen-from-XSD
//! rejected for this scope"). The envelope is a thin wrapper around the
//! `crate::signatures` outputs and the `crate::credentials::NavCredentials`
//! values — there is no DSL, no macros, no derive. The shape this module
//! writes is the same shape NAV's v3.0 XSD accepts; conformance is
//! verified by golden fixtures in `crates/nav-transport/tests/`.
//!
//! # What lives here
//!
//!   - [`render_token_exchange_request`] — full `<TokenExchangeRequest>`
//!     XML body, ready to POST to `/tokenExchange`. PR-7-B-2 consumer.
//!   - [`render_manage_invoice_request`] — full `<ManageInvoiceRequest>`
//!     XML body, ready to POST to `/manageInvoice`. PR-7-B-3 consumer.
//!
//! Lower-level building blocks (header, user, software type, request-id
//! generation, timestamp formatting) live in [`parts`] so the unit tests
//! can exercise them in isolation.
//!
//! # Namespaces (constant per NAV v3.0)
//!
//!   - **API**     `http://schemas.nav.gov.hu/OSA/3.0/api`     — default
//!     namespace on the request root (`<TokenExchangeRequest>`,
//!     `<ManageInvoiceRequest>`, ...).
//!   - **Common**  `http://schemas.nav.gov.hu/NTCA/1.0/common` — prefix
//!     `common`. Carries header, user, software, generic-result.
//!
//! The `InvoiceData` payload itself uses a separate namespace
//! (`http://schemas.nav.gov.hu/OSA/3.0/data`); that is the responsibility
//! of `apps/aberp/src/nav_xml.rs` (PR-5) and is delivered to NAV
//! base64-encoded inside `<invoiceData>`. The SOAP envelope does NOT need
//! the data namespace.
//!
//! # What this module does NOT do
//!
//!   - It does not call NAV (see `crate::operations`).
//!   - It does not parse responses (see `crate::operations`).
//!   - It does not compute signatures (see `crate::signatures`).
//!   - It does not load credentials (see `crate::credentials`).
//!
//! The split keeps each piece unit-testable in isolation. A future XSD
//! validator (deferred per ADR-0021 §Items deferred) plugs in between
//! this module and `operations` — the bytes this module produces are the
//! ones that would be validated.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::credentials::NavCredentials;
use crate::error::NavTransportError;
use crate::signatures::{
    password_hash, request_signature, request_signature_manage, InvoiceSignatureInput,
};

pub mod parts;

/// Default namespace for NAV API request/response envelopes (v3.0).
pub const NAV_NS_API: &str = "http://schemas.nav.gov.hu/OSA/3.0/api";

/// Namespace bound to prefix `common` (header, user, software, results).
pub const NAV_NS_COMMON: &str = "http://schemas.nav.gov.hu/NTCA/1.0/common";

/// One per-invoice operation for `manageInvoice` per ADR-0009 §6.
///
/// Mirrors NAV v3.0's `ManageInvoiceOperationType`. CREATE / MODIFY /
/// STORNO map to the same `manageInvoice` endpoint; `manageAnnulment`
/// uses a separate enum (`ANNUL` only) and a different envelope shape
/// (PR-7-C scope, not exercised by PR-7-B-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceOperation {
    Create,
    Modify,
    Storno,
}

impl InvoiceOperation {
    /// NAV-facing string per the v3.0 XSD enumeration. Returned as
    /// `&'static str` so it can be passed straight to the signature
    /// helpers ([`InvoiceSignatureInput::operation`]) without an
    /// allocation. Order MUST match the per-invoice signature suffix
    /// (`operation || base64(invoiceData)` — the same string lands on
    /// the wire inside `<operation>` and inside the signature input).
    pub fn as_nav_str(self) -> &'static str {
        match self {
            InvoiceOperation::Create => "CREATE",
            InvoiceOperation::Modify => "MODIFY",
            InvoiceOperation::Storno => "STORNO",
        }
    }
}

/// One element of a `manageInvoice` request: an operation + the
/// `<InvoiceData>` XML bytes to submit at this index.
///
/// Index is assigned implicitly by position in the slice the caller
/// passes to [`render_manage_invoice_request`] — NAV requires `<index>`
/// to start at 1 and increment monotonically. The renderer enforces
/// that contract; the caller does not pass indices.
#[derive(Debug, Clone)]
pub struct ManageInvoiceItem<'a> {
    pub operation: InvoiceOperation,
    /// Raw bytes of the `<InvoiceData>` XML element (the output of
    /// `apps/aberp/src/nav_xml.rs::render_invoice_data`). Will be
    /// base64-encoded onto the wire.
    pub invoice_data_xml: &'a [u8],
}

/// Render a `<TokenExchangeRequest>` body, ready for HTTP POST.
///
/// `tax_number` is the 8-digit base of the taxpayer's tax number per
/// ADR-0009 §4 — NOT the dashed full form (`12345678-1-42`). The caller
/// extracts the first 8 digits before calling; passing the dashed form
/// produces `INVALID_SECURITY_USER` from NAV.
///
/// `request_id` and `request_timestamp` are typically minted via
/// [`parts::new_request_id`] and [`parts::request_timestamp`]; the
/// signatures are computed against the same inputs.
pub fn render_token_exchange_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
) -> Result<Vec<u8>, NavTransportError> {
    let signature = request_signature(request_id, request_timestamp, credentials.sign_key_bytes());
    render_request(
        "TokenExchangeRequest",
        credentials,
        tax_number_8,
        request_id,
        request_timestamp,
        &signature,
        |_w| {
            // tokenExchange has no body beyond the common header/user/software
            // blocks — the request itself IS the token request.
            Ok(())
        },
    )
}

/// Render a `<ManageInvoiceRequest>` body, ready for HTTP POST.
///
/// `exchange_token` is the **decrypted** token bytes (the result of
/// `crate::cipher::decrypt_exchange_token`) — UTF-8-decoded into the
/// `<exchangeToken>` element. NAV returns this token base64-encoded and
/// AES-128/ECB-encrypted; this function is downstream of the decrypt.
///
/// `items` is the per-index invoice list. Per NAV v3.0 the max length
/// is 100; this function ENFORCES the cap (loud-fails on
/// `NavTransportError::ManageInvoiceTooManyItems`) so the caller cannot
/// silently truncate.
pub fn render_manage_invoice_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
    exchange_token: &str,
    items: &[ManageInvoiceItem<'_>],
) -> Result<Vec<u8>, NavTransportError> {
    if items.is_empty() {
        return Err(NavTransportError::ManageInvoiceEmpty);
    }
    if items.len() > 100 {
        return Err(NavTransportError::ManageInvoiceTooManyItems { count: items.len() });
    }

    // Build the per-index inputs for the signature in the same order
    // they will appear on the wire. The signature MUST match the
    // payload byte-for-byte — that's why we compute both from the same
    // `items` slice rather than from two parallel structures.
    let signature_inputs: Vec<InvoiceSignatureInput<'_>> = items
        .iter()
        .map(|i| InvoiceSignatureInput {
            operation: i.operation.as_nav_str(),
            invoice_data_xml: i.invoice_data_xml,
        })
        .collect();
    let signature = request_signature_manage(
        request_id,
        request_timestamp,
        credentials.sign_key_bytes(),
        &signature_inputs,
    );

    render_request(
        "ManageInvoiceRequest",
        credentials,
        tax_number_8,
        request_id,
        request_timestamp,
        &signature,
        |w| {
            // <exchangeToken>...</exchangeToken>
            write_text_in_default_ns(w, "exchangeToken", exchange_token)?;
            // <invoiceOperations>
            w.write_event(Event::Start(BytesStart::new("invoiceOperations")))
                .map_err(envelope_io)?;
            write_text_in_default_ns(w, "compressedContent", "false")?;
            for (i, item) in items.iter().enumerate() {
                let index = (i + 1) as u32;
                w.write_event(Event::Start(BytesStart::new("invoiceOperation")))
                    .map_err(envelope_io)?;
                write_text_in_default_ns(w, "index", &index.to_string())?;
                write_text_in_default_ns(w, "invoiceOperation", item.operation.as_nav_str())?;
                let encoded = BASE64_STANDARD.encode(item.invoice_data_xml);
                write_text_in_default_ns(w, "invoiceData", &encoded)?;
                w.write_event(Event::End(BytesEnd::new("invoiceOperation")))
                    .map_err(envelope_io)?;
            }
            w.write_event(Event::End(BytesEnd::new("invoiceOperations")))
                .map_err(envelope_io)?;
            Ok(())
        },
    )
}

// ──────────────────────────────────────────────────────────────────────
// Internal: shared envelope shell
// ──────────────────────────────────────────────────────────────────────

/// Common envelope assembly for every NAV v3.0 request: XML decl, root
/// element with the two namespaces, `<common:header>`, `<common:user>`,
/// `<software>`, then the per-operation body via the closure.
///
/// Extracted so the two public renderers above (and any future
/// `queryTransactionStatus` / `queryInvoiceCheck` in PR-7-C) share one
/// body and one set of element-ordering invariants. The closure receives
/// the writer positioned just after the `<software>` close tag and just
/// before the root close tag.
fn render_request<F>(
    root_name: &str,
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
    signature_hex: &str,
    write_body: F,
) -> Result<Vec<u8>, NavTransportError>
where
    F: FnOnce(&mut Writer<&mut Vec<u8>>) -> Result<(), NavTransportError>,
{
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut w = Writer::new(&mut buf);

    // <?xml version="1.0" encoding="UTF-8"?>
    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(envelope_io)?;

    // Root element with the two NAV namespaces. Default namespace is
    // the API namespace so child elements (header, user, software, ...)
    // can be written with the `common:` prefix only when they're in the
    // common namespace.
    let mut root = BytesStart::new(root_name.to_string());
    root.push_attribute(("xmlns", NAV_NS_API));
    root.push_attribute(("xmlns:common", NAV_NS_COMMON));
    w.write_event(Event::Start(root)).map_err(envelope_io)?;

    parts::write_header(&mut w, request_id, request_timestamp)?;
    parts::write_user(
        &mut w,
        credentials.login(),
        &password_hash(credentials.password_bytes()),
        tax_number_8,
        signature_hex,
    )?;
    parts::write_software(&mut w)?;

    // Operation-specific body.
    write_body(&mut w)?;

    w.write_event(Event::End(BytesEnd::new(root_name.to_string())))
        .map_err(envelope_io)?;

    Ok(buf)
}

/// Write `<tag>value</tag>` in the default namespace. Common enough
/// inside the body closures that hoisting it avoids ~5 `write_event`
/// calls per element.
pub(crate) fn write_text_in_default_ns(
    w: &mut Writer<&mut Vec<u8>>,
    tag: &str,
    value: &str,
) -> Result<(), NavTransportError> {
    w.write_event(Event::Start(BytesStart::new(tag.to_string())))
        .map_err(envelope_io)?;
    w.write_event(Event::Text(BytesText::new(value)))
        .map_err(envelope_io)?;
    w.write_event(Event::End(BytesEnd::new(tag.to_string())))
        .map_err(envelope_io)?;
    Ok(())
}

/// Map a `quick_xml::Error` from a writer call into our typed error
/// surface. Writer errors are I/O against an in-memory `Vec<u8>` and
/// should be effectively unreachable, but we treat them loud rather
/// than swallow.
pub(crate) fn envelope_io(e: quick_xml::Error) -> NavTransportError {
    NavTransportError::EnvelopeWriteFailed(e.to_string())
}

#[cfg(test)]
mod tests {
    //! Envelope-level tests. The per-part tests live in `parts.rs`; the
    //! end-to-end golden tests against fixed inputs live in
    //! `crates/nav-transport/tests/soap_golden.rs`.

    use super::*;

    fn fixture_credentials() -> NavCredentials {
        NavCredentials::from_parts(
            "test-tenant",
            "TECHNICAL_LOGIN",
            "tech-password",
            "SIGN-KEY-32BYTES-OF-FAKE-MATERIAL",
            "1234567890ABCDEF",
        )
    }

    #[test]
    fn token_exchange_request_contains_required_blocks() {
        let xml = render_token_exchange_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
        )
        .expect("envelope renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");

        // Sanity: root + namespaces + each common block present.
        assert!(s.contains("<TokenExchangeRequest"));
        assert!(s.contains("xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\""));
        assert!(s.contains("xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\""));
        assert!(s.contains("<common:header>"));
        assert!(s.contains("<common:requestId>REQ12345ABCDEFG</common:requestId>"));
        assert!(s.contains("<common:timestamp>20260520T120000Z</common:timestamp>"));
        assert!(s.contains("<common:user>"));
        assert!(s.contains("<common:login>TECHNICAL_LOGIN</common:login>"));
        assert!(s.contains("<common:taxNumber>12345678</common:taxNumber>"));
        assert!(s.contains("cryptoType=\"SHA-512\""));
        assert!(s.contains("cryptoType=\"SHA3-512\""));
        assert!(s.contains("<software>"));
        // Plaintext credentials must NEVER appear in the rendered envelope.
        assert!(
            !s.contains("tech-password"),
            "plaintext password leaked into envelope: {s}"
        );
        assert!(
            !s.contains("SIGN-KEY-32BYTES-OF-FAKE-MATERIAL"),
            "plaintext sign key leaked into envelope: {s}"
        );
        assert!(
            !s.contains("1234567890ABCDEF"),
            "plaintext change key leaked into envelope: {s}"
        );
    }

    #[test]
    fn manage_invoice_request_orders_indices_from_one() {
        let invoice_xml = b"<InvoiceData>placeholder</InvoiceData>";
        let xml = render_manage_invoice_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &[
                ManageInvoiceItem {
                    operation: InvoiceOperation::Create,
                    invoice_data_xml: invoice_xml,
                },
                ManageInvoiceItem {
                    operation: InvoiceOperation::Create,
                    invoice_data_xml: invoice_xml,
                },
            ],
        )
        .expect("envelope renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");
        assert!(s.contains("<index>1</index>"));
        assert!(s.contains("<index>2</index>"));
        // base64("<InvoiceData>placeholder</InvoiceData>") begins "PEludm9p…"
        // — the leading bytes are stable across base64 implementations and a
        // shape-only check is enough; full byte equality is in the golden
        // fixture test.
        assert!(s.contains("<invoiceData>PEludm9pY2VEYXRhPnBsYWNlaG9sZGVy"));
        assert!(s.contains("<exchangeToken>decrypted-token</exchangeToken>"));
    }

    #[test]
    fn manage_invoice_request_empty_loud_fails() {
        let err = render_manage_invoice_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &[],
        )
        .expect_err("empty list must loud-fail");
        assert!(matches!(err, NavTransportError::ManageInvoiceEmpty));
    }

    #[test]
    fn manage_invoice_request_over_cap_loud_fails() {
        let invoice_xml = b"<InvoiceData>x</InvoiceData>";
        let items: Vec<ManageInvoiceItem<'_>> = (0..101)
            .map(|_| ManageInvoiceItem {
                operation: InvoiceOperation::Create,
                invoice_data_xml: invoice_xml,
            })
            .collect();
        let err = render_manage_invoice_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &items,
        )
        .expect_err("over-cap must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::ManageInvoiceTooManyItems { count: 101 }
        ));
    }
}
