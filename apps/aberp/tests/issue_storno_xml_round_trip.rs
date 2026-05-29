//! Storno emitter ↔ XSD-validator round-trip (PR-10, ADR-0023).
//!
//! Parallels `nav_xsd_validator_round_trip.rs` for the storno path.
//! The two sources of truth for "what NAV v3.0 `<InvoiceData>` for a
//! storno looks like in ABERP" are:
//!
//!   1. `apps/aberp/src/nav_xml.rs::render_storno_data` — the emitter
//!   2. `crates/nav-xsd-validator/src/validate.rs::walk_invoice` +
//!      `walk_invoice_reference` — the allowlist
//!
//! Divergence between them is exactly the failure mode
//! `nav_xsd_validator_round_trip.rs`'s preamble names. This test pins
//! the storno-shape leg of that pair-up.
//!
//! Live (env-gated, with-NAV) PR-10 tests are not added in this
//! commit. The full `issue_storno::run()` pipeline loads NAV
//! credentials from the keychain for the Actor identity (closes F15)
//! even though it does not call NAV; an env-gated live test would
//! mirror `submit_invoice_live.rs`'s shape and is named in the PR-10
//! commit message as PR-10 follow-on work (no F number — it is
//! mechanical test plumbing, not a finding).

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, StornoReference,
    SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, ReadyInvoice, SeriesCode, SeriesId,
};
use aberp_nav_xsd_validator::{validate_invoice_data, NAV_XSD_VERSION};
use time::OffsetDateTime;

fn build_minimal_storno_invoice() -> ReadyInvoice {
    // The storno is itself an invoice with its own sequence number;
    // here it gets seq=2 against the base's seq=1.
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 2,
        fiscal_year: 0,
        lines: vec![LineItem {
            description: "Cancellation of widget".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            // Positive in the in-memory model — the emitter handles
            // negation by constructing a parallel negated Vec; see
            // `nav_xml::render_storno_data` doc comment.
            unit_price: Huf(1000),
            vat_rate_basis_points: 2700,
            note: None,
            unit: None,
        }],
        issue_date: OffsetDateTime::now_utc(),
        // PR-84 — storno chains default both date fields to the chain-
        // issue's server-clock issue date (out of scope for PR-84
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
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit
            // Domestic posture for legacy test fixtures.
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

fn minimal_storno_reference() -> StornoReference {
    StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
    }
}

/// The emitter's bytes for a minimal storno fixture must validate
/// cleanly. Pair-up between `render_storno_data` and
/// `walk_invoice`/`walk_invoice_reference`.
#[test]
fn storno_emitter_minimal_invoice_passes_validator() {
    let storno = build_minimal_storno_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = minimal_storno_reference();

    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .expect("storno emitter must succeed on minimal fixture");

    match validate_invoice_data(&xml) {
        Ok(()) => {}
        Err(err) => panic!(
            "validator rejected storno emitter output for NAV v{NAV_XSD_VERSION}: {err}\n\
             --- bytes ---\n{}\n--- end bytes ---",
            String::from_utf8_lossy(&xml)
        ),
    }
}

/// The storno XML body MUST carry the `<invoiceReference>` block —
/// it is the chain-link element that `submit_invoice::detect_operation_from_xml`
/// keys on (CLAUDE.md rule 5 — code answers, not LLM). If a future
/// refactor accidentally drops `<invoiceReference>` from the emitter,
/// `submit-invoice` would default to `InvoiceOperation::Create` and
/// NAV would reject the storno at the wire. This test pins the
/// detector's coupling to the emitter's structural choice.
#[test]
fn storno_xml_carries_invoice_reference_block() {
    let storno = build_minimal_storno_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 3, // pin a non-1 index to defend against literal 1 elision
    };
    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .unwrap();
    let body = std::str::from_utf8(&xml).expect("storno XML must be UTF-8");

    assert!(
        body.contains("<invoiceReference>"),
        "storno XML must contain <invoiceReference>; got: {body}"
    );
    assert!(
        body.contains("<originalInvoiceNumber>INV-default/00001</originalInvoiceNumber>"),
        "storno XML must carry the base invoice number verbatim; got: {body}"
    );
    assert!(
        body.contains("<modificationIndex>3</modificationIndex>"),
        "storno XML must carry the modification_index verbatim; got: {body}"
    );
    // modifyWithoutMaster is pinned to false for PR-10 (the migrated-
    // base path that would set this to true is deferred per ADR-0023
    // §4). A future PR landing the migrated path will update this
    // assertion to match the StornoReference field shape change.
    assert!(
        body.contains("<modifyWithoutMaster>false</modifyWithoutMaster>"),
        "storno XML must carry modifyWithoutMaster=false for PR-10; got: {body}"
    );
}

/// Negation invariant: storno's line/summary amounts in the XML must
/// be negative (NAV v3.0 storno convention). A test that only checked
/// the validator passes would still pass if the emitter accidentally
/// emitted positive amounts — CLAUDE.md rule 9 ("tests verify intent,
/// not just behavior"). This is the intent-pinning check.
#[test]
fn storno_xml_carries_negative_line_amounts() {
    let storno = build_minimal_storno_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = minimal_storno_reference();
    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .unwrap();
    let body = std::str::from_utf8(&xml).unwrap();

    // The fixture line is quantity=2, unit_price=1000, vat=27%. With
    // negation: unit_price = -1000, net = 2 * -1000 = -2000,
    // vat = floor(-2000 * 2700 / 10000) = floor(-540) = -540,
    // gross = -2000 + -540 = -2540.
    assert!(
        body.contains("<unitPrice>-1000</unitPrice>"),
        "unit_price must be negated: {body}"
    );
    assert!(
        body.contains("<lineNetAmount>-2000</lineNetAmount>"),
        "line net must be negated: {body}"
    );
    assert!(
        body.contains("<lineVatAmount>-540</lineVatAmount>"),
        "line vat must be negated: {body}"
    );
    assert!(
        body.contains("<lineGrossAmountNormal>-2540</lineGrossAmountNormal>"),
        "line gross must be negated: {body}"
    );
}

/// The storno emitter MUST format its own invoice number from the
/// passed series + storno's own sequence number (NOT the base's).
/// The base's number lives only inside `<invoiceReference>/<originalInvoiceNumber>`.
/// A swap of the two is a class of bug the per-invoice export bundle
/// would carry forward unchecked — pin it here.
#[test]
fn storno_xml_invoice_number_is_the_stornos_own_seq() {
    let mut storno = build_minimal_storno_invoice();
    storno.sequence_number = 42; // storno's own seq
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = StornoReference {
        base_invoice_number: "INV-default/00007".to_string(), // base's
        modification_index: 1,
    };
    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .unwrap();
    let body = std::str::from_utf8(&xml).unwrap();

    // The OUTER invoiceNumber is the storno's own.
    assert!(
        body.contains("<invoiceNumber>INV-default/00042</invoiceNumber>"),
        "storno's own invoice number must be INV-default/00042: {body}"
    );
    // The originalInvoiceNumber is the base's.
    assert!(
        body.contains("<originalInvoiceNumber>INV-default/00007</originalInvoiceNumber>"),
        "originalInvoiceNumber must be INV-default/00007: {body}"
    );
}

/// ADR-0049 §NAV emit (session 156) — the storno body MUST carry a
/// `<lineModificationReference>` on every `<line>`, or NAV ABORTS the
/// submit with business rule `LINE_MODIFICATION_EXPECTED`. The reference
/// carries `<lineNumberReference>` (the original line's position) +
/// `<lineOperation>MODIFY</lineOperation>`, positioned directly AFTER
/// `<lineNumber>` per NAV `LineType` ordering. This is the intent-pin
/// for the fix that unblocked a SAVED storno end-to-end.
#[test]
fn storno_xml_carries_line_modification_reference_after_line_number() {
    let storno = build_minimal_storno_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = minimal_storno_reference();
    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .unwrap();
    let body = std::str::from_utf8(&xml).unwrap();

    assert!(
        body.contains("<lineModificationReference>"),
        "storno line MUST carry <lineModificationReference> \
         (NAV LINE_MODIFICATION_EXPECTED); body:\n{body}"
    );
    assert!(
        body.contains("<lineNumberReference>1</lineNumberReference>"),
        "lineNumberReference must be the original line position (1); body:\n{body}"
    );
    assert!(
        body.contains("<lineOperation>MODIFY</lineOperation>"),
        "lineOperation must be MODIFY for a storno line; body:\n{body}"
    );

    // Ordering: <lineNumber> first, then <lineModificationReference>,
    // then <lineExpressionIndicator> — NAV LineType sequence. The
    // session-155 memo said "first child", but NAV requires <lineNumber>
    // to be the literal first child; the reference is the second element.
    let line_number_pos = body
        .find("</lineNumber>")
        .expect("storno line must write <lineNumber>");
    let line_mod_pos = body
        .find("<lineModificationReference>")
        .expect("storno line must write <lineModificationReference>");
    let line_expr_pos = body
        .find("<lineExpressionIndicator>")
        .expect("storno line must write <lineExpressionIndicator>");
    assert!(
        line_number_pos < line_mod_pos && line_mod_pos < line_expr_pos,
        "expected ordering lineNumber < lineModificationReference < lineExpressionIndicator; \
         got {line_number_pos} / {line_mod_pos} / {line_expr_pos}; body:\n{body}"
    );

    // Round-trip: the body carrying the new element must still validate.
    validate_invoice_data(&xml)
        .expect("storno body with <lineModificationReference> must pass the v3.0 invariant check");
}
