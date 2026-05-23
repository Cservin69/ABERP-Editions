//! Fixture-driven parser pin for ADR-0037 §2.a's wire-shape contract.
//!
//! Two fixtures (both real-shape, hand-captured from the
//! MNBArfolyamService WSDL examples), one parser, one expected
//! [`MnbRate`] value. The two fixtures differ ONLY in their
//! `<GetExchangeRatesResult>` encoding (character-escaped vs
//! CDATA-wrapped) — MNB has been observed to emit both forms
//! depending on the SOAP toolchain version. Pinning both shapes
//! against the same expected output means a regression that
//! handles one but not the other surfaces here, not when an
//! operator first sees a malformed printed invoice.

use aberp_billing::Currency;
use aberp_mnb_rates::parse::parse_get_exchange_rates_response;
use time::macros::date;

const FIXTURE_ESCAPED: &[u8] =
    include_bytes!("fixtures/get_exchange_rates_eur_2026-05-22.xml");
const FIXTURE_CDATA: &[u8] =
    include_bytes!("fixtures/get_exchange_rates_eur_2026-05-22_cdata.xml");

#[test]
fn escaped_inner_xml_round_trips_to_typed_rate() {
    let rate = parse_get_exchange_rates_response(FIXTURE_ESCAPED, Currency::Eur)
        .expect("character-escaped envelope must parse cleanly");
    assert_eq!(rate.currency, Currency::Eur);
    assert_eq!(rate.date, date!(2026 - 05 - 22));
    assert_eq!(rate.unit, 1);
    assert_eq!(rate.value, "405.2300");
}

#[test]
fn cdata_wrapped_inner_xml_round_trips_to_same_typed_rate() {
    let rate = parse_get_exchange_rates_response(FIXTURE_CDATA, Currency::Eur)
        .expect("CDATA-wrapped envelope must parse cleanly");
    assert_eq!(rate.currency, Currency::Eur);
    assert_eq!(rate.date, date!(2026 - 05 - 22));
    assert_eq!(rate.unit, 1);
    assert_eq!(rate.value, "405.2300");
}

/// Pin the two-encoding equivalence: the parser MUST treat
/// character-escaped and CDATA-wrapped responses as semantically
/// identical. A regression that diverges them surfaces here.
#[test]
fn escaped_and_cdata_encodings_produce_identical_rate() {
    let a = parse_get_exchange_rates_response(FIXTURE_ESCAPED, Currency::Eur)
        .expect("escaped parses");
    let b = parse_get_exchange_rates_response(FIXTURE_CDATA, Currency::Eur)
        .expect("cdata parses");
    assert_eq!(a, b);
}
