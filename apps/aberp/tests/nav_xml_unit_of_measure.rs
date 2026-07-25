//! S159 — `<unitOfMeasure>` emit pins.
//!
//! PR-91 shipped the products master-data with the closed-vocab
//! `ProductUnit` type; PR-100 added the product picker to the invoice
//! line. Before S159 the NAV emitter hardcoded
//! `<unitOfMeasure>PIECE</unitOfMeasure>` for EVERY line of EVERY invoice
//! — so a line billed in `LITER`, `HOUR`, or the fuel measure
//! `liter@15C` went to NAV mislabelled as PIECE. S159 threads the picked
//! product's unit (`LineItem.unit: Option<ProductUnit>`) through to the
//! emit.
//!
//! These pins fix the wire shape per `ProductUnit` variant:
//!   - `Nav(token)` → `<unitOfMeasure>{TOKEN}</...>`, NO `<unitOfMeasureOwn>`.
//!   - `Own(label)` → `<unitOfMeasure>OWN</...>` + `<unitOfMeasureOwn>{label}</...>`.
//!   - `None` (freetext / pre-S159 line) → `<unitOfMeasure>PIECE</...>`.

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
use aberp::products::Product;
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, NavUnitOfMeasure, ProductUnit, ReadyInvoice,
    SeriesCode, SeriesId,
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

/// One-line fixture invoice whose single line carries `unit`. Fixed
/// issue date so the body is deterministic.
fn fixture_with_unit(unit: Option<ProductUnit>) -> ReadyInvoice {
    let fixed_date = time::macros::datetime!(2026-05-27 10:30:00 UTC);
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        lines: vec![LineItem {
            description: "Áram".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            unit_price: Huf(1_000),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit,
        }],
        issue_date: fixed_date,
        payment_deadline: fixed_date.date(),
        delivery_date: fixed_date.date(),
        sequence_number: 42,
        fiscal_year: 0,
    }
}

fn render(unit: Option<ProductUnit>) -> String {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");
    let invoice = fixture_with_unit(unit);
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("render NAV XML");
    String::from_utf8(xml).expect("NAV body is UTF-8")
}

/// Assert `<unitOfMeasure>OWN</...>` is IMMEDIATELY followed by
/// `<unitOfMeasureOwn>{label}</...>` (schema order, nothing between but
/// the emitter's indentation whitespace). Pins NAV's LineType ordering.
fn assert_own_then_own_text(body: &str, label: &str) {
    let own = "<unitOfMeasure>OWN</unitOfMeasure>";
    let own_text = format!("<unitOfMeasureOwn>{label}</unitOfMeasureOwn>");
    let own_idx = body
        .find(own)
        .unwrap_or_else(|| panic!("body missing {own}; got:\n{body}"));
    let own_text_idx = body
        .find(&own_text)
        .unwrap_or_else(|| panic!("body missing {own_text}; got:\n{body}"));
    assert!(
        own_idx < own_text_idx,
        "schema order violation: <unitOfMeasure>OWN</...> must precede <unitOfMeasureOwn>"
    );
    let between = &body[own_idx + own.len()..own_text_idx];
    assert!(
        between.trim().is_empty(),
        "<unitOfMeasureOwn> must IMMEDIATELY follow <unitOfMeasure>OWN</...>; \
         found {between:?} between them"
    );
}

/// Each `NavUnitOfMeasure` variant emits its NAV token verbatim and
/// emits NO `<unitOfMeasureOwn>` element.
#[test]
fn nav_variant_emits_token_and_no_own_text() {
    // (variant, expected NAV token). Exhaustive over the closed vocab —
    // a new variant added without the matching token here is a compile
    // miss the reviewer must notice (the list is hand-maintained against
    // `NavUnitOfMeasure`).
    let cases: &[(NavUnitOfMeasure, &str)] = &[
        (NavUnitOfMeasure::Piece, "PIECE"),
        (NavUnitOfMeasure::Kilogram, "KILOGRAM"),
        (NavUnitOfMeasure::Ton, "TON"),
        (NavUnitOfMeasure::Kwh, "KWH"),
        (NavUnitOfMeasure::Day, "DAY"),
        (NavUnitOfMeasure::Hour, "HOUR"),
        (NavUnitOfMeasure::Minute, "MINUTE"),
        (NavUnitOfMeasure::Month, "MONTH"),
        (NavUnitOfMeasure::Liter, "LITER"),
        (NavUnitOfMeasure::Kilometer, "KILOMETER"),
        (NavUnitOfMeasure::CubicMeter, "CUBIC_METER"),
        (NavUnitOfMeasure::Meter, "METER"),
        (NavUnitOfMeasure::LinearMeter, "LINEAR_METER"),
        (NavUnitOfMeasure::Carton, "CARTON"),
        (NavUnitOfMeasure::Pack, "PACK"),
    ];
    for (variant, token) in cases {
        let body = render(Some(ProductUnit::Nav(*variant)));
        assert!(
            body.contains(&format!("<unitOfMeasure>{token}</unitOfMeasure>")),
            "variant {variant:?} must emit <unitOfMeasure>{token}</unitOfMeasure>; got:\n{body}"
        );
        assert!(
            !body.contains("<unitOfMeasureOwn>"),
            "Nav variant {variant:?} must NOT emit <unitOfMeasureOwn>; got:\n{body}"
        );
    }
}

/// `Own(label)` emits `OWN` + the free-text element in schema order.
#[test]
fn own_variant_emits_own_plus_free_text_in_schema_order() {
    let body = render(Some(ProductUnit::Own("liter@15C".to_string())));
    assert_own_then_own_text(&body, "liter@15C");
}

/// `None` (freetext line / pre-S159 line) falls back to PIECE and emits
/// no free-text element — the byte-for-byte pre-S159 shape.
#[test]
fn none_unit_falls_back_to_piece() {
    let body = render(None);
    assert!(
        body.contains("<unitOfMeasure>PIECE</unitOfMeasure>"),
        "None unit must fall back to <unitOfMeasure>PIECE</unitOfMeasure>; got:\n{body}"
    );
    assert!(
        !body.contains("<unitOfMeasureOwn>"),
        "PIECE fallback must NOT emit <unitOfMeasureOwn>; got:\n{body}"
    );
}

/// THE `liter@15C` end-to-end pin (PR-91 brief). A product whose unit is
/// the fuel measure `Own("liter@15C")` — picked onto a line, emitted to
/// NAV — produces `<unitOfMeasure>OWN</...>` immediately followed by
/// `<unitOfMeasureOwn>liter@15C</...>`, AND the rendered body passes the
/// NAV v3.0 XSD invariant check (emitter + validator agree on the shape).
#[test]
fn liter_at_15c_product_threads_to_nav_emit_end_to_end() {
    // The master-data product the operator created in Maintenance →
    // Products (Part G step 3).
    let product = Product {
        id: "prd_00000000000000000000000000".to_string(),
        name: "Áram".to_string(),
        unit: ProductUnit::Own("liter@15C".to_string()),
        currency: Currency::Huf,
        unit_price_minor: 1_000,
        created_at: "2026-05-29T00:00:00Z".to_string(),
        updated_at: "2026-05-29T00:00:00Z".to_string(),
        deleted_at: None,
    };

    // The PR-100 picker's autofill, modelled on the domain side: the
    // line's unit is stamped from the picked product's unit.
    let body = render(Some(product.unit.clone()));

    assert_own_then_own_text(&body, "liter@15C");

    // Emitter + validator agree: the OWN shape the emitter produced is a
    // valid NAV v3.0 LineType (the ADR-0022 invariant check the issue
    // route runs between render and disk write).
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");
    let parties = parties();
    let invoice = fixture_with_unit(Some(product.unit.clone()));
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("render NAV XML");
    aberp_nav_xsd_validator::validate_invoice_data(&xml)
        .expect("OWN + unitOfMeasureOwn body must pass the NAV v3.0 XSD invariant check");
}
