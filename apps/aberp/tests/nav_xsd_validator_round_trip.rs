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
            tax_number: "12345678-1-42".to_string(),
            name: "ABERP Supplier Kft.".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1011".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Fő utca 1.".to_string(),
        },
        customer: CustomerInfo {
            tax_number: "87654321-1-42".to_string(),
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

/// PR-66 / session-87 — byte-verbatim pin on the structured
/// `<supplierTaxNumber>` and `<customerTaxNumber>` shape. The
/// emitter MUST write three `common:`-prefixed children (NAV v3.0
/// base namespace per ADR-0022 §"common namespace") for both
/// supplier and customer sides.
///
/// This is the load-bearing closer against the regression class
/// session 87 surfaced: PR-50 / session-70 added the `common:`
/// prefix to the emitter, but `parse_supplier_tax_number_from_xml`
/// (a substring scan in `apps/aberp/src/serve.rs`) was not updated
/// and continued looking for the bare-prefix form. The on-disk XML
/// passed the v3.0 invariant check (which is namespace-blind via
/// `local_name_of` — PR-66 tightens it for these children) but the
/// extractor returned the wrong tag and surfaced as
/// `<supplierTaxNumber> missing <taxpayerId>` at submit time.
///
/// The byte-verbatim assertions below — paired with PR-66's
/// `WrongChildNamespacePrefix` invariant in the validator AND the
/// updated substring literals in
/// `parse_supplier_tax_number_from_xml` — span emitter, validator,
/// and extractor so a divergence on any side surfaces at CI time
/// rather than at a live NAV-test submit.
#[test]
fn emitter_renders_common_prefix_on_supplier_and_customer_tax_number() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed on the minimal fixture");
    let body = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    // Supplier side — the three structured children must carry the
    // `common:` prefix (NAV v3.0 base namespace). Bare-prefix shape
    // (the regression class) would make these assertions fail.
    assert!(
        body.contains("<common:taxpayerId>12345678</common:taxpayerId>"),
        "supplier <common:taxpayerId> must be present byte-verbatim; got body:\n{body}"
    );
    assert!(
        body.contains("<common:vatCode>1</common:vatCode>"),
        "supplier <common:vatCode> must be present byte-verbatim; got body:\n{body}"
    );
    assert!(
        body.contains("<common:countyCode>42</common:countyCode>"),
        "supplier <common:countyCode> must be present byte-verbatim; got body:\n{body}"
    );

    // Customer side — symmetric. PR-50's "second-pass AI fix" added
    // the customer-side structured shape; PR-66 pins it here so a
    // future regression that drops the prefix on either side
    // surfaces at the same test.
    assert!(
        body.contains("<common:taxpayerId>87654321</common:taxpayerId>"),
        "customer <common:taxpayerId> must be present byte-verbatim; got body:\n{body}"
    );
    assert!(
        body.contains("<common:vatCode>1</common:vatCode>"),
        "customer <common:vatCode> must be present byte-verbatim; got body:\n{body}"
    );
    // The minimal parties fixture above uses 42 for both supplier
    // and customer county codes; the supplier assertion above is
    // satisfied by the same bytes that satisfy the customer one,
    // so we add a defence-in-depth count check rather than a
    // duplicated string match.
    let common_county_count = body.matches("<common:countyCode>").count();
    assert_eq!(
        common_county_count, 2,
        "expected exactly two <common:countyCode> elements (supplier + customer), \
         got {common_county_count}; body:\n{body}"
    );

    // And the load-bearing negative — the bare-prefix shape MUST
    // NOT appear anywhere in the body. If a future regression
    // changes `common_element` to drop the prefix, this catches it
    // even when (e.g.) only one of the two parties switched.
    assert!(
        !body.contains("<taxpayerId>"),
        "bare-prefix <taxpayerId> must NOT appear anywhere — the emitter \
         writes the `common:` prefix per NAV v3.0; got body:\n{body}"
    );
    assert!(
        !body.contains("<vatCode>"),
        "bare-prefix <vatCode> must NOT appear anywhere; got body:\n{body}"
    );
    assert!(
        !body.contains("<countyCode>"),
        "bare-prefix <countyCode> must NOT appear anywhere; got body:\n{body}"
    );

    // The v3.0 invariant check MUST pass on these same bytes —
    // the PR-66 tightening (WrongChildNamespacePrefix) only fires
    // on the bare-prefix shape; the canonical wire shape this
    // emitter writes is what the tightened invariant accepts.
    validate_invoice_data(&xml)
        .expect("v3.0 invariant check must pass on the emitter's canonical output");
}

/// PR-66 / session-87 — concrete invoice-16 pin. Reproduces the
/// exact wire shape that bit Áben Consulting KFT.'s invoice 16
/// against NAV-test (tax number `24904362-2-41`, customer AZ9
/// Services `27952890-2-42` per the on-disk XML at
/// `~/.aberp/serve/test/issued/01KSJVFW4FW5T21X5KXXBQAJZJ.xml`).
/// Confirms the emitter still produces the wire-real `common:`
/// prefixed shape for the operator-relevant Áben tax number, AND
/// that the tightened v3.0 invariant accepts it. The extractor
/// side of the pin lives in `serve.rs::tests::
/// parse_supplier_tax_number_from_xml_round_trips_against_emitter`
/// (same crate, where the extractor function is private).
#[test]
fn invoice_16_aben_consulting_tax_number_round_trips_through_emit_and_validate() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = NavParties {
        supplier: SupplierInfo {
            tax_number: "24904362-2-41".to_string(),
            name: "Aben Consulting Kft".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            // ASCII subset of the real street name — the Hungarian
            // accents in "Visszatérő köz 6" round-trip through the
            // emitter's UTF-8 path fine but a non-ASCII literal in
            // the test source can stumble on a future toolchain
            // change; the byte-verbatim assertions below only care
            // about the tax-number shape.
            address_street: "Visszatero koz 6".to_string(),
        },
        customer: CustomerInfo {
            tax_number: "27952890-2-42".to_string(),
            name: "AZ9 Services".to_string(),
        },
    };

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed on the invoice-16 fixture");
    let body = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    assert!(
        body.contains("<common:taxpayerId>24904362</common:taxpayerId>"),
        "Áben supplier taxpayerId 24904362 must appear with common: prefix; body:\n{body}"
    );
    assert!(
        body.contains("<common:vatCode>2</common:vatCode>"),
        "Áben supplier vatCode 2 must appear with common: prefix; body:\n{body}"
    );
    assert!(
        body.contains("<common:countyCode>41</common:countyCode>"),
        "Áben supplier countyCode 41 must appear with common: prefix; body:\n{body}"
    );
    assert!(
        body.contains("<common:taxpayerId>27952890</common:taxpayerId>"),
        "AZ9 customer taxpayerId 27952890 must appear with common: prefix; body:\n{body}"
    );

    validate_invoice_data(&xml).expect(
        "v3.0 invariant check must pass on invoice-16's Áben/AZ9 emit output \
         (this is the same byte shape that NAV-test accepted post-fix)",
    );
}
