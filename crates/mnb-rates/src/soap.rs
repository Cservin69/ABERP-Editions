//! Hand-rolled SOAP envelope for MNB's `GetExchangeRates` operation
//! per ADR-0037 §2.a + ADR-0021 §A8 ("codegen-from-XSD rejected").
//!
//! MNB exposes its rate publication via a classic .NET ASMX SOAP
//! 1.1 service at `https://www.mnb.hu/arfolyamok.asmx`. The same
//! posture nav-transport takes for NAV envelopes applies here: one
//! file, no DSL, no macros, no SOAP-client dep. The envelope shape
//! this module writes is the shape the ASMX service accepts.
//!
//! # Transport details
//!
//! Re-verified against MNB's currently-published WSDL
//! (`https://www.mnb.hu/arfolyamok.asmx?WSDL`) at PR-86 / session-111
//! implementation time. The PR-44β baseline used the legacy
//! `http://www.mnbarfolyamservice.hu/` namespace; MNB has since
//! migrated to `http://www.mnb.hu/webservices/` on the same endpoint
//! path, and the legacy namespace now produces a SOAP fault
//! (`Object reference not set to an instance of an object` —
//! WCF binding refuses the unbound message). The new shape:
//!
//!   - URL: `http://www.mnb.hu/arfolyamok.asmx` (HTTP — the HTTPS
//!     form 404s SOAP POSTs at the WAF layer; see
//!     `lib.rs::MNB_ENDPOINT_URL` for the live-verified rationale).
//!   - SOAPAction header:
//!     `"http://www.mnb.hu/webservices/MNBArfolyamServiceSoap/GetExchangeRates"`.
//!   - Content-Type: `text/xml; charset=utf-8` (SOAP 1.1 convention).
//!   - Operation namespace: `http://www.mnb.hu/webservices/`.
//!
//! These three transport details are exercised end-to-end by the
//! env-gated live test in `tests/live_mnb.rs`; a regression on any
//! one of them surfaces there.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::error::MnbError;

/// SOAP 1.1 envelope namespace.
pub const SOAP_NS: &str = "http://schemas.xmlsoap.org/soap/envelope/";

/// MNB operation namespace, used as default-ns on the
/// `<GetExchangeRates>` body element.
///
/// **PR-86 / session-111** — migrated from the pre-PR-86 value
/// `http://www.mnbarfolyamservice.hu/` (which no longer matches the
/// live WSDL — a request carrying the old namespace produces a WCF
/// `Object reference not set to an instance of an object` SOAP fault
/// because the binding has no handler bound to the old URI). The
/// PR-86 value matches the live WSDL emitted by
/// `https://www.mnb.hu/arfolyamok.asmx?WSDL`.
pub const MNB_NS: &str = "http://www.mnb.hu/webservices/";

/// SOAPAction header value for the `GetExchangeRates` operation.
/// MUST be sent quoted per SOAP 1.1; the caller wraps it in
/// `header("SOAPAction", format!("\"{}\"", SOAP_ACTION_GET_RATES))`.
///
/// **PR-86 / session-111** — migrated from the pre-PR-86 value
/// `http://www.mnbarfolyamservice.hu/GetExchangeRates`. The new
/// value mirrors the operation binding in the live WSDL —
/// `<namespace>/MNBArfolyamServiceSoap/GetExchangeRates`.
pub const SOAP_ACTION_GET_RATES: &str =
    "http://www.mnb.hu/webservices/MNBArfolyamServiceSoap/GetExchangeRates";

/// Render a `<GetExchangeRates>` SOAP 1.1 envelope, ready for HTTP
/// POST. Single-date queries (start == end) are the only shape PR-44β
/// uses today; multi-date queries would be an additive parameter
/// when an operational case fires for them.
///
/// `date_iso` is the ISO 8601 `YYYY-MM-DD` date string. The caller
/// (`crate::MnbClient::fetch_official_rate`) formats it from
/// `time::Date` to keep the date formatting in one place.
///
/// `currency_iso` is the ISO 4217 three-letter currency code. Same
/// posture: the caller resolves it from the typed
/// `aberp_billing::Currency` enum via the
/// [`Currency::iso_code`](aberp_billing::Currency::iso_code)
/// accessor that landed in PR-44α.
pub fn render_get_exchange_rates_request(
    date_iso: &str,
    currency_iso: &str,
) -> Result<Vec<u8>, MnbError> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut w = Writer::new(&mut buf);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(envelope_io)?;

    let mut envelope = BytesStart::new("soap:Envelope");
    envelope.push_attribute(("xmlns:soap", SOAP_NS));
    w.write_event(Event::Start(envelope)).map_err(envelope_io)?;

    w.write_event(Event::Start(BytesStart::new("soap:Body")))
        .map_err(envelope_io)?;

    // <GetExchangeRates xmlns="http://www.mnbarfolyamservice.hu/">
    let mut op = BytesStart::new("GetExchangeRates");
    op.push_attribute(("xmlns", MNB_NS));
    w.write_event(Event::Start(op)).map_err(envelope_io)?;

    write_text(&mut w, "startDate", date_iso)?;
    write_text(&mut w, "endDate", date_iso)?;
    write_text(&mut w, "currencyNames", currency_iso)?;

    w.write_event(Event::End(BytesEnd::new("GetExchangeRates")))
        .map_err(envelope_io)?;
    w.write_event(Event::End(BytesEnd::new("soap:Body")))
        .map_err(envelope_io)?;
    w.write_event(Event::End(BytesEnd::new("soap:Envelope")))
        .map_err(envelope_io)?;

    Ok(buf)
}

fn write_text(w: &mut Writer<&mut Vec<u8>>, tag: &str, value: &str) -> Result<(), MnbError> {
    w.write_event(Event::Start(BytesStart::new(tag.to_string())))
        .map_err(envelope_io)?;
    w.write_event(Event::Text(BytesText::new(value)))
        .map_err(envelope_io)?;
    w.write_event(Event::End(BytesEnd::new(tag.to_string())))
        .map_err(envelope_io)?;
    Ok(())
}

fn envelope_io(e: quick_xml::Error) -> MnbError {
    MnbError::EnvelopeParse(format!("SOAP envelope write failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_carries_required_fields() {
        let xml = render_get_exchange_rates_request("2026-05-22", "EUR").expect("renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");

        // SOAP 1.1 envelope + namespace.
        assert!(s.contains("<soap:Envelope"));
        assert!(s.contains("xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\""));
        assert!(s.contains("<soap:Body>"));
        // Operation + MNB namespace (PR-86 — live WSDL namespace).
        assert!(s.contains("<GetExchangeRates"));
        assert!(s.contains("xmlns=\"http://www.mnb.hu/webservices/\""));
        // Parameters in XSD-sequence order.
        assert!(s.contains("<startDate>2026-05-22</startDate>"));
        assert!(s.contains("<endDate>2026-05-22</endDate>"));
        assert!(s.contains("<currencyNames>EUR</currencyNames>"));

        // XSD-sequence order pin: startDate → endDate → currencyNames.
        let r_start = s.find("<startDate>").expect("startDate present");
        let r_end = s.find("<endDate>").expect("endDate present");
        let r_cur = s.find("<currencyNames>").expect("currencyNames present");
        assert!(
            r_start < r_end && r_end < r_cur,
            "GetExchangeRates child order drifted: {s}"
        );
    }
}
