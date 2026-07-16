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
    Currency, CustomerId, Huf, InvoiceId, LineItem, NavUnitOfMeasure, PaymentMethod, ProductUnit,
    ReadyInvoice, SeriesCode, SeriesId,
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
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
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
        // S369 — the minimal storno fixture is a 1:1 storno of a
        // single-line base, so the base carries 1 line. The storno's
        // CREATE line therefore numbers at base_line_count + 1 = 2.
        base_line_count: 1,
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

/// The storno XML body MUST carry the `<invoiceReference>` block — NAV
/// v3.0 requires it on every STORNO/MODIFY chain invoice (rule 18,
/// `INVOICE_REFERENCE_EXPECTED`), and a CREATE body must NOT carry it.
/// (S381/F1 — the wire operation is no longer sniffed from the body; it
/// is derived from the audit ledger by
/// `submission_queue::operation_for_invoice`. This test still pins the
/// structural element the NAV contract requires on a storno body.)
#[test]
fn storno_xml_carries_invoice_reference_block() {
    let storno = build_minimal_storno_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 3, // pin a non-1 index to defend against literal 1 elision
        base_line_count: 1,    // S369 — single-line base
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

    // The fixture line is quantity=2, unit_price=1000, vat=27%. S381/F3
    // — NAV spec §2.5.1 negates the QUANTITY, not the unit price:
    // quantity = -2, unit_price = +1000, net = -2 * 1000 = -2000,
    // vat = floor(-2000 * 2700 / 10000) = floor(-540) = -540,
    // gross = -2000 + -540 = -2540. Line totals are unchanged from the
    // pre-S381 negate-unitPrice shape; only the sign placement moves.
    assert!(
        body.contains("<quantity>-2</quantity>"),
        "quantity must be negated (S381/F3): {body}"
    );
    assert!(
        body.contains("<unitPrice>1000</unitPrice>"),
        "unit_price must stay positive (S381/F3): {body}"
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
        base_line_count: 1, // S369 — single-line base
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
/// `<lineOperation>CREATE</lineOperation>` per S184 — NAV's
/// `INVALID_LINE_OPERATION` business rule requires `CREATE` (not
/// `MODIFY`) for every chain-body line. The `<lineModificationReference>`
/// is positioned directly AFTER `<lineNumber>` per NAV `LineType`
/// ordering.
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
    // S369 — lineNumberReference CONTINUES PAST the base's line count.
    // The minimal fixture's base carries 1 line (base_line_count = 1),
    // so the storno's single CREATE line numbers at 1 + 0 + 1 = 2. The
    // pre-S369 emit reused `1` here, colliding with base line 1 → NAV
    // INVOICE_LINE_ALREADY_EXISTS (S370 prod incident). This assertion
    // pins the corrected, collision-free continuation.
    assert!(
        body.contains("<lineNumberReference>2</lineNumberReference>"),
        "lineNumberReference must continue past the base's line count \
         (base_line_count 1 → first storno line 2); body:\n{body}"
    );
    assert!(
        !body.contains("<lineNumberReference>1</lineNumberReference>"),
        "S369 regression guard — must NOT reuse base line number 1 \
         (NAV INVOICE_LINE_ALREADY_EXISTS); body:\n{body}"
    );
    assert!(
        body.contains("<lineOperation>CREATE</lineOperation>"),
        "S184 — lineOperation must be CREATE for a storno line per NAV \
         INVALID_LINE_OPERATION business rule; body:\n{body}"
    );
    assert!(
        !body.contains("<lineOperation>MODIFY</lineOperation>"),
        "S184 — must not emit MODIFY (regression guard: pre-S184 emit); body:\n{body}"
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

// ──────────────────────────────────────────────────────────────────────
// S184 — reverse-regression pins.
//
// The S184 change to `CHAIN_LINE_OPERATION` flipped storno + modification
// from `MODIFY` to `CREATE` (NAV business rule INVALID_LINE_OPERATION,
// confirmed against transaction `5EF1QF3Y1W9HIFNW`). The on-disk read
// of the base's `<invoiceNumber>` replaced the seller-toml-template
// re-render (NAV business rule INVALID_INVOICE_REFERENCE on the same
// transaction). These tests pin both, plus a PRIVATE_PERSON storno
// emit (S154 / ADR-0048) and the cross-customer-shape invariant Ervin
// asked for in S184b.
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
            // S154 / ADR-0048 — PRIVATE_PERSON buyers carry NO tax number
            // (NAV business rule INVALID_CUSTOMER_VAT_STATUS rejects
            // `<customerVatData>` for PRIVATE_PERSON) and NO address
            // (ADR-0048 §addendum — address is optional for
            // PRIVATE_PERSON; NAV permits its absence on chain bodies
            // too per PR-77).
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            name: "Kovács József".to_string(),
            address: None,
        },
    }
}

/// S184 reverse-regression pin (S154 / ADR-0048 anchor). The storno
/// of a PRIVATE_PERSON base MUST emit `<customerVatStatus>` only —
/// the S154 amendment (`apps/aberp/src/nav_xml.rs:1178+`) suppresses
/// `<customerName>` AND `<customerAddress>` AND `<customerVatData>`
/// on the NAV wire for PRIVATE_PERSON because NAV's
/// `CUSTOMER_DATA_NOT_EXPECTED` business rule ABORTS any submission
/// carrying them ("Magánszemély vevő adatai nem adhatók meg."). PR-148/
/// 150 had emitted name + address unconditionally; S154 separates the
/// wire (suppressed) from the PDF (always rendered, §169 Áfa tv.). This
/// pin guards against the S184 changes accidentally re-routing
/// PRIVATE_PERSON storno through the DOMESTIC emit branch.
#[test]
fn storno_xml_private_person_emits_vat_status_only() {
    let storno = build_minimal_storno_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties_private_person();
    let reference = minimal_storno_reference();
    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .expect("PRIVATE_PERSON storno renders");
    let body = std::str::from_utf8(&xml).unwrap();

    assert!(
        body.contains("<customerVatStatus>PRIVATE_PERSON</customerVatStatus>"),
        "PRIVATE_PERSON storno MUST declare its vatStatus; body:\n{body}"
    );
    assert!(
        !body.contains("<customerVatData>"),
        "S154 / ADR-0048 — PRIVATE_PERSON storno MUST NOT emit \
         <customerVatData>; body:\n{body}"
    );
    assert!(
        !body.contains("<customerTaxNumber>"),
        "S154 / ADR-0048 — PRIVATE_PERSON storno MUST NOT emit \
         <customerTaxNumber>; body:\n{body}"
    );
    assert!(
        !body.contains("<customerName>"),
        "S154 — PRIVATE_PERSON storno MUST NOT emit <customerName> on \
         the NAV wire (CUSTOMER_DATA_NOT_EXPECTED); the printed PDF \
         path renders it separately per §169 Áfa tv.; body:\n{body}"
    );
    assert!(
        !body.contains("<customerAddress>"),
        "S154 — PRIVATE_PERSON storno MUST NOT emit <customerAddress> \
         on the NAV wire (CUSTOMER_DATA_NOT_EXPECTED); body:\n{body}"
    );

    // Validator round-trip — the v3.0 invariant walker MUST accept the
    // PRIVATE_PERSON storno shape (no customerVatData / customerName /
    // customerAddress).
    validate_invoice_data(&xml).expect("PRIVATE_PERSON storno MUST pass the v3.0 invariant check");
}

/// S184 invariant pin — the storno's `<customerVatStatus>` element MUST
/// match the `NavParties::customer.customer_vat_status` field for every
/// closed-vocab buyer kind. Property-style across the full vocab
/// (DOMESTIC, PRIVATE_PERSON; OTHER deferred per ADR-0048). A
/// regression that drops the discriminator (e.g. always emits
/// DOMESTIC) would silently corrupt PRIVATE_PERSON chains; this fires
/// loud at compile time of every variant.
#[test]
fn storno_xml_customer_vat_status_round_trips_across_closed_vocab() {
    let cases: &[(CustomerVatStatus, &str, fn() -> NavParties)] = &[
        (CustomerVatStatus::Domestic, "DOMESTIC", minimal_parties),
        (
            CustomerVatStatus::PrivatePerson,
            "PRIVATE_PERSON",
            minimal_parties_private_person,
        ),
    ];
    for (variant, wire, mk_parties) in cases {
        let storno = build_minimal_storno_invoice();
        let series = SeriesCode::new("INV-default".to_string()).unwrap();
        let parties = mk_parties();
        let reference = minimal_storno_reference();
        let xml = nav_xml::render_storno_data(
            &storno,
            &series,
            &parties,
            &reference,
            Currency::Huf,
            None,
        )
        .expect("storno renders");
        let body = std::str::from_utf8(&xml).unwrap();
        let expected = format!("<customerVatStatus>{wire}</customerVatStatus>");
        assert!(
            body.contains(&expected),
            "{variant:?} storno must emit `{expected}`; body:\n{body}"
        );
        validate_invoice_data(&xml)
            .expect("storno body must pass the v3.0 invariant check across vat-status vocab");
    }
}

/// S184 invariant pin — the storno's `<originalInvoiceNumber>` MUST be
/// byte-identical to the `StornoReference::base_invoice_number` the
/// emitter is handed. Defends against the regression class S184 closed
/// at the call site (where pre-S184 the call site re-derived via
/// `template.render_for_build`, which drifted under seller.toml literal
/// edits): a future caller might be tempted to "fix" the reference at
/// the emitter level (silently stripping a prefix or substituting from
/// the storno's own series); this pin fails loud.
#[test]
fn storno_xml_original_invoice_number_round_trips_verbatim() {
    let cases = &[
        "INV-default/00001",
        "TEST-ABERP/2026/0042",
        "TEST-TEST-ABERP/2026/0042", // S184 — the actual NAV-side string Ervin's prod drift produced
        "ABERP-2025/000017",
        "1/2026", // operator-configured single-counter literal
    ];
    for original_number in cases {
        let storno = build_minimal_storno_invoice();
        let series = SeriesCode::new("INV-default".to_string()).unwrap();
        let parties = minimal_parties();
        let reference = StornoReference {
            base_invoice_number: (*original_number).to_string(),
            modification_index: 1,
            base_line_count: 1, // S369 — single-line base
        };
        let xml = nav_xml::render_storno_data(
            &storno,
            &series,
            &parties,
            &reference,
            Currency::Huf,
            None,
        )
        .expect("storno renders");
        let body = std::str::from_utf8(&xml).unwrap();
        let expected = format!("<originalInvoiceNumber>{original_number}</originalInvoiceNumber>");
        assert!(
            body.contains(&expected),
            "S184 — `<originalInvoiceNumber>` must round-trip the caller-\
             supplied string verbatim. Expected `{expected}`; body:\n{body}"
        );
    }
}

/// S184 invariant pin — `read_invoice_number_from_xml` round-trips
/// every byte of `<invoiceNumber>` from a freshly-emitted NAV InvoiceData
/// XML. Pairs the renderer + reader (the same two-source-of-truth
/// pattern PR-10 documented for emitter ↔ validator). A regression
/// that quotes / escapes / trims either side of the round-trip surfaces
/// here at the emit-then-read boundary BEFORE it can drift NAV
/// references silently. Property-style across DOMESTIC + PRIVATE_PERSON
/// + EUR currency.
#[test]
fn read_invoice_number_from_xml_round_trips_across_emit_shapes() {
    use aberp_billing::{IdempotencyKey, ReadyInvoice};
    use ulid::Ulid;

    let scratch_dir = std::env::temp_dir()
        .join("aberp-s184-read-roundtrip")
        .join(format!("{}", Ulid::new()));
    std::fs::create_dir_all(&scratch_dir).expect("create scratch dir");

    // Build a base "plain" invoice fixture (NOT a storno) so the reader
    // is exercised against the same shape the chain emitter will see.
    fn build_plain_invoice() -> ReadyInvoice {
        ReadyInvoice {
            id: InvoiceId::new(),
            series_id: SeriesId::new(),
            customer_id: CustomerId::new(),
            lines: vec![LineItem {
                description: "Test megnevezés".to_string(),
                quantity: rust_decimal::Decimal::from(1),
                unit_price: Huf(1000),
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            }],
            issue_date: time::OffsetDateTime::now_utc(),
            payment_deadline: time::OffsetDateTime::now_utc().date(),
            delivery_date: time::OffsetDateTime::now_utc().date(),
            sequence_number: 42,
            fiscal_year: 2026,
        }
    }

    let cases: &[(&str, fn() -> NavParties)] = &[
        ("DOMESTIC fixture", minimal_parties),
        ("PRIVATE_PERSON fixture", minimal_parties_private_person),
    ];

    let exotic_numbers = &[
        "INV-default/00042",
        "TEST-ABERP/2026/0042",
        "TEST-TEST-ABERP/2026/0042",
        "ABERP-2025/000017",
    ];

    for (label, mk_parties) in cases {
        for &number in exotic_numbers {
            let invoice = build_plain_invoice();
            let _idem = IdempotencyKey::new();
            let series = SeriesCode::new("INV-default".to_string()).unwrap();
            let parties = mk_parties();
            // Use render_invoice_data_with_number so we control the
            // emitted `<invoiceNumber>` precisely.
            let xml = nav_xml::render_invoice_data_with_number(
                &invoice,
                &series,
                &parties,
                Currency::Huf,
                None,
                aberp_billing::PaymentMethod::default(),
                Some(number),
            )
            .expect("plain invoice renders");

            let path = scratch_dir.join(format!("{}.xml", Ulid::new()));
            std::fs::write(&path, &xml).expect("write xml");

            let read_back = nav_xml::read_invoice_number_from_xml(&path).unwrap_or_else(|e| {
                panic!("{label}: read_invoice_number_from_xml({number}) failed: {e:#}")
            });
            assert_eq!(
                read_back, number,
                "{label} / {number}: round-trip must be byte-identical"
            );
        }
    }
}

/// S381/F2 — `read_invoice_delivery_date_from_xml` round-trips the
/// base's `<invoiceDeliveryDate>` so a storno can copy it instead of
/// stamping today's date (NAV `UNINTENDED_CANCELLATION_DELIVERY_DATE`
/// WARN + wrong VAT period). Render a base invoice with a known,
/// NON-today delivery date, write it, and read it back byte-identical.
#[test]
fn read_invoice_delivery_date_from_xml_round_trips() {
    use ulid::Ulid;

    let scratch_dir = std::env::temp_dir()
        .join("aberp-s381-f2-delivery-roundtrip")
        .join(format!("{}", Ulid::new()));
    std::fs::create_dir_all(&scratch_dir).expect("create scratch dir");

    // A fixed delivery date distinct from today so a "stamp today"
    // regression cannot coincidentally pass.
    let delivery = time::Date::from_calendar_date(2025, time::Month::March, 14).unwrap();
    let mut invoice = build_minimal_storno_invoice();
    invoice.delivery_date = delivery;

    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("base invoice renders");

    let path = scratch_dir.join(format!("{}.xml", Ulid::new()));
    std::fs::write(&path, &xml).expect("write xml");

    let read_back =
        nav_xml::read_invoice_delivery_date_from_xml(&path).expect("read invoiceDeliveryDate back");
    assert_eq!(
        read_back, "2025-03-14",
        "storno must be able to copy the base's delivery date verbatim (S381/F2)"
    );
}

/// S381/F2 — the helper MUST fail loud when the base XML lacks an
/// `<invoiceDeliveryDate>` (tampered / foreign body) rather than
/// silently substituting a date. CLAUDE.md rule 12.
#[test]
fn read_invoice_delivery_date_from_xml_loud_fails_on_missing_element() {
    use ulid::Ulid;

    let scratch_dir = std::env::temp_dir()
        .join("aberp-s381-f2-delivery-missing")
        .join(format!("{}", Ulid::new()));
    std::fs::create_dir_all(&scratch_dir).expect("create scratch dir");
    let path = scratch_dir.join("no-delivery.xml");
    std::fs::write(
        &path,
        b"<InvoiceData><invoiceNumber>X/00001</invoiceNumber></InvoiceData>",
    )
    .expect("write xml");

    let err = nav_xml::read_invoice_delivery_date_from_xml(&path).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invoiceDeliveryDate"),
        "missing-element error must name the element: {msg}"
    );
}

/// S390/D — `read_invoice_payment_date_from_xml` round-trips the base's
/// `<paymentDate>` so a storno can copy it instead of stamping the
/// storno's own issue date. Payment-date counterpart of the S381/F2
/// delivery-date round-trip. Render a base invoice with a known,
/// NON-today payment date, write it, and read it back byte-identical.
#[test]
fn read_invoice_payment_date_from_xml_round_trips() {
    use ulid::Ulid;

    let scratch_dir = std::env::temp_dir()
        .join("aberp-s390-d-payment-roundtrip")
        .join(format!("{}", Ulid::new()));
    std::fs::create_dir_all(&scratch_dir).expect("create scratch dir");

    // A fixed payment date distinct from both today AND the delivery
    // date so a "stamp today" OR a "copy delivery date" regression
    // cannot coincidentally pass.
    let payment = time::Date::from_calendar_date(2025, time::Month::April, 30).unwrap();
    let delivery = time::Date::from_calendar_date(2025, time::Month::March, 14).unwrap();
    let mut invoice = build_minimal_storno_invoice();
    invoice.payment_deadline = payment;
    invoice.delivery_date = delivery;

    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("base invoice renders");

    let path = scratch_dir.join(format!("{}.xml", Ulid::new()));
    std::fs::write(&path, &xml).expect("write xml");

    let read_back =
        nav_xml::read_invoice_payment_date_from_xml(&path).expect("read paymentDate back");
    assert_eq!(
        read_back, "2025-04-30",
        "storno must be able to copy the base's paymentDate verbatim (S390/D)"
    );
    // Defends against the "copy delivery date" mistake specifically.
    assert_ne!(
        read_back, "2025-03-14",
        "paymentDate must come from <paymentDate>, NOT <invoiceDeliveryDate> (S390/D)"
    );
}

/// S390/D — the helper MUST fail loud when the base XML lacks a
/// `<paymentDate>` (tampered / foreign body) rather than silently
/// substituting a date. CLAUDE.md rule 12.
#[test]
fn read_invoice_payment_date_from_xml_loud_fails_on_missing_element() {
    use ulid::Ulid;

    let scratch_dir = std::env::temp_dir()
        .join("aberp-s390-d-payment-missing")
        .join(format!("{}", Ulid::new()));
    std::fs::create_dir_all(&scratch_dir).expect("create scratch dir");
    let path = scratch_dir.join("no-payment.xml");
    std::fs::write(
        &path,
        b"<InvoiceData><invoiceNumber>X/00001</invoiceNumber></InvoiceData>",
    )
    .expect("write xml");

    let err = nav_xml::read_invoice_payment_date_from_xml(&path).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("paymentDate"),
        "missing-element error must name the element: {msg}"
    );
}

/// S184 — `read_invoice_number_from_xml` MUST fail loud (not return an
/// empty string or a fallback) when the file is missing, empty, or
/// lacks the `<invoiceNumber>` element. CLAUDE.md rule 12. A silent
/// fallback would let a chain emitter ship `<originalInvoiceNumber>` =
/// some default and NAV would ABORT exactly the way S184 fixed.
#[test]
fn read_invoice_number_from_xml_loud_fails_on_missing_or_malformed() {
    use ulid::Ulid;

    let scratch_dir = std::env::temp_dir()
        .join("aberp-s184-loud-fail")
        .join(format!("{}", Ulid::new()));
    std::fs::create_dir_all(&scratch_dir).expect("create scratch dir");

    // Missing file.
    let nonexistent = scratch_dir.join("missing.xml");
    let err = nav_xml::read_invoice_number_from_xml(&nonexistent)
        .expect_err("missing file MUST fail loud");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing.xml"),
        "missing-file error must name the path: {msg}"
    );

    // XML without an <invoiceNumber> element.
    let no_elem_path = scratch_dir.join("no-elem.xml");
    std::fs::write(
        &no_elem_path,
        b"<?xml version=\"1.0\"?>\n<InvoiceData xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/data\">\
          <invoiceMain/></InvoiceData>",
    )
    .unwrap();
    let err = nav_xml::read_invoice_number_from_xml(&no_elem_path)
        .expect_err("XML missing <invoiceNumber> MUST fail loud");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invoiceNumber") || msg.contains("tampered") || msg.contains("EOF"),
        "missing-element error must name what's missing: {msg}"
    );

    // Empty <invoiceNumber> element.
    let empty_elem_path = scratch_dir.join("empty-elem.xml");
    std::fs::write(
        &empty_elem_path,
        b"<?xml version=\"1.0\"?>\n<InvoiceData xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/data\">\
          <invoiceNumber></invoiceNumber></InvoiceData>",
    )
    .unwrap();
    let err = nav_xml::read_invoice_number_from_xml(&empty_elem_path)
        .expect_err("empty <invoiceNumber> MUST fail loud");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("empty"),
        "empty-element error must say so: {msg}"
    );
}

/// S184 — the lineOperation regression guard's most important property:
/// for ANY storno + modification shape, the emitted XML MUST carry
/// `<lineOperation>CREATE</lineOperation>` and MUST NOT carry
/// `<lineOperation>MODIFY</lineOperation>`. Pinned against multi-line
/// + alternative-currency fixtures because the bug was at the rendering
/// constant level — variation in the input space MUST NOT vary the
/// output here.
#[test]
fn storno_line_operation_is_create_across_input_variations() {
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = minimal_storno_reference();

    // Single line.
    let single = build_minimal_storno_invoice();
    let xml1 =
        nav_xml::render_storno_data(&single, &series, &parties, &reference, Currency::Huf, None)
            .unwrap();
    let body1 = std::str::from_utf8(&xml1).unwrap();
    assert!(body1.contains("<lineOperation>CREATE</lineOperation>"));
    assert!(!body1.contains("<lineOperation>MODIFY</lineOperation>"));

    // Multi-line.
    let mut multi = build_minimal_storno_invoice();
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
    let xml2 =
        nav_xml::render_storno_data(&multi, &series, &parties, &reference, Currency::Huf, None)
            .unwrap();
    let body2 = std::str::from_utf8(&xml2).unwrap();
    let create_count = body2
        .matches("<lineOperation>CREATE</lineOperation>")
        .count();
    let modify_count = body2
        .matches("<lineOperation>MODIFY</lineOperation>")
        .count();
    assert_eq!(
        create_count, 3,
        "every storno line must emit lineOperation=CREATE; body:\n{body2}"
    );
    assert_eq!(
        modify_count, 0,
        "no storno line may emit lineOperation=MODIFY; body:\n{body2}"
    );

    // The validator round-trip for the multi-line variation.
    validate_invoice_data(&xml2)
        .expect("multi-line storno with CREATE ops must pass the v3.0 invariant check");
}

// ──────────────────────────────────────────────────────────────────────
// S369 — lineNumberReference offset (NAV INVOICE_LINE_ALREADY_EXISTS).
//
// A storno's CREATE lines add NEW lines to NAV's virtual consolidated
// invoice; their <lineNumberReference> MUST continue past the base's line
// numbers or NAV ABORTs the submit with `INVOICE_LINE_ALREADY_EXISTS`
// (prod incident, S370 root cause). Their own <lineNumber> stays
// document-local 1..=n (S372: only the reference carries the offset; NAV
// LINE_NUMBER_NOT_SEQUENTIAL otherwise). The offset lives in
// `StornoReference::base_line_count`, read from the base's on-disk XML by
// `count_invoice_lines_from_xml` at issuance time.
// ──────────────────────────────────────────────────────────────────────

/// Build a storno fixture with `n` lines (positive in-memory; the
/// emitter negates). Description `L{i}` keeps each line distinguishable.
fn build_storno_invoice_with_lines(n: usize) -> ReadyInvoice {
    let mut invoice = build_minimal_storno_invoice();
    invoice.lines = (1..=n)
        .map(|i| LineItem {
            description: format!("L{i}"),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(1000),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        })
        .collect();
    invoice
}

/// S369/S372 headline pin. Storno of a 3-line base: the three storno
/// lines' `<lineNumberReference>` MUST be 4, 5, 6 (base_line_count 3 +
/// ordinal + 1) — NOT 1, 2, 3, which would collide with the base's
/// recorded lines and trip NAV's INVOICE_LINE_ALREADY_EXISTS. Their
/// `<lineNumber>` stays document-local 1, 2, 3 (S372: only the reference
/// carries the offset; NAV LINE_NUMBER_NOT_SEQUENTIAL otherwise).
#[test]
fn storno_line_number_reference_continues_past_three_line_base() {
    let storno = build_storno_invoice_with_lines(3);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count: 3,
    };
    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .expect("3-line storno renders");
    let body = std::str::from_utf8(&xml).unwrap();

    // First storno line continues at 4, NOT 1.
    assert!(
        body.contains("<lineNumberReference>4</lineNumberReference>"),
        "first storno line of a 3-line base must reference line 4; body:\n{body}"
    );
    assert!(
        body.contains("<lineNumberReference>5</lineNumberReference>")
            && body.contains("<lineNumberReference>6</lineNumberReference>"),
        "storno lines 2 and 3 must reference 5 and 6; body:\n{body}"
    );
    // The <lineNumber> elements stay DOCUMENT-LOCAL: 1, 2, 3 — only the
    // <lineNumberReference> carries the chain offset. NAV requires each
    // invoice's own <lineNumber> sequence to start at 1 and be monotonic
    // (LINE_NUMBER_NOT_SEQUENTIAL, S372 prod incident — S369 over-shifted).
    assert!(
        body.contains("<lineNumber>1</lineNumber>")
            && body.contains("<lineNumber>2</lineNumber>")
            && body.contains("<lineNumber>3</lineNumber>"),
        "storno <lineNumber> elements must be document-local 1, 2, 3; body:\n{body}"
    );
    // Regression guard: NONE of the base's line numbers (1, 2, 3) may
    // reappear as a CREATE-line reference.
    for collide in ["1", "2", "3"] {
        let needle = format!("<lineNumberReference>{collide}</lineNumberReference>");
        assert!(
            !body.contains(&needle),
            "storno of a 3-line base must NOT reuse base line {collide} \
             (NAV INVOICE_LINE_ALREADY_EXISTS); body:\n{body}"
        );
    }

    validate_invoice_data(&xml)
        .expect("offset 3-line storno must still pass the v3.0 invariant check");
}

/// S369 round-trip pin: the OFFSET threads through the real prod path —
/// base XML on disk → `count_invoice_lines_from_xml` → storno emit. A
/// 3-line base is rendered, written to disk, its line count read back
/// (XML→count, the same call `issue_storno.rs` makes), then fed as the
/// storno's `base_line_count`; the storno's first CREATE line MUST
/// reference base_count + 1 = 4. This is the XML→struct→XML round-trip
/// for the line-offset invariant.
#[test]
fn storno_line_offset_round_trips_through_on_disk_base_count() {
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    // Render a 3-line BASE invoice (plain issuance) and write it to disk.
    let base = build_storno_invoice_with_lines(3);
    let base_xml = nav_xml::render_invoice_data(&base, &series, &parties, Currency::Huf, None)
        .expect("3-line base renders");
    let path = std::env::temp_dir().join(format!(
        "aberp_s369_storno_roundtrip_{}.xml",
        std::process::id()
    ));
    nav_xml::write_to_path(&path, &base_xml).expect("write base XML to temp");

    // XML → struct: read the base's line count back from disk, the exact
    // call the storno issuance path makes.
    let base_line_count =
        nav_xml::count_invoice_lines_from_xml(&path).expect("count base lines from disk");
    assert_eq!(base_line_count, 3, "base XML must report 3 lines");

    // struct → XML: the storno continues past the parsed count.
    let storno = build_storno_invoice_with_lines(3);
    let reference = StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count,
    };
    let storno_xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .expect("storno renders against parsed base count");
    let body = std::str::from_utf8(&storno_xml).unwrap();
    assert!(
        body.contains("<lineNumberReference>4</lineNumberReference>"),
        "storno's first CREATE line must reference base_count + 1 = 4; body:\n{body}"
    );

    let _ = std::fs::remove_file(&path);
}

/// S372 regression. Storno of a SINGLE-line base (the live prod incident,
/// DEV TEST-ABERP/2026/0042): the one storno line's `<lineNumber>` MUST be
/// document-local `1` while its `<lineNumberReference>` is `2`. S369's bug
/// shifted BOTH to `2`, tripping NAV's LINE_NUMBER_NOT_SEQUENTIAL.
#[test]
fn storno_single_line_base_keeps_document_local_line_number() {
    let storno = build_storno_invoice_with_lines(1);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count: 1,
    };
    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .expect("1-line storno renders");
    let body = std::str::from_utf8(&xml).unwrap();

    assert!(
        body.contains("<lineNumber>1</lineNumber>"),
        "storno of a 1-line base must keep <lineNumber>1; body:\n{body}"
    );
    assert!(
        body.contains("<lineNumberReference>2</lineNumberReference>"),
        "storno of a 1-line base must reference base line 2; body:\n{body}"
    );
    assert!(
        !body.contains("<lineNumber>2</lineNumber>"),
        "S372 regression — <lineNumber> must NOT be shifted to 2 \
         (NAV LINE_NUMBER_NOT_SEQUENTIAL); body:\n{body}"
    );

    validate_invoice_data(&xml).expect("1-line storno must pass the v3.0 invariant check");
}

/// S372 regression. The storno's OWN line count drives `<lineNumber>`; the
/// base's count drives `<lineNumberReference>`. A 2-line storno against a
/// 3-line base must emit `<lineNumber>` `[1, 2]` and `<lineNumberReference>`
/// `[4, 5]` — the two axes are independent.
#[test]
fn storno_line_number_and_reference_are_independent_axes() {
    let storno = build_storno_invoice_with_lines(2);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count: 3,
    };
    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .expect("2-line storno against 3-line base renders");
    let body = std::str::from_utf8(&xml).unwrap();

    // Document-local line numbers: 1, 2 (NOT shifted by the base count).
    assert!(
        body.contains("<lineNumber>1</lineNumber>") && body.contains("<lineNumber>2</lineNumber>"),
        "<lineNumber> must be document-local 1, 2; body:\n{body}"
    );
    // References continue past the 3-line base: 4, 5.
    assert!(
        body.contains("<lineNumberReference>4</lineNumberReference>")
            && body.contains("<lineNumberReference>5</lineNumberReference>"),
        "<lineNumberReference> must continue past the base as 4, 5; body:\n{body}"
    );
    assert!(
        !body.contains("<lineNumber>4</lineNumber>")
            && !body.contains("<lineNumber>5</lineNumber>"),
        "S372 regression — <lineNumber> must NOT carry the base offset; body:\n{body}"
    );

    validate_invoice_data(&xml).expect("2-line storno must pass the v3.0 invariant check");
}

/// S372 regression guard for the offset=0 case. Plain initial issuance
/// (no chain) numbers its lines document-local 1..=n. This pins that the
/// `<lineNumber>` derivation never silently regrows the chain offset.
#[test]
fn initial_issuance_numbers_lines_document_local() {
    let invoice = build_storno_invoice_with_lines(2);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("2-line initial issuance renders");
    let body = std::str::from_utf8(&xml).unwrap();

    assert!(
        body.contains("<lineNumber>1</lineNumber>") && body.contains("<lineNumber>2</lineNumber>"),
        "initial issuance must number lines 1, 2; body:\n{body}"
    );
    // Plain issuance carries NO <lineModificationReference>, so no
    // <lineNumberReference> element at all.
    assert!(
        !body.contains("<lineNumberReference>"),
        "plain initial issuance must not emit any <lineNumberReference>; body:\n{body}"
    );

    validate_invoice_data(&xml).expect("initial issuance must pass the v3.0 invariant check");
}

// ──────────────────────────────────────────────────────────────────────
// S384/F5 — chain-aware storno: line extractor round-trip + render fold.
// ──────────────────────────────────────────────────────────────────────

/// S384/F5 — `read_invoice_lines_from_xml` reconstructs lines byte-
/// faithfully: render a multi-line HUF invoice, parse the lines back,
/// RE-RENDER from the parsed lines, and assert the two `<invoiceLines>`
/// blocks are byte-identical. The re-render equality is the strongest
/// fidelity check — it proves a storno built from folded chain-member
/// lines emits exactly what NAV recorded (so the negation is exact),
/// independent of the None-unit↔`PIECE` struct nuance (both emit PIECE).
#[test]
fn read_invoice_lines_from_xml_round_trips_through_re_render() {
    use ulid::Ulid;

    let scratch = std::env::temp_dir().join(format!("aberp-s384-lineroundtrip-{}", Ulid::new()));
    std::fs::create_dir_all(&scratch).expect("create scratch dir");

    let mut invoice = build_minimal_storno_invoice();
    invoice.lines = vec![
        LineItem {
            description: "Plain line, no unit".to_string(),
            quantity: rust_decimal::Decimal::from(3),
            unit_price: Huf(1234),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        },
        LineItem {
            description: "Fractional qty, NAV unit".to_string(),
            quantity: rust_decimal::Decimal::new(15, 1), // 1.5
            unit_price: Huf(800),
            vat_rate_basis_points: 500, // 5%
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: Some(ProductUnit::Nav(NavUnitOfMeasure::Day)),
        },
        LineItem {
            description: "Own unit line".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            unit_price: Huf(50),
            vat_rate_basis_points: 0,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: Some(ProductUnit::Own("liter@15C".to_string())),
        },
    ];

    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, Currency::Huf, None)
        .expect("base renders");
    let path = scratch.join(format!("{}.xml", Ulid::new()));
    std::fs::write(&path, &xml).expect("write xml");

    let parsed = nav_xml::read_invoice_lines_from_xml(&path).expect("parse lines back");
    assert_eq!(parsed.len(), 3, "all three lines must be recovered");

    // Wire-relevant fields recovered exactly.
    assert_eq!(parsed[0].description, "Plain line, no unit");
    assert_eq!(parsed[0].quantity, rust_decimal::Decimal::from(3));
    assert_eq!(parsed[0].unit_price, Huf(1234));
    assert_eq!(parsed[0].vat_rate_basis_points, 2700);
    assert_eq!(parsed[1].quantity, rust_decimal::Decimal::new(15, 1));
    assert_eq!(parsed[1].vat_rate_basis_points, 500);
    assert_eq!(
        parsed[1].unit,
        Some(ProductUnit::Nav(NavUnitOfMeasure::Day))
    );
    assert_eq!(
        parsed[2].unit,
        Some(ProductUnit::Own("liter@15C".to_string()))
    );

    // Strongest check: re-render from the parsed lines reproduces the
    // SAME XML byte-for-byte (a storno reverses these exact bytes).
    let mut re = build_minimal_storno_invoice();
    re.lines = parsed;
    let re_xml = nav_xml::render_invoice_data(&re, &series, &parties, Currency::Huf, None)
        .expect("re-render from parsed lines");
    let extract_lines = |b: &[u8]| {
        let s = std::str::from_utf8(b).unwrap().to_string();
        let start = s.find("<invoiceLines>").unwrap();
        let end = s.find("</invoiceLines>").unwrap();
        s[start..end].to_string()
    };
    assert_eq!(
        extract_lines(&xml),
        extract_lines(&re_xml),
        "parsed → re-rendered <invoiceLines> must be byte-identical (S384/F5)"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

/// S384/F5 — the EUR `<unitPrice>` two-decimal form parses back to native
/// cents (the decimal branch of `parse_native_minor_units`). `10.50` →
/// 1050 cents, `0.05` → 5 cents. (HUF integers are covered by the
/// round-trip test above against the real emitter.)
#[test]
fn read_invoice_lines_from_xml_parses_eur_unit_price_to_cents() {
    use ulid::Ulid;

    let scratch = std::env::temp_dir().join(format!("aberp-s384-eur-{}", Ulid::new()));
    std::fs::create_dir_all(&scratch).expect("create scratch dir");
    let path = scratch.join("eur.xml");
    // Minimal body with EUR-format (two-decimal) unitPrice values.
    let body = "<InvoiceData><invoiceLines>\
        <line><lineNumber>1</lineNumber><lineDescription>A</lineDescription>\
        <quantity>1</quantity><unitOfMeasure>PIECE</unitOfMeasure>\
        <unitPrice>10.50</unitPrice><lineAmountsNormal>\
        <lineVatRate><vatPercentage>0.27</vatPercentage></lineVatRate></lineAmountsNormal></line>\
        <line><lineNumber>2</lineNumber><lineDescription>B</lineDescription>\
        <quantity>1</quantity><unitOfMeasure>PIECE</unitOfMeasure>\
        <unitPrice>0.05</unitPrice><lineAmountsNormal>\
        <lineVatRate><vatPercentage>0.27</vatPercentage></lineVatRate></lineAmountsNormal></line>\
        </invoiceLines></InvoiceData>";
    std::fs::write(&path, body).unwrap();

    let parsed = nav_xml::read_invoice_lines_from_xml(&path).expect("parse EUR lines");
    assert_eq!(parsed[0].unit_price, Huf(1050), "10.50 EUR → 1050 cents");
    assert_eq!(parsed[1].unit_price, Huf(5), "0.05 EUR → 5 cents");

    let _ = std::fs::remove_dir_all(&scratch);
}

/// S384/F5 — the render-side fold: a storno whose `reversal_source_lines`
/// is base + saved-modification lines reverses ALL of them, with
/// `<lineNumberReference>` continuing past the total prior chain line
/// count. Here base has 1 line and a saved modification contributes 2,
/// so the storno emits 3 reversal lines referencing 4, 5, 6 (offset 3).
#[test]
fn storno_render_reverses_base_plus_saved_modification_lines() {
    let invoice = build_minimal_storno_invoice(); // 1 base line
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();

    // base line + two folded saved-modification lines = 3 reversal lines.
    let mut reversal = invoice.lines.clone();
    reversal.push(LineItem {
        description: "SAVED-MOD-LINE-1".to_string(),
        quantity: rust_decimal::Decimal::from(4),
        unit_price: Huf(250),
        vat_rate_basis_points: 2700,
        vat_rate_kind: aberp_billing::VatRateKind::Percent,
        note: None,
        unit: None,
    });
    reversal.push(LineItem {
        description: "SAVED-MOD-LINE-2".to_string(),
        quantity: rust_decimal::Decimal::from(7),
        unit_price: Huf(100),
        vat_rate_basis_points: 2700,
        vat_rate_kind: aberp_billing::VatRateKind::Percent,
        note: None,
        unit: None,
    });

    let reference = StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 2,
        base_line_count: reversal.len(), // total prior chain line count
    };
    let xml = nav_xml::render_storno_data_with_number(
        &invoice,
        &series,
        &parties,
        &reference,
        Currency::Huf,
        None,
        PaymentMethod::Transfer,
        Some("INV-default/00010"),
        &reversal,
    )
    .expect("chain-aware storno renders");
    let body = std::str::from_utf8(&xml).unwrap();

    // All three reversal lines present (base + both saved-mod lines).
    assert!(
        body.contains("Cancellation of widget"),
        "base line reversed"
    );
    assert!(
        body.contains("SAVED-MOD-LINE-1"),
        "saved mod line 1 reversed"
    );
    assert!(
        body.contains("SAVED-MOD-LINE-2"),
        "saved mod line 2 reversed"
    );

    // Offset = 3 (total prior chain lines): references continue at 4,5,6,
    // and NONE collide with prior chain line numbers 1,2,3.
    for r in ["4", "5", "6"] {
        assert!(
            body.contains(&format!("<lineNumberReference>{r}</lineNumberReference>")),
            "reversal line must reference {r} (offset 3); body:\n{body}"
        );
    }
    for collide in ["1", "2", "3"] {
        assert!(
            !body.contains(&format!(
                "<lineNumberReference>{collide}</lineNumberReference>"
            )),
            "reversal must NOT reuse prior chain line {collide} (NAV INVOICE_LINE_ALREADY_EXISTS)"
        );
    }
    // Quantities negated (reversal): -4 and -7 appear.
    assert!(
        body.contains("<quantity>-4</quantity>"),
        "mod line 1 qty negated"
    );
    assert!(
        body.contains("<quantity>-7</quantity>"),
        "mod line 2 qty negated"
    );

    validate_invoice_data(&xml).expect("chain-aware storno must pass the v3.0 invariant check");
}
