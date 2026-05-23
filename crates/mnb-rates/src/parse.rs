//! MNB `<GetExchangeRatesResponse>` parser. ADR-0037 §2.a.
//!
//! The response carries a `<GetExchangeRatesResult>` element whose
//! text content is **another** XML document (character-escaped, or
//! occasionally CDATA-wrapped — both forms collapse to the same
//! string after `quick_xml::events::BytesText::unescape()`):
//!
//! ```text
//! <soap:Envelope ...>
//!   <soap:Body>
//!     <GetExchangeRatesResponse xmlns="http://www.mnbarfolyamservice.hu/">
//!       <GetExchangeRatesResult>
//!         &lt;MNBExchangeRates&gt;
//!           &lt;Day date="2026-05-22"&gt;
//!             &lt;Rate curr="EUR" unit="1"&gt;405,2300&lt;/Rate&gt;
//!           &lt;/Day&gt;
//!         &lt;/MNBExchangeRates&gt;
//!       </GetExchangeRatesResult>
//!     </GetExchangeRatesResponse>
//!   </soap:Body>
//! </soap:Envelope>
//! ```
//!
//! Two passes over the bytes: outer extracts the inner XML string,
//! inner extracts the `<Day>` + `<Rate>` pair matching the request.
//! The split keeps each pass simple and lets the inner parse be
//! exercised on its own (the inner-XML fixture, without the SOAP
//! envelope wrapper, is the smaller failure surface).

use quick_xml::events::Event;
use quick_xml::Reader;
use time::macros::format_description;
use time::Date;

use crate::error::MnbError;
use aberp_billing::Currency;

/// Parsed MNB rate per ADR-0037 §1.a + §2.b. Carries the verbatim
/// MNB-published value (normalized to dot-decimal form), the
/// publication date MNB actually returned (which may differ from the
/// requested supply-fulfilment date when MNB does not publish on
/// that day — see ADR-0037 §2.b's non-publication-day fallback rule;
/// the consumer pins the differential), the `unit` MNB published
/// (1 for EUR/USD/CHF; 100 for JPY), and the typed currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MnbRate {
    /// The quoted currency (e.g., [`Currency::Eur`]). The base
    /// currency is always [`Currency::Huf`] per ADR-0037 §2 —
    /// MNB publishes HUF cross-rates only.
    pub currency: Currency,

    /// Publication date as MNB returned it. May differ from the
    /// requested date when the requested date was a non-publication
    /// day (weekend / Hungarian public holiday) and MNB walked back
    /// to the most-recent prior publication date per ADR-0037 §2.b.
    pub date: Date,

    /// MNB's unit attribute. EUR/USD/CHF: `1` (rate is HUF per 1
    /// unit). JPY: `100` (rate is HUF per 100 JPY; a future
    /// Currency::Jpy lift makes this matter).
    pub unit: u32,

    /// Verbatim decimal value MNB published, normalized to
    /// dot-decimal form. e.g., MNB's `"405,2300"` → `"405.2300"`.
    /// Stored as `String` so the regulatory-record precision (4
    /// decimal places for EUR per MNB's current cadence) is
    /// preserved without a scale-factor choice that PR-44β does
    /// not need to make.
    pub value: String,
}

/// Source identifier per ADR-0037 §1.a — the literal printed on
/// the invoice. PR-44ε's SPA render reads this constant rather than
/// open-coding the string.
pub const SOURCE: &str = "MNB";

/// Parse a SOAP envelope returned by MNB's `GetExchangeRates`
/// operation. Validates against the requested `currency`; the date
/// returned in the [`MnbRate`] is whatever publication date MNB
/// actually answered with.
pub fn parse_get_exchange_rates_response(
    envelope_xml: &[u8],
    currency: Currency,
) -> Result<MnbRate, MnbError> {
    // 1. Detect a SOAP Fault BEFORE looking for GetExchangeRatesResult
    //    — a faulted response often omits the result element entirely.
    if let Some(fault) = find_first_text_local(envelope_xml, "faultstring")? {
        return Err(MnbError::SoapFault(fault));
    }

    // 2. Extract the inner XML text.
    let inner = find_first_text_local(envelope_xml, "GetExchangeRatesResult")?.ok_or_else(
        || MnbError::EnvelopeParse("response missing <GetExchangeRatesResult>".to_string()),
    )?;

    parse_mnb_exchange_rates_inner(inner.as_bytes(), currency)
}

/// Parse the inner `<MNBExchangeRates>` payload (the string MNB
/// embeds inside `<GetExchangeRatesResult>`). Public so the parse
/// tests can exercise this pass without writing the full SOAP
/// envelope wrapper; the production path always reaches it through
/// [`parse_get_exchange_rates_response`].
pub fn parse_mnb_exchange_rates_inner(
    inner_xml: &[u8],
    currency: Currency,
) -> Result<MnbRate, MnbError> {
    let iso_target = currency.iso_code();

    let mut reader = Reader::from_reader(inner_xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut current_day_date: Option<String> = None;
    // The most recent matching <Rate ...> open tag's metadata —
    // captured at Start so the inner text-event handler knows whether
    // to harvest its body.
    let mut pending_rate: Option<(String, u32)> = None; // (date, unit)
    let mut harvested_value: Option<String> = None;
    let mut harvested_date: Option<String> = None;
    let mut harvested_unit: Option<u32> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                if local == b"Day" {
                    current_day_date = attribute_value(&e, "date")?;
                } else if local == b"Rate" {
                    let curr = attribute_value(&e, "curr")?
                        .ok_or_else(|| MnbError::PayloadParse(
                            "<Rate> missing curr attribute".to_string(),
                        ))?;
                    if curr.as_str() == iso_target {
                        let unit = attribute_value(&e, "unit")?
                            .ok_or_else(|| MnbError::PayloadParse(
                                "<Rate> missing unit attribute".to_string(),
                            ))?
                            .parse::<u32>()
                            .map_err(|e| MnbError::PayloadParse(format!(
                                "<Rate unit=...> is not a u32: {e}"
                            )))?;
                        let day_date = current_day_date.clone().ok_or_else(|| {
                            MnbError::PayloadParse(
                                "<Rate> appeared outside any <Day> element".to_string(),
                            )
                        })?;
                        pending_rate = Some((day_date, unit));
                    }
                }
            }
            Ok(Event::Text(t)) if pending_rate.is_some() => {
                let raw = t.unescape().map_err(|e| {
                    MnbError::PayloadParse(format!("XML text unescape failed: {e}"))
                })?;
                let normalized = normalize_decimal(raw.as_ref())?;
                let (date_str, unit) = pending_rate.take().expect("guard above");
                harvested_value = Some(normalized);
                harvested_date = Some(date_str);
                harvested_unit = Some(unit);
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                if local_name(name.as_ref()) == b"Day" {
                    current_day_date = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(MnbError::PayloadParse(format!(
                    "inner XML parse failed at position {}: {e}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
        buf.clear();
    }

    let value = harvested_value.ok_or_else(|| MnbError::NoRateForCurrency {
        currency: iso_target.to_string(),
        date: harvested_date.clone().unwrap_or_else(|| "?".to_string()),
    })?;
    let date_str = harvested_date.expect("set together with value");
    let unit = harvested_unit.expect("set together with value");

    let date_fmt = format_description!("[year]-[month]-[day]");
    let date = Date::parse(&date_str, &date_fmt).map_err(|e| {
        MnbError::PayloadParse(format!(
            "<Day date=\"{date_str}\"> is not an ISO 8601 date: {e}"
        ))
    })?;

    Ok(MnbRate {
        currency,
        date,
        unit,
        value,
    })
}

/// Normalize MNB's decimal text to dot-form. MNB returns rates in
/// the Hungarian locale convention (comma decimal separator) in
/// some endpoints and dot in others. The regulatory print form per
/// ADR-0037 §1.a uses dot, so we normalize at the parse boundary
/// and store dot-form verbatim. Loud-fails on a non-decimal input
/// per CLAUDE.md rule 12.
fn normalize_decimal(raw: &str) -> Result<String, MnbError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(MnbError::MalformedDecimal {
            value: raw.to_string(),
        });
    }
    let dot_form = trimmed.replace(',', ".");
    let mut seen_dot = false;
    for (i, c) in dot_form.chars().enumerate() {
        match c {
            '0'..='9' => {}
            '-' if i == 0 => {}
            '.' if !seen_dot => seen_dot = true,
            _ => {
                return Err(MnbError::MalformedDecimal {
                    value: raw.to_string(),
                });
            }
        }
    }
    Ok(dot_form)
}

fn attribute_value(
    e: &quick_xml::events::BytesStart<'_>,
    target: &str,
) -> Result<Option<String>, MnbError> {
    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|err| {
            MnbError::PayloadParse(format!("attribute parse failed: {err}"))
        })?;
        if attr.key.as_ref() == target.as_bytes() {
            let v = attr.unescape_value().map_err(|err| {
                MnbError::PayloadParse(format!("attribute unescape failed: {err}"))
            })?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

fn local_name(qualified: &[u8]) -> &[u8] {
    match qualified.iter().rposition(|&b| b == b':') {
        Some(i) => &qualified[i + 1..],
        None => qualified,
    }
}

/// Local-name match against quick-xml's qualified element name.
/// Mirrors `nav-transport::operations::find_first_text` — kept here
/// rather than reaching into nav-transport because that crate's
/// helper is `pub(crate)` AND because PR-44β's blast radius should
/// not extend into nav-transport. CLAUDE.md rule 3.
fn find_first_text_local(xml: &[u8], target: &str) -> Result<Option<String>, MnbError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut inside = false;
    let mut collected = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if local_name(e.name().as_ref()) == target.as_bytes() => {
                inside = true;
            }
            Ok(Event::End(e))
                if inside && local_name(e.name().as_ref()) == target.as_bytes() =>
            {
                return Ok(Some(collected));
            }
            Ok(Event::Text(t)) if inside => {
                let unescaped = t.unescape().map_err(|e| {
                    MnbError::EnvelopeParse(format!("XML text unescape failed: {e}"))
                })?;
                collected.push_str(unescaped.as_ref());
            }
            Ok(Event::CData(c)) if inside => {
                let raw = std::str::from_utf8(c.as_ref()).map_err(|e| {
                    MnbError::EnvelopeParse(format!("CDATA is not UTF-8: {e}"))
                })?;
                collected.push_str(raw);
            }
            Ok(Event::Eof) => return Ok(None),
            Err(e) => {
                return Err(MnbError::EnvelopeParse(format!(
                    "outer XML parse failed at position {}: {e}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    const INNER_OK_EUR: &str = r#"<MNBExchangeRates>
  <Day date="2026-05-22">
    <Rate curr="EUR" unit="1">405,2300</Rate>
    <Rate curr="USD" unit="1">357,1500</Rate>
    <Rate curr="CHF" unit="1">412,8800</Rate>
  </Day>
</MNBExchangeRates>"#;

    #[test]
    fn inner_payload_parses_eur_rate_with_comma_normalization() {
        let r = parse_mnb_exchange_rates_inner(INNER_OK_EUR.as_bytes(), Currency::Eur)
            .expect("parse");
        assert_eq!(r.currency, Currency::Eur);
        assert_eq!(r.date, date!(2026 - 05 - 22));
        assert_eq!(r.unit, 1);
        // Comma → dot normalization is the regulatory-print posture
        // per ADR-0037 §1.a; preserves MNB's published 4-decimal
        // precision verbatim.
        assert_eq!(r.value, "405.2300");
    }

    #[test]
    fn inner_payload_loud_fails_when_requested_currency_absent() {
        // MNB returned EUR/USD/CHF but the caller asked for HUF
        // (regulatorily this would be a programmer error — base
        // currency is always HUF — but the parser MUST loud-fail
        // rather than coerce silently).
        let err = parse_mnb_exchange_rates_inner(INNER_OK_EUR.as_bytes(), Currency::Huf)
            .expect_err("must loud-fail");
        match err {
            MnbError::NoRateForCurrency { currency, .. } => {
                assert_eq!(currency, "HUF");
            }
            other => panic!("expected NoRateForCurrency, got {other:?}"),
        }
    }

    #[test]
    fn inner_payload_loud_fails_on_malformed_decimal() {
        let bad = r#"<MNBExchangeRates>
  <Day date="2026-05-22">
    <Rate curr="EUR" unit="1">not-a-number</Rate>
  </Day>
</MNBExchangeRates>"#;
        let err = parse_mnb_exchange_rates_inner(bad.as_bytes(), Currency::Eur)
            .expect_err("must loud-fail");
        match err {
            MnbError::MalformedDecimal { value } => {
                assert_eq!(value, "not-a-number");
            }
            other => panic!("expected MalformedDecimal, got {other:?}"),
        }
    }

    #[test]
    fn soap_fault_surfaces_as_typed_error() {
        // Synthetic SOAP fault — the parser MUST treat it as a
        // loud-fail SoapFault rather than silently returning
        // a "no rate" result.
        let fault = br#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
  <soap:Body>
    <soap:Fault>
      <faultcode>soap:Server</faultcode>
      <faultstring>MNB internal error</faultstring>
    </soap:Fault>
  </soap:Body>
</soap:Envelope>"#;
        let err =
            parse_get_exchange_rates_response(fault, Currency::Eur).expect_err("must loud-fail");
        match err {
            MnbError::SoapFault(msg) => {
                assert!(msg.contains("MNB internal error"), "fault msg: {msg}");
            }
            other => panic!("expected SoapFault, got {other:?}"),
        }
    }

    #[test]
    fn normalize_decimal_accepts_dot_and_comma_only() {
        assert_eq!(normalize_decimal("405,23").unwrap(), "405.23");
        assert_eq!(normalize_decimal("405.23").unwrap(), "405.23");
        assert_eq!(normalize_decimal(" 405,2300 ").unwrap(), "405.2300");
        assert_eq!(normalize_decimal("-1,5").unwrap(), "-1.5");
        assert!(normalize_decimal("").is_err());
        assert!(normalize_decimal("12,3,4").is_err());
        assert!(normalize_decimal("12x3").is_err());
    }
}
