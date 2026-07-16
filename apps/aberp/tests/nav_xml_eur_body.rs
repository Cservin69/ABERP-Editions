//! PR-44δ pin tests — NAV `<InvoiceData>` body's EUR + HUF extension
//! per ADR-0037 §1.b + §4 invariants C4 + C5 + C11.
//!
//! Slice covered:
//!
//! - **EUR happy path.** A fixture invoice with `currency=EUR`,
//!   `exchange_rate=356.690000`, `exchange_rate_source=MNB`,
//!   `exchange_rate_date=2026-05-08`, `huf_equivalent_total` computed.
//!   Asserts every one of the three ADR-0037 §1.b confirmed XSD field
//!   leaves carries the expected value: `<currencyCode>EUR</currencyCode>`,
//!   `<exchangeRate>356.690000</exchangeRate>` (exactly 6 decimals per
//!   §1.c + C11), `<invoiceVatAmountHUF>` equal to the round-half-even
//!   conversion of the EUR VAT total at the stamped rate.
//!
//! - **HUF back-compat.** A fixture invoice with `currency=HUF`,
//!   `rate_metadata=None`. Asserts `<currencyCode>HUF</currencyCode>`,
//!   `<exchangeRate>1.000000</exchangeRate>` (uniformly 6-decimal per
//!   the C11 precision pin — supersedes the pre-PR-44δ `1` form),
//!   `<invoiceVatAmountHUF>` byte-equal to `<invoiceVatAmount>` (HUF is
//!   denominated in HUF; the "equivalent" is the value itself).
//!
//! - **Rate-precision pin.** `<exchangeRate>` MUST serialize at exactly
//!   six decimal places — the load-bearing differential against the
//!   pre-PR-44δ `1` form and against a hypothetical truncated EUR rate
//!   like `356.69` (would lose three trailing zeros). The pin fails
//!   loud if a future refactor accidentally drops the `{:.6}` precision
//!   specifier on `rust_decimal::Decimal::fmt`.
//!
//! - **Round-trip parse byte-equality.** Render the EUR body, parse it
//!   back, assert the three ADR-0037 §1.b confirmed XSD field-leaf
//!   text contents are byte-identical to what the renderer wrote. This
//!   pins the emitter against `quick-xml`'s indenter or any other
//!   serialization-layer mutation accidentally rewriting the leaf
//!   bytes.
//!
//! - **EUR loud-fail without rate metadata.** Pins the ADR-0037 §4
//!   invariant C1 wire-side counterpart: calling `render_invoice_data`
//!   with `Currency::Eur` and `rate_metadata=None` MUST loud-fail
//!   (not silently emit a HUF-shaped body for an EUR invoice).

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
use aberp_billing::{
    huf_equivalent_round_half_even, Currency, CustomerId, Huf, InvoiceId, LineItem, RateMetadata,
    ReadyInvoice, SeriesCode, SeriesId,
};
use aberp_nav_xsd_validator::validate_invoice_data;
use quick_xml::events::Event;
use quick_xml::Reader;
use rust_decimal::Decimal;
use std::str::FromStr;
use time::macros::date;
use time::OffsetDateTime;

fn minimal_parties() -> NavParties {
    NavParties {
        supplier: SupplierInfo {
            tax_number: "12345678-1-42".to_string(),
            name: "ABERP Supplier Kft.".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1011".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Fő utca 1.".to_string(),
        },
        customer: CustomerInfo {
            community_vat_number: None,
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit
            // Domestic posture for legacy test fixtures.
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("87654321-1-42".to_string()),
            name: "Test Customer Zrt.".to_string(),
            // PR-77 / session-101 — `customerAddress` required for any
            // DOMESTIC customerVatStatus.
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Váci utca 19.".to_string(),
            }),
        },
    }
}

/// Build a fixture EUR invoice. The line `unit_price` is wrapped in
/// `Huf(cents)` per PR-44γ's interim posture (see
/// `apps/aberp/src/issue_invoice.rs::finalize_rate`'s doc); a future PR
/// lifts `LineItem` to typed-EUR cents. Two lines × 27% VAT = the
/// canonical Hungarian rate.
fn build_eur_invoice() -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 7,
        fiscal_year: 0,
        lines: vec![
            LineItem {
                description: "EUR widget".to_string(),
                quantity: rust_decimal::Decimal::from(2),
                unit_price: Huf(1000), // 10.00 EUR per unit
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            },
            LineItem {
                description: "EUR install".to_string(),
                quantity: rust_decimal::Decimal::from(1),
                unit_price: Huf(5000), // 50.00 EUR
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            },
        ],
        // Fixed wall-clock date so the rate-publication-date assertion
        // is deterministic across runs.
        issue_date: OffsetDateTime::now_utc(),
        // PR-84 — fixture defaults both invoice-date fields to issue
        // date (preserves the pre-PR-84 wire-on-disk byte shape this
        // test asserts against).
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
    }
}

fn build_huf_invoice() -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 7,
        fiscal_year: 0,
        lines: vec![
            LineItem {
                description: "HUF widget".to_string(),
                quantity: rust_decimal::Decimal::from(2),
                unit_price: Huf(1000),
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            },
            LineItem {
                description: "HUF install".to_string(),
                quantity: rust_decimal::Decimal::from(1),
                unit_price: Huf(5000),
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            },
        ],
        issue_date: OffsetDateTime::now_utc(),
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
    }
}

/// Build the canonical EUR rate-metadata fixture: rate `356.690000`,
/// source `MNB`, date `2026-05-08`. The `huf_equivalent_total` field
/// is the per-invoice gross-total stamp PR-44γ computes; we recompute
/// it here at the same `huf_equivalent_round_half_even` helper to keep
/// the fixture self-consistent (the test's load-bearing assertion is
/// the per-VAT-rate / invoice-level `*HUF` amounts on the wire body,
/// not this gross stamp).
fn build_eur_rate_metadata(invoice: &ReadyInvoice) -> RateMetadata {
    let rate = Decimal::from_str("356.690000").unwrap();
    let gross_cents: i64 = invoice
        .lines
        .iter()
        .map(|l| l.gross_total().unwrap().as_i64())
        .sum();
    let huf_equivalent_total = huf_equivalent_round_half_even(gross_cents, &rate).unwrap();
    RateMetadata {
        rate,
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        huf_equivalent_total,
    }
}

/// Walk an XML byte slice and return the text content of every
/// element whose local-name appears in `targets`. Used by the
/// round-trip assertions so we read the parsed text exactly the way
/// any downstream consumer would.
fn collect_leaf_texts(xml: &[u8], targets: &[&str]) -> Vec<(String, String)> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut found = Vec::new();
    let mut buf = Vec::new();
    let mut current: Option<String> = None;
    let mut text_buf: Vec<u8> = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = std::str::from_utf8(e.local_name().as_ref())
                    .unwrap()
                    .to_string();
                if targets.contains(&local.as_str()) {
                    current = Some(local);
                    text_buf.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if current.is_some() {
                    text_buf.extend_from_slice(t.as_ref());
                }
            }
            Ok(Event::End(e)) => {
                let local = std::str::from_utf8(e.local_name().as_ref())
                    .unwrap()
                    .to_string();
                if Some(local.as_str()) == current.as_deref() {
                    let text = String::from_utf8(text_buf.clone()).unwrap();
                    found.push((current.take().unwrap(), text));
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("XML parse error: {e}"),
        }
        buf.clear();
    }
    found
}

// ── Tests ─────────────────────────────────────────────────────────────

/// EUR happy path — the three confirmed ADR-0037 §1.b XSD leaves all
/// carry the expected values.
#[test]
fn eur_body_pins_currency_code_exchange_rate_invoice_vat_amount_huf() {
    let invoice = build_eur_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let rate_metadata = build_eur_rate_metadata(&invoice);

    let xml = nav_xml::render_invoice_data(
        &invoice,
        &series,
        &parties,
        Currency::Eur,
        Some(&rate_metadata),
    )
    .expect("EUR happy path must render cleanly");

    // Validator MUST accept the rendered bytes (ADR-0022 trap-door
    // pair-up — extends to EUR per PR-44δ).
    validate_invoice_data(&xml).expect("validator must accept EUR body");

    let body = String::from_utf8(xml.clone()).unwrap();
    assert!(
        body.contains("<currencyCode>EUR</currencyCode>"),
        "expected <currencyCode>EUR</currencyCode> in body, got: {body}"
    );
    assert!(
        body.contains("<exchangeRate>356.690000</exchangeRate>"),
        "expected <exchangeRate>356.690000</exchangeRate> in body (ADR-0037 §1.c 6-decimal pin), got: {body}"
    );

    // Compute the expected per-invoice HUF VAT total: sum of EUR VAT
    // cents × stamped rate, round-half-even.
    let vat_cents: i64 = invoice
        .lines
        .iter()
        .map(|l| l.vat_amount().unwrap().as_i64())
        .sum();
    let vat_huf_expected = huf_equivalent_round_half_even(vat_cents, &rate_metadata.rate).unwrap();
    let needle = format!("<invoiceVatAmountHUF>{vat_huf_expected}</invoiceVatAmountHUF>");
    assert!(
        body.contains(&needle),
        "expected {needle} in body (per ADR-0037 §1.b / C5), got: {body}"
    );
}

/// HUF back-compat — `<currencyCode>HUF</currencyCode>`,
/// `<exchangeRate>1.000000</exchangeRate>`, and
/// `<invoiceVatAmountHUF>` byte-equal to `<invoiceVatAmount>`.
#[test]
fn huf_body_back_compat_currency_rate_and_vat_huf_pair() {
    let invoice = build_huf_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("HUF back-compat path must render cleanly");

    validate_invoice_data(&xml).expect("validator must accept HUF body");

    let body = String::from_utf8(xml.clone()).unwrap();
    assert!(
        body.contains("<currencyCode>HUF</currencyCode>"),
        "expected <currencyCode>HUF</currencyCode> in body, got: {body}"
    );
    assert!(
        body.contains("<exchangeRate>1.000000</exchangeRate>"),
        "expected <exchangeRate>1.000000</exchangeRate> in body (uniform 6-decimal C11 pin), got: {body}"
    );

    // Pull the two paired leaves and assert byte equality.
    let leaves = collect_leaf_texts(&xml, &["invoiceVatAmount", "invoiceVatAmountHUF"]);
    let native = leaves
        .iter()
        .find(|(k, _)| k == "invoiceVatAmount")
        .map(|(_, v)| v.as_str())
        .expect("invoiceVatAmount must be present");
    let huf = leaves
        .iter()
        .find(|(k, _)| k == "invoiceVatAmountHUF")
        .map(|(_, v)| v.as_str())
        .expect("invoiceVatAmountHUF must be present");
    assert_eq!(
        native, huf,
        "HUF body: invoiceVatAmountHUF must byte-equal invoiceVatAmount"
    );
}

/// Rate-precision pin — `<exchangeRate>` MUST serialize at exactly six
/// decimals. The load-bearing differential is against the pre-PR-44δ
/// integer-`1` form AND against a truncated 2-decimal form like
/// `356.69` (which would lose three trailing zeros). Any future
/// refactor that drops the `{:.6}` precision specifier on
/// `rust_decimal::Decimal::fmt` will break this test.
#[test]
fn rate_serializes_at_six_decimals() {
    let invoice = build_eur_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    // Use a rate value that exposes the precision pin clearly:
    // `405.23` (the §1.a printed-invoice example) MUST become
    // `405.230000` on the wire.
    let rate_metadata = RateMetadata {
        rate: Decimal::from_str("405.23").unwrap(),
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        huf_equivalent_total: 0,
    };

    let xml = nav_xml::render_invoice_data(
        &invoice,
        &series,
        &parties,
        Currency::Eur,
        Some(&rate_metadata),
    )
    .unwrap();
    let body = String::from_utf8(xml).unwrap();

    assert!(
        body.contains("<exchangeRate>405.230000</exchangeRate>"),
        "expected <exchangeRate>405.230000</exchangeRate> (6-decimal padded), got: {body}"
    );
    // Negative pins — these forms would indicate the precision pin
    // dropped:
    assert!(
        !body.contains("<exchangeRate>405.23</exchangeRate>"),
        "exchangeRate must not serialize at fewer than 6 decimals"
    );
    assert!(
        !body.contains("<exchangeRate>405.2300</exchangeRate>"),
        "exchangeRate must serialize at exactly 6 decimals (not 4)"
    );
}

/// Round-trip parse — render the EUR body, parse it back via
/// `quick_xml::Reader`, assert the three ADR-0037 §1.b confirmed XSD
/// leaves carry byte-identical text contents to what the renderer
/// wrote. Pins the emitter against any serialization-layer mutation
/// of the leaf bytes (whitespace, entity encoding, etc.).
#[test]
fn round_trip_parse_pins_three_confirmed_xsd_leaves() {
    let invoice = build_eur_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let rate_metadata = build_eur_rate_metadata(&invoice);

    let xml = nav_xml::render_invoice_data(
        &invoice,
        &series,
        &parties,
        Currency::Eur,
        Some(&rate_metadata),
    )
    .unwrap();

    let leaves = collect_leaf_texts(
        &xml,
        &["currencyCode", "exchangeRate", "invoiceVatAmountHUF"],
    );

    let currency_code = leaves
        .iter()
        .find(|(k, _)| k == "currencyCode")
        .map(|(_, v)| v.as_str())
        .expect("currencyCode leaf must round-trip");
    let exchange_rate = leaves
        .iter()
        .find(|(k, _)| k == "exchangeRate")
        .map(|(_, v)| v.as_str())
        .expect("exchangeRate leaf must round-trip");
    let invoice_vat_amount_huf = leaves
        .iter()
        .find(|(k, _)| k == "invoiceVatAmountHUF")
        .map(|(_, v)| v.as_str())
        .expect("invoiceVatAmountHUF leaf must round-trip");

    assert_eq!(currency_code, "EUR");
    assert_eq!(exchange_rate, "356.690000");

    let vat_cents: i64 = invoice
        .lines
        .iter()
        .map(|l| l.vat_amount().unwrap().as_i64())
        .sum();
    let vat_huf_expected = huf_equivalent_round_half_even(vat_cents, &rate_metadata.rate).unwrap();
    assert_eq!(invoice_vat_amount_huf, vat_huf_expected.to_string());
}

/// EUR + `rate_metadata=None` MUST loud-fail at the render boundary
/// (ADR-0037 §4 invariant C1's wire-side counterpart).
#[test]
fn eur_render_without_rate_metadata_loud_fails() {
    let invoice = build_eur_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let err = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Eur, None)
        .expect_err("EUR without rate metadata must loud-fail");
    let msg = err.to_string();
    assert!(
        msg.contains("non-HUF") && msg.contains("rate_metadata"),
        "expected loud-fail naming non-HUF + rate_metadata, got: {msg}"
    );
}

/// HUF with `rate_metadata=Some(_)` is accepted (the rate is ignored on
/// the HUF branch — `<exchangeRate>1.000000</exchangeRate>` regardless).
/// Pins the rate-metadata-is-tolerated posture for the HUF currency
/// branch so a future caller that always supplies metadata can do so
/// without hitting a false-positive validation error.
#[test]
fn huf_with_rate_metadata_is_accepted_and_ignored() {
    let invoice = build_huf_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let rate_metadata = RateMetadata {
        rate: Decimal::from_str("999.999999").unwrap(),
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        huf_equivalent_total: 0,
    };

    let xml = nav_xml::render_invoice_data(
        &invoice,
        &series,
        &parties,
        Currency::Huf,
        Some(&rate_metadata),
    )
    .unwrap();
    let body = String::from_utf8(xml).unwrap();

    assert!(
        body.contains("<currencyCode>HUF</currencyCode>"),
        "HUF + rate_metadata: currencyCode must still be HUF"
    );
    assert!(
        body.contains("<exchangeRate>1.000000</exchangeRate>"),
        "HUF + rate_metadata: exchangeRate must still be 1.000000 (rate ignored)"
    );
    assert!(
        !body.contains("999.999999"),
        "HUF + rate_metadata: stamped rate must NOT appear on the wire"
    );
}

/// PR-50 / session-70 — pin the structured `<supplierTaxNumber>`
/// emit. NAV `Online Számla` v3.0 requires three sub-elements
/// (`<taxpayerId>` + `<vatCode>` + `<countyCode>`) in that order;
/// the pre-PR-50 renderer emitted a flat dashed string, which NAV's
/// submit endpoint rejected. This pin catches a regression that
/// reverts the fix.
#[test]
fn supplier_tax_number_emits_three_structured_subchildren() {
    let invoice = build_huf_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("HUF render");
    let body = String::from_utf8(xml).expect("UTF-8 body");

    // The structured block — byte-verbatim, indented per the
    // `Writer::new_with_indent(b' ', 2)` posture in `nav_xml.rs`.
    // A regression that re-introduced the flat string would fail
    // every contains() below.
    assert!(
        body.contains("<common:taxpayerId>12345678</common:taxpayerId>"),
        "supplier <taxpayerId> must carry the 8-digit base, got body:\n{body}"
    );
    assert!(
        body.contains("<common:vatCode>1</common:vatCode>"),
        "supplier <vatCode> must carry the 1-digit VAT code, got body:\n{body}"
    );
    assert!(
        body.contains("<common:countyCode>42</common:countyCode>"),
        "supplier <countyCode> must carry the 2-digit county code, got body:\n{body}"
    );
    // And the flat shape must NOT appear — a regression would make
    // both the flat and structured forms present (one would lose
    // the brackets); the negative pin catches that.
    assert!(
        !body.contains("<supplierTaxNumber>12345678-1-42</supplierTaxNumber>"),
        "supplier tax number must NOT serialize as a flat dashed string — that's the pre-PR-50 bug. Body:\n{body}"
    );

    // Defence in depth — the XSD validator that runs at issuance
    // time MUST accept the structured form. A regression that
    // drifted the renderer past the validator's reading would
    // surface here before the live test on Ervin's tree.
    aberp_nav_xsd_validator::validate_invoice_data(body.as_bytes())
        .expect("structured <supplierTaxNumber> must pass the XSD validator");
}

/// PR-66 / session-88 — symmetric pin for the customer side. The
/// session-87 partial fix that landed `walk_structured_tax_number`
/// is symmetric between supplier and customer, but the only
/// pre-session-88 byte-verbatim pin was supplier-only. A future
/// regression that dropped the `common:` prefix from
/// `nav_xml::write_customer` (which uses the same `common_element`
/// helper as `write_supplier` — see CLAUDE.md rule 7 / 11 about
/// keeping parallel code in lockstep) would currently fail only
/// the validator pin, not a byte-shape pin. This test closes that
/// asymmetry so a customer-side prefix regression surfaces both as
/// a byte-shape diff AND as a validator rejection at commit time.
#[test]
fn customer_tax_number_emits_three_structured_subchildren() {
    let invoice = build_huf_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("HUF render");
    let body = String::from_utf8(xml).expect("UTF-8 body");

    // The structured block — byte-verbatim, mirroring the supplier
    // pin above. `minimal_parties().customer.tax_number` is
    // `87654321-1-42`, so the three children carry the parsed
    // 8-digit base / 1-digit vatCode / 2-digit countyCode.
    assert!(
        body.contains("<common:taxpayerId>87654321</common:taxpayerId>"),
        "customer <common:taxpayerId> must carry the 8-digit base, got body:\n{body}"
    );
    assert!(
        body.contains("<common:vatCode>1</common:vatCode>"),
        "customer <common:vatCode> must carry the 1-digit VAT code, got body:\n{body}"
    );
    assert!(
        body.contains("<common:countyCode>42</common:countyCode>"),
        "customer <common:countyCode> must carry the 2-digit county code, got body:\n{body}"
    );
    // Negative byte-shape pin for the bare-prefix regression — the
    // session-87 live failure mode on the supplier side. Same rule
    // both sides per nav-gotchas section 5.
    assert!(
        !body.contains("<customerTaxNumber><taxpayerId>"),
        "customer tax number MUST NOT carry a bare-prefix `<taxpayerId>` child — \
         that's the session-87 regression class. Body:\n{body}"
    );
    // And the dashed flat string must not leak either (the pre-PR-50
    // shape). Symmetric to the supplier pin above.
    assert!(
        !body.contains("<customerTaxNumber>87654321-1-42</customerTaxNumber>"),
        "customer tax number must NOT serialize as a flat dashed string — that's the pre-PR-50 bug. Body:\n{body}"
    );

    // Defence in depth — the XSD validator must accept the structured
    // customer form. Mirror of the supplier-side assertion.
    aberp_nav_xsd_validator::validate_invoice_data(body.as_bytes())
        .expect("structured <customerTaxNumber> must pass the XSD validator");
}

/// PR-50 / session-70 — renderer must loud-fail when handed a
/// `SupplierInfo` whose `tax_number` does not match Hungarian
/// `xxxxxxxx-y-zz` shape. Defence in depth for any CLI / library
/// caller that bypasses the route-layer validate. Mirror pin for
/// the unit test inside `nav_xml::tests` but exercised at the
/// outer `render_invoice_data` surface.
#[test]
fn render_invoice_data_rejects_malformed_supplier_tax_number() {
    let invoice = build_huf_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let mut parties = minimal_parties();
    parties.supplier.tax_number = "24904362".to_string(); // ← bare base; pre-PR-50 leak

    let err = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect_err("renderer must loud-fail on malformed supplier tax");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("supplier tax number"),
        "loud-fail must name the field: {msg}"
    );
    assert!(
        msg.contains("xxxxxxxx-y-zz") || msg.contains("dash-separated"),
        "loud-fail must surface the expected shape: {msg}"
    );
}
