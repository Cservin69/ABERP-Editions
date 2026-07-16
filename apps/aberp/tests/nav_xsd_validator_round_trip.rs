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

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
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
                quantity: rust_decimal::Decimal::from(2),
                unit_price: Huf(1000),
                vat_rate_basis_points: 2700, // 27%
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            },
            LineItem {
                description: "Test installation service".to_string(),
                quantity: rust_decimal::Decimal::from(1),
                unit_price: Huf(5000),
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            },
        ],
        issue_date: OffsetDateTime::now_utc(),
        // PR-84 — minimal fixture defaults both date fields to issue.
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
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
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit Domestic
            // behaviour for the minimal-invoice integration fixture.
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("87654321-1-42".to_string()),
            name: "Test Customer Zrt.".to_string(),
            // PR-77 / session-101 — `customerAddress` required for any
            // DOMESTIC customerVatStatus per NAV business-rule
            // `CUSTOMER_DATA_EXPECTED`.
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Váci utca 19.".to_string(),
            }),
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

/// S157 — a decimal line quantity (1.5 days) must emit as
/// `<quantity>1.5</quantity>` (dot-separated, trailing zeros trimmed),
/// NOT truncated to `<quantity>1</quantity>`, AND the resulting body must
/// still pass the NAV XSD validator. This pins both the emit shape and
/// the round-trip in one test.
#[test]
fn decimal_quantity_emits_dot_separated_and_validates() {
    let mut invoice = build_minimal_invoice();
    invoice.lines = vec![LineItem {
        description: "Consulting".to_string(),
        quantity: rust_decimal::Decimal::new(15, 1), // 1.5
        unit_price: Huf(1000),
        vat_rate_basis_points: 2700,
        vat_rate_kind: aberp_billing::VatRateKind::Percent,
        note: None,
        unit: None,
    }];
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed");
    let body = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    assert!(
        body.contains("<quantity>1.5</quantity>"),
        "decimal quantity must emit dot-separated and untruncated; body:\n{body}"
    );
    assert!(
        !body.contains("<quantity>1</quantity>"),
        "1.5 must NOT be truncated to 1; body:\n{body}"
    );
    // The net total of 1.5 × 1000 = 1500 forints must appear (rounding
    // path exercised — here it is exact).
    assert!(
        body.contains("<lineNetAmount>1500</lineNetAmount>"),
        "net = round(1.5 × 1000) = 1500; body:\n{body}"
    );

    validate_invoice_data(&xml).expect("decimal-quantity body must validate against NAV XSD");
}

/// ADR-0049 §NAV emit (session 156) — the NEGATIVE side of the
/// `<lineModificationReference>` fix. A plain new invoice carries NO
/// `<invoiceReference>` at the head, so its lines MUST NOT carry a
/// `<lineModificationReference>` (it is a chain-body-only element;
/// emitting it on a fresh CREATE would itself be a schema/business-rule
/// violation). The storno/modification round-trip tests pin the
/// positive side; this pins that the fix did not leak onto fresh
/// issuance.
#[test]
fn plain_invoice_omits_line_modification_reference() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed on the minimal fixture");
    let body = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    assert!(
        !body.contains("<lineModificationReference>"),
        "a plain new invoice (no <invoiceReference>) MUST NOT carry \
         <lineModificationReference>; body:\n{body}"
    );
    assert!(
        !body.contains("<lineOperation>"),
        "a plain new invoice MUST NOT carry <lineOperation>; body:\n{body}"
    );
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
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit Domestic
            // posture for the invoice-16 byte-verbatim fixture.
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("27952890-2-42".to_string()),
            name: "AZ9 Services".to_string(),
            // PR-77 / session-101 — `customerAddress` required for any
            // DOMESTIC customerVatStatus; supply a realistic Hungarian
            // address so the round-trip continues to validate. The
            // street name uses the ASCII subset for the same source-
            // literal posture as the supplier address above.
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1097".to_string(),
                city: "Budapest".to_string(),
                street: "Ulloi ut 1.".to_string(),
            }),
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

/// PR-76 — byte-verbatim pin on `<completenessIndicator>false</…>`. NAV
/// v3.0 InvoiceData XSD names this element as required, positioned
/// between `<invoiceIssueDate>` and `<invoiceMain>`. Invoice 17
/// (`inv_01KSM8SRH3X2WQ2TPBHGF8QQBX`) was rejected with NAV
/// `SCHEMA_VIOLATION` naming exactly this element. The pin asserts:
///
///   1. The emitter writes the element with the value `false` (ABERP
///      data-submits via the NAV API; the printed invoice does NOT
///      replace the data record, so the value is always `false`).
///   2. The element appears AFTER `<invoiceIssueDate>` and BEFORE
///      `<invoiceMain>` — the ordered-required position NAV's XSD
///      enforces. A future emitter that drops the element OR puts it
///      in the wrong slot loud-fails here at CI time rather than at
///      the next live submit.
#[test]
fn emitter_writes_completeness_indicator_before_invoice_main() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed on the minimal fixture");
    let body = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    assert!(
        body.contains("<completenessIndicator>false</completenessIndicator>"),
        "<completenessIndicator>false</…> must be present byte-verbatim; body:\n{body}"
    );

    let issue_date_pos = body
        .find("</invoiceIssueDate>")
        .expect("emitter must write <invoiceIssueDate> before <completenessIndicator>");
    let completeness_pos = body
        .find("<completenessIndicator>")
        .expect("emitter must write <completenessIndicator>");
    let invoice_main_pos = body
        .find("<invoiceMain>")
        .expect("emitter must write <invoiceMain> after <completenessIndicator>");
    assert!(
        issue_date_pos < completeness_pos && completeness_pos < invoice_main_pos,
        "expected ordering invoiceIssueDate < completenessIndicator < invoiceMain; \
         got positions {issue_date_pos} / {completeness_pos} / {invoice_main_pos}; body:\n{body}"
    );

    validate_invoice_data(&xml).expect(
        "v3.0 invariant check must pass — the validator now requires completenessIndicator",
    );
}

/// PR-77 / session-101 — byte-verbatim pin on the `<customerAddress>`
/// block. NAV's `CUSTOMER_DATA_EXPECTED` business rule (the rule that
/// ABORTED invoice 18, transaction `5E9KWQSOX3L9EC30`) requires this
/// element whenever `customerVatStatus` is non-PRIVATE_PERSON; the
/// pre-PR-77 emitter omitted it entirely. The pin asserts:
///
///   1. The emitter writes `<customerAddress>` containing
///      `<common:simpleAddress>` containing the four `common:`-prefixed
///      sub-elements `countryCode` / `postalCode` / `city` /
///      `additionalAddressDetail`.
///   2. The block appears AFTER `<customerName>` and BEFORE the
///      closing `</customerInfo>` — the XSD CustomerInfoType ordering.
///   3. The strengthened v3.0 invariant check passes (round-trip).
///
/// Uses the invoice-18 buyer AZ9 Services (tax 27952890-2-42) at the
/// canonical Hungarian-business address shape the SPA's PR-77 form
/// populates from the partner record.
#[test]
fn emitter_writes_customer_address_under_domestic_status() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = NavParties {
        supplier: SupplierInfo {
            tax_number: "24904362-2-41".to_string(),
            name: "Aben Consulting Kft".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatero koz 6".to_string(),
        },
        customer: CustomerInfo {
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit Domestic
            // posture for the AZ9 Services regression fixture.
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("27952890-2-42".to_string()),
            name: "AZ9 Services".to_string(),
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1097".to_string(),
                city: "Budapest".to_string(),
                street: "Ulloi ut 1.".to_string(),
            }),
        },
    };

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed on the invoice-18 fixture");
    let body = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    // Byte-verbatim block presence — the four sub-elements with their
    // `common:` prefix. Mirror of the supplier address shape (the
    // emitter delegates to `write_customer_address`, which uses the
    // same `common_element` helper as `write_address` for the
    // supplier).
    assert!(
        body.contains("<customerAddress>"),
        "<customerAddress> opener must be present byte-verbatim; body:\n{body}"
    );
    assert!(
        body.contains("</customerAddress>"),
        "</customerAddress> closer must be present byte-verbatim; body:\n{body}"
    );
    assert!(
        body.contains("<common:countryCode>HU</common:countryCode>"),
        "<common:countryCode>HU</…> must appear in the customer-address block; body:\n{body}"
    );
    assert!(
        body.contains("<common:postalCode>1097</common:postalCode>"),
        "<common:postalCode>1097</…> must appear; body:\n{body}"
    );
    assert!(
        body.contains("<common:city>Budapest</common:city>"),
        "<common:city>Budapest</…> must appear; body:\n{body}"
    );
    assert!(
        body.contains(
            "<common:additionalAddressDetail>Ulloi ut 1.</common:additionalAddressDetail>"
        ),
        "<common:additionalAddressDetail>Ulloi ut 1.</…> must appear; body:\n{body}"
    );

    // Ordering: customerAddress lives AFTER customerName and BEFORE
    // </customerInfo>. The XSD CustomerInfoType positions it at slot 4
    // (after customerVatStatus, customerVatData, customerName).
    let customer_name_pos = body
        .find("</customerName>")
        .expect("emitter must write </customerName> before <customerAddress>");
    let customer_address_pos = body
        .find("<customerAddress>")
        .expect("emitter must write <customerAddress>");
    let customer_info_close_pos = body
        .find("</customerInfo>")
        .expect("emitter must write </customerInfo>");
    assert!(
        customer_name_pos < customer_address_pos && customer_address_pos < customer_info_close_pos,
        "expected ordering customerName < customerAddress < /customerInfo; \
         got positions {customer_name_pos} / {customer_address_pos} / {customer_info_close_pos}; \
         body:\n{body}"
    );

    validate_invoice_data(&xml).expect(
        "v3.0 invariant check must pass — the validator now requires customerAddress under \
         DOMESTIC customerVatStatus, and the emitter writes it",
    );
}

/// PR-97 / ADR-0048 §4 — byte-verbatim pin on the PRIVATE_PERSON
/// branch's `<customerInfo>` block. NAV's `CUSTOMER_DATA_EXPECTED`
/// business rule is symmetric: it FORBIDS `<customerVatData>` when
/// `customerVatStatus = PRIVATE_PERSON` (the negation of the
/// PR-77 / domestic positive half). The pin asserts:
///
///   1. `<customerVatStatus>PRIVATE_PERSON</customerVatStatus>` literal
///      bytes (SCREAMING_SNAKE_CASE NAV wire token, NOT PascalCase).
///   2. NO `<customerVatData>` element anywhere in the emitted body.
///   3. NO `<customerTaxNumber>` block anywhere (the structured
///      Hungarian tax-number children that DOMESTIC requires).
///   4. NO `<customerName>` element (Session-154: NAV business rule
///      CUSTOMER_DATA_NOT_EXPECTED forbids it under PRIVATE_PERSON; §169
///      governs the printed PDF, not the wire).
///   5. The validator passes the round-trip — confirming the symmetric
///      ForbiddenChildUnderStatus rule does not falsely fire on a
///      compliant PRIVATE_PERSON body.
#[test]
fn emitter_writes_customer_info_under_private_person_omits_vat_data() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = NavParties {
        supplier: SupplierInfo {
            tax_number: "24904362-2-41".to_string(),
            name: "Aben Consulting Kft".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatero koz 6".to_string(),
        },
        customer: CustomerInfo {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            // PRIVATE_PERSON forbids tax_number — None matches the
            // partner-form invariant + the validator's symmetric rule.
            tax_number: None,
            name: "Kovács János".to_string(),
            // ADR-0048 §3 open-question #5 — address is optional under
            // PRIVATE_PERSON at the NAV wire layer; omit here to pin
            // the bare-minimum compliant body.
            address: None,
        },
    };

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed on the PRIVATE_PERSON minimal fixture");
    let body = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    assert!(
        body.contains("<customerVatStatus>PRIVATE_PERSON</customerVatStatus>"),
        "PRIVATE_PERSON wire token (SCREAMING_SNAKE) missing; body:\n{body}"
    );
    assert!(
        !body.contains("<customerVatData>"),
        "PRIVATE_PERSON buyer MUST NOT carry <customerVatData> \
         (NAV business-rule CUSTOMER_DATA_EXPECTED, negative half); body:\n{body}"
    );
    assert!(
        !body.contains("<customerTaxNumber>"),
        "PRIVATE_PERSON buyer MUST NOT carry <customerTaxNumber>; body:\n{body}"
    );
    // Session-154 (ADR-0048 amendment 2026-05-29) — `<customerName>` is
    // SUPPRESSED on the wire under PRIVATE_PERSON even though the operator
    // supplied "Kovács János" (it still renders on the §169 PDF). NAV
    // business rule CUSTOMER_DATA_NOT_EXPECTED forbids it server-side.
    assert!(
        !body.contains("<customerName>"),
        "PRIVATE_PERSON wire body must OMIT <customerName> \
         (NAV CUSTOMER_DATA_NOT_EXPECTED), even with an operator-supplied name; body:\n{body}"
    );
    assert!(
        !body.contains("<customerAddress>"),
        "PRIVATE_PERSON wire body must OMIT <customerAddress>; body:\n{body}"
    );

    validate_invoice_data(&xml).expect(
        "v3.0 invariant check must pass — PRIVATE_PERSON + absent customerVatData/Address \
         is the symmetric compliant body; the new ForbiddenChildUnderStatus rule must \
         NOT false-fire here",
    );
}

/// Session-154 (ADR-0048 amendment 2026-05-29) — INVERTS the
/// session-148 pin. `<customerName>` is SUPPRESSED on the NAV wire for
/// PRIVATE_PERSON buyers: NAV business rule CUSTOMER_DATA_NOT_EXPECTED
/// ("Magánszemély vevő adatai nem adhatók meg.") ABORTS a submit that
/// carries it (confirmed against invoice 31, 2026-05-29). §169's
/// buyer-name mandate is a printed-PDF obligation, NOT a wire one — the
/// PDF still renders the name. The wire body omits `<customerVatData>`,
/// `<customerName>`, AND `<customerAddress>` for natural-person buyers.
#[test]
fn emitter_writes_customer_info_under_private_person_omits_name_and_address() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = NavParties {
        supplier: SupplierInfo {
            tax_number: "24904362-2-41".to_string(),
            name: "Aben Consulting Kft".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatero koz 6".to_string(),
        },
        customer: CustomerInfo {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            // §169 populates the buyer name for the PDF; the wire emit
            // suppresses it under PRIVATE_PERSON (Session-154).
            name: "Teszt Magánszemély".to_string(),
            // Address present on the struct (§169 preflight populates it)
            // to prove the wire suppression holds even when populated.
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1011".to_string(),
                city: "Budapest".to_string(),
                street: "Fő utca 1.".to_string(),
            }),
        },
    };

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("emitter must succeed on the PRIVATE_PERSON fixture");
    let body = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    assert!(
        body.contains("<customerVatStatus>PRIVATE_PERSON</customerVatStatus>"),
        "PRIVATE_PERSON wire token still required; body:\n{body}"
    );
    assert!(
        !body.contains("<customerVatData>"),
        "PRIVATE_PERSON forbids <customerVatData>; body:\n{body}"
    );
    assert!(
        !body.contains("<customerName>"),
        "Session-154: PRIVATE_PERSON wire body must OMIT <customerName> \
         (NAV CUSTOMER_DATA_NOT_EXPECTED); body:\n{body}"
    );
    assert!(
        !body.contains("<customerAddress>"),
        "Session-154: PRIVATE_PERSON wire body must OMIT <customerAddress> even when \
         populated (NAV CUSTOMER_DATA_NOT_EXPECTED); body:\n{body}"
    );

    validate_invoice_data(&xml).expect(
        "v3.0 invariant check must pass — PRIVATE_PERSON + omitted name/address \
         is a valid minimal body",
    );
}

/// PR-97 / ADR-0048 §7 — defence-in-depth pin: the v1 emitter
/// loud-fails on `CustomerVatStatus::Other`. Preflight catches it
/// upstream as `CustomerVatStatusOtherNotSupportedV1`, but a buggy
/// caller that bypasses preflight must not escape an OTHER-shaped body
/// onto the wire (v1 has no community-VAT / third-state-tax-id
/// emission path).
#[test]
fn emitter_loud_fails_when_other_status_materialises() {
    let invoice = build_minimal_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = NavParties {
        supplier: SupplierInfo {
            tax_number: "24904362-2-41".to_string(),
            name: "Aben Consulting Kft".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatero koz 6".to_string(),
        },
        customer: CustomerInfo {
            customer_vat_status: CustomerVatStatus::Other,
            tax_number: None,
            name: "Foreign Buyer".to_string(),
            address: None,
        },
    };

    let err = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect_err("Other-status emit MUST loud-fail in v1 per ADR-0048 §7");
    let msg = err.to_string();
    assert!(
        msg.contains("Other") || msg.contains("v2"),
        "loud-fail message must name the v1-deferral reason; got: {msg}"
    );
}
