//! S160 — `<paymentMethod>` emit pins (ADR-0050).
//!
//! Before S160 the NAV emitter hardcoded
//! `<paymentMethod>TRANSFER</paymentMethod>` for EVERY invoice. S160
//! threads the operator-selected `PaymentMethod` (Fizetési mód) through
//! `render_invoice_data_with_number`.
//!
//! These pins fix the wire shape per variant. NAV's `paymentMethodType`
//! is a CLOSED enum with NO free-text companion — unlike
//! `<unitOfMeasure>`, there is NO `<paymentMethodOwn>` element. So every
//! variant (including `Other`) emits exactly one `<paymentMethod>` token
//! and NOTHING else; emitting a `<paymentMethodOwn>` would be rejected by
//! `nav-xsd-validator` AND by NAV (SCHEMA_VIOLATION).

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, PaymentMethod, ReadyInvoice, SeriesCode,
    SeriesId,
};

fn parties() -> NavParties {
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
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("87654321-1-42".to_string()),
            name: "Test Customer Zrt.".to_string(),
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Váci utca 19.".to_string(),
            }),
        },
    }
}

fn fixture() -> ReadyInvoice {
    let fixed_date = time::macros::datetime!(2026-05-27 10:30:00 UTC);
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        lines: vec![LineItem {
            description: "Tanácsadás".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            unit_price: Huf(1_000),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        }],
        issue_date: fixed_date,
        payment_deadline: fixed_date.date(),
        delivery_date: fixed_date.date(),
        sequence_number: 42,
        fiscal_year: 0,
    }
}

/// Render via the `_with_number` variant carrying an explicit payment
/// method (the production path).
fn render_with(pm: PaymentMethod) -> String {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");
    let invoice = fixture();
    let xml = nav_xml::render_invoice_data_with_number(
        &invoice,
        &series,
        &parties,
        Currency::Huf,
        None,
        pm,
        None,
    )
    .expect("render NAV XML");
    String::from_utf8(xml).expect("NAV body is UTF-8")
}

/// S160 — every closed-vocab variant emits exactly its NAV token, and
/// NEVER a `<paymentMethodOwn>` (no such element in NAV's schema).
#[test]
fn each_variant_emits_its_nav_token_and_no_own() {
    for (pm, token) in [
        (PaymentMethod::Transfer, "TRANSFER"),
        (PaymentMethod::Cash, "CASH"),
        (PaymentMethod::Card, "CARD"),
        (PaymentMethod::Voucher, "VOUCHER"),
        (PaymentMethod::Other, "OTHER"),
    ] {
        let body = render_with(pm);
        let elem = format!("<paymentMethod>{token}</paymentMethod>");
        assert!(
            body.contains(&elem),
            "expected {elem} in body for {pm:?}; got:\n{body}"
        );
        assert!(
            !body.contains("paymentMethodOwn"),
            "NAV has no <paymentMethodOwn> element — must never be emitted (variant {pm:?})"
        );
        // Exactly one paymentMethod element.
        assert_eq!(
            body.matches("<paymentMethod>").count(),
            1,
            "expected exactly one <paymentMethod> for {pm:?}"
        );
    }
}

/// S160 — backward compat: the thin `render_invoice_data` wrapper (used
/// by the pre-S160 test corpus + any caller that has not adopted the
/// payment-method param) defaults to `Transfer`, emitting the same
/// `<paymentMethod>TRANSFER</...>` the pre-S160 hardcoded path did. This
/// is also what a pre-S160 side-stored `input.json` resolves to via
/// `InvoiceInputJson`'s `#[serde(default)]` on `payment_method`.
#[test]
fn thin_wrapper_defaults_to_transfer() {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");
    let invoice = fixture();
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("render NAV XML");
    let body = String::from_utf8(xml).expect("NAV body is UTF-8");
    assert!(body.contains("<paymentMethod>TRANSFER</paymentMethod>"));
    assert!(!body.contains("paymentMethodOwn"));
}

/// S160 — the SPA / side-store wire contract: `InvoiceInputJson` binds
/// the camelCase `"paymentMethod"` field to a bare NAV token string, and
/// a body that OMITS it (pre-S160 side-stored `input.json`, CLI callers)
/// resolves to `Transfer` via `#[serde(default)]`.
#[test]
fn invoice_input_json_payment_method_wire_contract() {
    use aberp::issue_invoice::InvoiceInputJson;

    let with_cash = r#"{
        "supplier": {"taxNumber":"12345678-1-42","name":"S","address":{"countryCode":"HU","postalCode":"1011","city":"Bp","street":"Fő 1."}},
        "customer": {"taxNumber":"87654321-1-42","name":"C"},
        "lines": [{"description":"x","quantity":1,"unitPrice":1000,"vatRatePercent":27}],
        "paymentMethod": "CASH"
    }"#;
    let parsed: InvoiceInputJson =
        serde_json::from_str(with_cash).expect("parse with paymentMethod");
    assert_eq!(parsed.payment_method, PaymentMethod::Cash);

    // Pre-S160 body omitting the field → Transfer (backward compat).
    let without = r#"{
        "supplier": {"taxNumber":"12345678-1-42","name":"S","address":{"countryCode":"HU","postalCode":"1011","city":"Bp","street":"Fő 1."}},
        "customer": {"taxNumber":"87654321-1-42","name":"C"},
        "lines": [{"description":"x","quantity":1,"unitPrice":1000,"vatRatePercent":27}]
    }"#;
    let parsed: InvoiceInputJson =
        serde_json::from_str(without).expect("parse without paymentMethod");
    assert_eq!(parsed.payment_method, PaymentMethod::Transfer);
}
