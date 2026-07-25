//! Modification emitter ↔ XSD-validator round-trip (PR-11, ADR-0024).
//!
//! Parallels `issue_storno_xml_round_trip.rs` for the MODIFY path.
//! The two sources of truth for "what NAV v3.0 `<InvoiceData>` for a
//! modification looks like in ABERP" are:
//!
//!   1. `apps/aberp/src/nav_xml.rs::render_modification_data` — the
//!      emitter.
//!   2. `crates/nav-xsd-validator/src/validate.rs::walk_invoice` +
//!      `walk_invoice_reference` — the allowlist (S381/F1 — the v2.0-only
//!      `<modificationIssueDate>` allowance was removed; the MODIFY
//!      `<invoiceReference>` now carries exactly the three NAV v3.0
//!      children, identical to STORNO).
//!
//! Divergence between them is the silent-rejection failure mode
//! `nav_xsd_validator_round_trip.rs`'s preamble names. This file
//! pins the MODIFY-shape leg of that pair-up.
//!
//! Live (env-gated, with-NAV) PR-11 tests are not added in this
//! commit. The full `issue_modification::run()` pipeline loads NAV
//! credentials from the keychain for the Actor identity (closes F15)
//! even though it does not call NAV; an env-gated live test would
//! mirror `submit_invoice_live.rs`'s shape and is named in the PR-11
//! commit message as PR-11 follow-on work.

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, ModificationReference, NavParties,
    SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, ReadyInvoice, SeriesCode, SeriesId,
};
use aberp_nav_xsd_validator::{validate_invoice_data, NAV_XSD_VERSION};
use time::OffsetDateTime;

fn build_minimal_modification_invoice() -> ReadyInvoice {
    // The modification is itself an invoice with its own sequence
    // number; here it gets seq=2 against the base's seq=1.
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 2,
        fiscal_year: 0,
        lines: vec![LineItem {
            description: "Corrected widget price".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            // Full-replace per ADR-0024 §4 — the modification body
            // carries the NEW effective values, not a delta. No
            // negation (contrast with STORNO emitter).
            unit_price: Huf(1200),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        }],
        issue_date: OffsetDateTime::now_utc(),
        // PR-84 — modification chains default both date fields to the
        // chain-issue's server-clock issue date (out of scope for PR-84
        // operator UX).
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

fn minimal_modification_reference() -> ModificationReference {
    ModificationReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        // S369 — the minimal fixture full-replaces a single-line base,
        // so base_line_count = 1 and the modification's CREATE line
        // numbers at base_line_count + 1 = 2.
        base_line_count: 1,
    }
}

/// The emitter's bytes for a minimal modification fixture must
/// validate cleanly. Pair-up between `render_modification_data` and
/// `walk_invoice` / `walk_invoice_reference`.
#[test]
fn modification_emitter_minimal_invoice_passes_validator() {
    let modification = build_minimal_modification_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = minimal_modification_reference();

    let xml = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
    )
    .expect("modification emitter must succeed on minimal fixture");

    match validate_invoice_data(&xml) {
        Ok(()) => {}
        Err(err) => panic!(
            "validator rejected modification emitter output for NAV v{NAV_XSD_VERSION}: {err}\n\
             --- bytes ---\n{}\n--- end bytes ---",
            String::from_utf8_lossy(&xml)
        ),
    }
}

/// S381/F1 — the MODIFY `<invoiceReference>` carries EXACTLY the three
/// NAV v3.0 children and MUST NOT carry `<modificationIssueDate>` (a
/// v2.0-only element, illegal in v3.0 — every MODIFY submission carrying
/// it was a guaranteed schema-fail). STORNO and MODIFY bodies are now
/// byte-identical at the reference level; the wire operation is derived
/// from the audit ledger (`submission_queue::operation_for_invoice`),
/// not sniffed from the body. This test pins the element's ABSENCE so a
/// future regression re-adding it is caught here.
#[test]
fn modification_xml_omits_modification_issue_date() {
    let modification = build_minimal_modification_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = ModificationReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 3, // pin a non-1 index to defend against literal-1 elision
        base_line_count: 1,    // S369 — single-line base
    };
    let xml = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
    )
    .unwrap();
    let body = std::str::from_utf8(&xml).expect("modification XML must be UTF-8");

    assert!(
        body.contains("<invoiceReference>"),
        "modification XML must contain <invoiceReference>: {body}"
    );
    assert!(
        !body.contains("modificationIssueDate"),
        "modification XML must NOT carry the v2.0-only <modificationIssueDate> (S381/F1): {body}"
    );
    assert!(
        body.contains("<originalInvoiceNumber>INV-default/00001</originalInvoiceNumber>"),
        "modification XML must carry the base invoice number verbatim: {body}"
    );
    assert!(
        body.contains("<modificationIndex>3</modificationIndex>"),
        "modification XML must carry the modification_index verbatim: {body}"
    );
    // Parity pin with STORNO: modifyWithoutMaster is false by
    // default for PR-11 (the migrated-from-Billingo path that would
    // set this true is deferred per ADR-0023 §4 / ADR-0024 §7 / F23).
    assert!(
        body.contains("<modifyWithoutMaster>false</modifyWithoutMaster>"),
        "modification XML must carry modifyWithoutMaster=false for PR-11: {body}"
    );
}

/// **Intent pin — full-replace, not negation.** The MODIFY emitter
/// MUST NOT negate line/summary amounts (contrast with the STORNO
/// emitter's negation). A test that only checked the validator
/// passes would still pass if the emitter accidentally negated
/// values — CLAUDE.md rule 9. Verifies ADR-0024 §4 in code.
#[test]
fn modification_xml_carries_positive_line_amounts() {
    let modification = build_minimal_modification_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = minimal_modification_reference();
    let xml = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
    )
    .unwrap();
    let body = std::str::from_utf8(&xml).unwrap();

    // The fixture line is quantity=2, unit_price=1200, vat=27%.
    // No negation: unit_price = 1200, net = 2 * 1200 = 2400,
    // vat = floor(2400 * 2700 / 10000) = 648, gross = 2400 + 648 = 3048.
    assert!(
        body.contains("<unitPrice>1200</unitPrice>"),
        "unit_price must remain positive (full-replace per ADR-0024 §4): {body}"
    );
    assert!(
        body.contains("<lineNetAmount>2400</lineNetAmount>"),
        "line net must remain positive: {body}"
    );
    assert!(
        body.contains("<lineVatAmount>648</lineVatAmount>"),
        "line vat must remain positive: {body}"
    );
    assert!(
        body.contains("<lineGrossAmountNormal>3048</lineGrossAmountNormal>"),
        "line gross must remain positive: {body}"
    );
    // Belt-and-suspenders: a negative sign on a line amount would
    // indicate the negation logic from STORNO leaked into MODIFY.
    assert!(
        !body.contains("<unitPrice>-"),
        "modification XML must not carry any negated unitPrice: {body}"
    );
}

/// ADR-0049 §NAV emit (session 156) — defense-in-depth for the
/// `<lineModificationReference>` fix on the MODIFY leg. The
/// `LINE_MODIFICATION_EXPECTED` gap was latent in the SHARED
/// `write_lines` path, so a modification body must ALSO carry the
/// per-line reference. Symmetric to
/// `storno_xml_carries_line_modification_reference_after_line_number`.
#[test]
fn modification_xml_carries_line_modification_reference() {
    let modification = build_minimal_modification_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = minimal_modification_reference();
    let xml = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
    )
    .unwrap();
    let body = std::str::from_utf8(&xml).unwrap();

    assert!(
        body.contains("<lineModificationReference>"),
        "modification line MUST carry <lineModificationReference> \
         (NAV LINE_MODIFICATION_EXPECTED); body:\n{body}"
    );
    // S369 — lineNumberReference CONTINUES PAST the base's line count.
    // The minimal fixture's base carries 1 line (base_line_count = 1),
    // so the modification's CREATE line numbers at 1 + 0 + 1 = 2. The
    // pre-S369 emit reused `1`, colliding with base line 1 → NAV
    // INVOICE_LINE_ALREADY_EXISTS (S370 prod incident).
    assert!(
        body.contains("<lineNumberReference>2</lineNumberReference>"),
        "lineNumberReference must continue past the base's line count \
         (base_line_count 1 → first modification line 2); body:\n{body}"
    );
    assert!(
        !body.contains("<lineNumberReference>1</lineNumberReference>"),
        "S369 regression guard — must NOT reuse base line number 1 \
         (NAV INVOICE_LINE_ALREADY_EXISTS); body:\n{body}"
    );
    assert!(
        body.contains("<lineOperation>CREATE</lineOperation>"),
        "S184 — lineOperation must be CREATE for a modification line per \
         NAV INVALID_LINE_OPERATION business rule; body:\n{body}"
    );
    assert!(
        !body.contains("<lineOperation>MODIFY</lineOperation>"),
        "S184 — must not emit MODIFY (regression guard: pre-S184 emit); body:\n{body}"
    );

    validate_invoice_data(&xml).expect(
        "modification body with <lineModificationReference> must pass the v3.0 invariant check",
    );
}

/// The modification emitter MUST format its OWN invoice number from
/// the passed series + modification's own sequence number (NOT the
/// base's). Symmetric to the STORNO test
/// `storno_xml_invoice_number_is_the_stornos_own_seq`. A swap of the
/// two is a class of bug the per-invoice export bundle would carry
/// forward unchecked.
#[test]
fn modification_xml_invoice_number_is_the_modifications_own_seq() {
    let mut modification = build_minimal_modification_invoice();
    modification.sequence_number = 42; // modification's own seq
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = ModificationReference {
        base_invoice_number: "INV-default/00007".to_string(), // base's
        modification_index: 1,
        base_line_count: 1, // S369 — single-line base
    };
    let xml = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
    )
    .unwrap();
    let body = std::str::from_utf8(&xml).unwrap();

    assert!(
        body.contains("<invoiceNumber>INV-default/00042</invoiceNumber>"),
        "modification's own invoice number must be INV-default/00042: {body}"
    );
    assert!(
        body.contains("<originalInvoiceNumber>INV-default/00007</originalInvoiceNumber>"),
        "originalInvoiceNumber must be INV-default/00007: {body}"
    );
}

// ──────────────────────────────────────────────────────────────────────
// S184 — reverse-regression + invariant pins (modification-side
// parallel of the storno tests in issue_storno_xml_round_trip.rs).
// ──────────────────────────────────────────────────────────────────────

fn minimal_parties_private_person() -> NavParties {
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
            // S154 / ADR-0048 — PRIVATE_PERSON suppresses tax + address.
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            name: "Kovács József".to_string(),
            address: None,
        },
    }
}

/// S184 reverse-regression pin — PRIVATE_PERSON modification MUST
/// emit `<customerVatStatus>` only; no customerName / customerAddress /
/// customerVatData on the NAV wire (S154 / ADR-0048 — NAV's
/// CUSTOMER_DATA_NOT_EXPECTED rule). Parallels the storno-side pin.
#[test]
fn modification_xml_private_person_emits_vat_status_only() {
    let modification = build_minimal_modification_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties_private_person();
    let reference = minimal_modification_reference();
    let xml = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
    )
    .expect("PRIVATE_PERSON modification renders");
    let body = std::str::from_utf8(&xml).unwrap();

    assert!(
        body.contains("<customerVatStatus>PRIVATE_PERSON</customerVatStatus>"),
        "PRIVATE_PERSON modification MUST declare its vatStatus; body:\n{body}"
    );
    assert!(
        !body.contains("<customerVatData>"),
        "S154 — PRIVATE_PERSON modification MUST NOT emit <customerVatData>; body:\n{body}"
    );
    assert!(
        !body.contains("<customerName>"),
        "S154 — PRIVATE_PERSON modification MUST NOT emit <customerName> on \
         the NAV wire (CUSTOMER_DATA_NOT_EXPECTED); body:\n{body}"
    );
    assert!(
        !body.contains("<customerAddress>"),
        "S154 — PRIVATE_PERSON modification MUST NOT emit <customerAddress> \
         on the NAV wire; body:\n{body}"
    );
    validate_invoice_data(&xml)
        .expect("PRIVATE_PERSON modification MUST pass the v3.0 invariant check");
}

/// S184 invariant pin — the modification's `<originalInvoiceNumber>`
/// must round-trip the caller-supplied `base_invoice_number` verbatim
/// across exotic shapes (the same drift class S184 closed at the call
/// site for storno). Parallels the storno-side test.
#[test]
fn modification_xml_original_invoice_number_round_trips_verbatim() {
    let cases = &[
        "INV-default/00001",
        "TEST-ABERP/2026/0042",
        "TEST-TEST-ABERP/2026/0042",
        "ABERP-2025/000017",
        "1/2026",
    ];
    for original_number in cases {
        let modification = build_minimal_modification_invoice();
        let series = SeriesCode::new("INV-default".to_string()).unwrap();
        let parties = minimal_parties();
        let reference = ModificationReference {
            base_invoice_number: (*original_number).to_string(),
            modification_index: 1,
            base_line_count: 1, // S369 — single-line base
        };
        let xml = nav_xml::render_modification_data(
            &modification,
            &series,
            &parties,
            &reference,
            Currency::Huf,
            None,
        )
        .expect("modification renders");
        let body = std::str::from_utf8(&xml).unwrap();
        let expected = format!("<originalInvoiceNumber>{original_number}</originalInvoiceNumber>");
        assert!(
            body.contains(&expected),
            "S184 — `<originalInvoiceNumber>` must round-trip verbatim. \
             Expected `{expected}`; body:\n{body}"
        );
    }
}

/// S184 invariant pin — the modification's lineOperation MUST be
/// `CREATE` across multi-line input variations. Parallels the storno
/// test; same `INVALID_LINE_OPERATION` business-rule evidence.
#[test]
fn modification_line_operation_is_create_across_input_variations() {
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = minimal_modification_reference();

    let mut multi = build_minimal_modification_invoice();
    multi.lines = vec![
        LineItem {
            description: "L1".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(1000),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        },
        LineItem {
            description: "L2".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            unit_price: Huf(500),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        },
        LineItem {
            description: "L3 (zero vat)".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(123),
            vat_rate_basis_points: 0,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        },
    ];
    let xml = nav_xml::render_modification_data(
        &multi,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
    )
    .unwrap();
    let body = std::str::from_utf8(&xml).unwrap();
    let create_count = body
        .matches("<lineOperation>CREATE</lineOperation>")
        .count();
    let modify_count = body
        .matches("<lineOperation>MODIFY</lineOperation>")
        .count();
    assert_eq!(
        create_count, 3,
        "every modification line must emit lineOperation=CREATE; body:\n{body}"
    );
    assert_eq!(
        modify_count, 0,
        "no modification line may emit lineOperation=MODIFY; body:\n{body}"
    );
    validate_invoice_data(&xml)
        .expect("multi-line modification with CREATE ops must pass the v3.0 invariant check");
}

/// S369/S372 headline pin (modification leg). Modification of a 2-line
/// base: the full-replace CREATE lines' `<lineNumberReference>` MUST
/// continue past the base — the first references 3 (base_line_count 2 + 0
/// + 1), NOT 1, which would collide with the base's recorded line 1 and
/// trip NAV's INVOICE_LINE_ALREADY_EXISTS (S370 prod incident). Their own
/// `<lineNumber>` stays document-local 1 (S372: only the reference carries
/// the offset; NAV LINE_NUMBER_NOT_SEQUENTIAL otherwise).
#[test]
fn modification_line_number_reference_continues_past_two_line_base() {
    let mut modification = build_minimal_modification_invoice();
    // A single appended/corrected line is enough to pin the offset; the
    // base's count, not the modification's, drives the continuation.
    modification.lines = vec![LineItem {
        description: "Corrected line".to_string(),
        quantity: rust_decimal::Decimal::from(1),
        unit_price: Huf(1200),
        vat_rate_basis_points: 2700,
        vat_rate_kind: aberp_billing::VatRateKind::Percent,
        note: None,
        unit: None,
    }];
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = ModificationReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count: 2,
    };
    let xml = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
    )
    .expect("modification of 2-line base renders");
    let body = std::str::from_utf8(&xml).unwrap();

    assert!(
        body.contains("<lineNumberReference>3</lineNumberReference>"),
        "modification's appended line of a 2-line base must reference line 3; body:\n{body}"
    );
    assert!(
        !body.contains("<lineNumberReference>1</lineNumberReference>")
            && !body.contains("<lineNumberReference>2</lineNumberReference>"),
        "S369 regression guard — must NOT reuse base line 1 or 2 \
         (NAV INVOICE_LINE_ALREADY_EXISTS); body:\n{body}"
    );
    assert!(
        body.contains("<lineNumber>1</lineNumber>"),
        "modification <lineNumber> stays document-local 1 — only the \
         reference carries the offset (S372 LINE_NUMBER_NOT_SEQUENTIAL); body:\n{body}"
    );

    validate_invoice_data(&xml)
        .expect("offset modification must still pass the v3.0 invariant check");
}
