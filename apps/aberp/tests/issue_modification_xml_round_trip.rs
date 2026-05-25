//! Modification emitter ↔ XSD-validator round-trip (PR-11, ADR-0024).
//!
//! Parallels `issue_storno_xml_round_trip.rs` for the MODIFY path.
//! The two sources of truth for "what NAV v3.0 `<InvoiceData>` for a
//! modification looks like in ABERP" are:
//!
//!   1. `apps/aberp/src/nav_xml.rs::render_modification_data` — the
//!      emitter.
//!   2. `crates/nav-xsd-validator/src/validate.rs::walk_invoice` +
//!      `walk_invoice_reference` — the allowlist (extended in PR-11
//!      to allow the optional `<modificationIssueDate>` per
//!      ADR-0024 §2).
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
    self, CustomerInfo, ModificationReference, NavParties, SupplierInfo,
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
            quantity: 2,
            // Full-replace per ADR-0024 §4 — the modification body
            // carries the NEW effective values, not a delta. No
            // negation (contrast with STORNO emitter).
            unit_price: Huf(1200),
            vat_rate_basis_points: 2700,
        }],
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
            tax_number: "87654321".to_string(),
            name: "Test Customer Zrt.".to_string(),
        },
    }
}

fn minimal_modification_reference() -> ModificationReference {
    ModificationReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        modification_issue_date: "2026-05-21".to_string(),
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

    let xml = nav_xml::render_modification_data(&modification, &series, &parties, &reference, Currency::Huf, None)
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

/// The MODIFY XML body MUST carry BOTH `<invoiceReference>` AND
/// `<modificationIssueDate>` — these are the two substrings the
/// detector `submit_invoice::detect_operation_from_xml` keys on per
/// ADR-0024 §3 (CLAUDE.md rule 5 — code answers, not LLM). If a
/// future refactor accidentally drops `<modificationIssueDate>` from
/// the emitter, `submit-invoice` would default to
/// `InvoiceOperation::Storno` (wrong operation) and NAV would reject
/// with `INVOICE_OPERATION_MISMATCH`-shape. This test pins the
/// detector's coupling to the emitter's structural choices.
#[test]
fn modification_xml_carries_invoice_reference_and_modification_issue_date() {
    let modification = build_minimal_modification_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = ModificationReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 3, // pin a non-1 index to defend against literal-1 elision
        modification_issue_date: "2026-05-21".to_string(),
    };
    let xml = nav_xml::render_modification_data(&modification, &series, &parties, &reference, Currency::Huf, None)
        .unwrap();
    let body = std::str::from_utf8(&xml).expect("modification XML must be UTF-8");

    assert!(
        body.contains("<invoiceReference>"),
        "modification XML must contain <invoiceReference>: {body}"
    );
    assert!(
        body.contains("<modificationIssueDate>2026-05-21</modificationIssueDate>"),
        "modification XML must carry <modificationIssueDate> verbatim: {body}"
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
    let xml = nav_xml::render_modification_data(&modification, &series, &parties, &reference, Currency::Huf, None)
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
        modification_issue_date: "2026-05-21".to_string(),
    };
    let xml = nav_xml::render_modification_data(&modification, &series, &parties, &reference, Currency::Huf, None)
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
