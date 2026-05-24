//! Integration test pinning the ADR-0022 trap-door against drift.
//!
//! The validator's allowlist (`crates/nav-xsd-validator/src/validate.rs`)
//! and the emitter's element set (`apps/aberp/src/nav_xml.rs`) are two
//! sources of truth for "what NAV v3.0 `<InvoiceData>` looks like in
//! ABERP." Divergence between them is exactly the failure mode
//! CLAUDE.md rule 7 names ("two patterns in the codebase ... Claude
//! blending them is how errors get swallowed twice"). This test is
//! the load-bearing closer of that divergence.
//!
//! It renders the bytes the emitter actually produces for the
//! canonical `fixtures/invoice_minimal.json` fixture and asserts the
//! validator accepts them. If a future PR adds a new element to the
//! emitter without extending the validator's allowlist, or removes a
//! required element from the emitter, this test fails at commit time.

use std::time::Duration;

use aberp::nav_xml::{self, CustomerInfo, NavParties, SupplierInfo};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, ReadyInvoice, SeriesCode, SeriesId,
};
use aberp_nav_xsd_validator::{validate_invoice_data, NAV_XSD_VERSION};
use time::OffsetDateTime;

/// Construct a minimal `ReadyInvoice` that mirrors `fixtures/invoice_minimal.json`.
///
/// We use the constructors the billing crate exposes for the
/// integration-test boundary; if a future refactor renames any field
/// the type checker surfaces it loud before the test runs.
fn build_minimal_invoice() -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 1,
        fiscal_year: 0,
        lines: vec![
            LineItem {
                description: "Test widget".to_string(),
                quantity: 2,
                unit_price: Huf(1000),
                vat_rate_basis_points: 2700, // 27%
            },
            LineItem {
                description: "Test installation service".to_string(),
                quantity: 1,
                unit_price: Huf(5000),
                vat_rate_basis_points: 2700,
            },
        ],
        issue_date: OffsetDateTime::now_utc(),
    }
}

fn minimal_parties() -> NavParties {
    NavParties {
        supplier: SupplierInfo {
            tax_number: "12345678".to_string(),
            name: "ABERP Supplier Kft.".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1011".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Fő utca 1.".to_string(),
        },
        customer: CustomerInfo {
            tax_number: "87654321".to_string(),
            name: "Test Customer Zrt.".to_string(),
        },
    }
}

/// The emitter's bytes for the minimal fixture must validate cleanly.
/// This is the ADR-0022 §"Trap-doors against drift" pair-up.
#[test]
fn emitter_minimal_invoice_passes_validator() {
    // `time::OffsetDateTime::now_utc()` is fine for the validator (it
    // only checks YYYY-MM-DD shape). Pin the test to be deterministic
    // by replacing with a fixed date would be over-precise — the
    // validator's only date contract is the YYYY-MM-DD ASCII shape.
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed on the minimal fixture");

    match validate_invoice_data(&xml) {
        Ok(()) => {}
        Err(err) => panic!(
            "validator rejected emitter output for NAV v{NAV_XSD_VERSION}: {err}\n\
             --- bytes ---\n{}\n--- end bytes ---",
            String::from_utf8_lossy(&xml)
        ),
    }
}

/// A trivially-broken byte string (the minimal fixture with a required
/// child removed) must FAIL validation. This pins the negative side of
/// the trap-door: if a future refactor makes the validator over-
/// accepting, this test fails.
#[test]
fn malformed_xml_loud_fails_validator() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let xml =
        nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None).unwrap();
    // Strip the invoiceNumber element entirely. The validator must
    // surface `MissingRequiredChild` (or a wrapping malformed-XML
    // error if the strip happened to leave a tag pair unbalanced).
    let s = String::from_utf8(xml).unwrap();
    let stripped = s.replace("<invoiceNumber>INV-default/00001</invoiceNumber>", "");
    let err = validate_invoice_data(stripped.as_bytes())
        .expect_err("validator must reject XML missing <invoiceNumber>");
    let msg = err.to_string();
    assert!(
        msg.contains("invoiceNumber") || msg.contains("malformed"),
        "expected error to mention invoiceNumber or malformed shape, got: {msg}"
    );
}

/// Validate runs in single-digit milliseconds on the minimal payload.
/// A future refactor that accidentally introduces an O(n²) walk would
/// blow this assertion. The bound is loose (250ms) to survive CI
/// noise while still catching a real regression.
#[test]
fn validator_is_fast_on_minimal_payload() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let xml =
        nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None).unwrap();

    let start = std::time::Instant::now();
    for _ in 0..200 {
        validate_invoice_data(&xml).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "200 validate_invoice_data calls took {elapsed:?} (>250ms)"
    );
}
