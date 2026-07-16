//! PR-44γ.1 pin tests — chain-currency-match (ADR-0037 §4 invariant C6).
//!
//! Two surfaces under test:
//!
//! - **NAV body shape for EUR chain children.** An EUR storno (and an
//!   EUR modification) MUST emit `<currencyCode>EUR</currencyCode>` +
//!   the inherited base rate at exactly six decimal places. Pinned by
//!   `eur_storno_body_carries_inherited_currency_and_rate` /
//!   `eur_modification_body_carries_inherited_currency_and_rate`.
//!
//! - **Sign-of-amounts invariant for the storno.** An EUR storno's
//!   `<exchangeRate>` MUST match the base's verbatim (positive, six
//!   decimals); the per-line and per-summary `*HUF` amounts MUST be
//!   negative (the negation cascades through the same line writer the
//!   HUF storno uses). Pinned by
//!   `eur_storno_body_carries_negative_huf_amounts`.
//!
//! - **Loud-fail symmetry.** Rendering a chain child with `Currency::Eur`
//!   but `rate_metadata=None` MUST loud-fail (same C1-wire-side guard
//!   `render_invoice_data` enforces; `render_storno_data` and
//!   `render_modification_data` share the `ensure_rate_metadata_invariant`
//!   helper). Pinned by `eur_storno_without_rate_metadata_loud_fails`
//!   and `eur_modification_without_rate_metadata_loud_fails`.
//!
//! The chain-inheritance helper (`inherit_rate_metadata_for_chain`)
//! and the defensive C6 guard (`require_chain_currency_match`) are
//! unit-tested in `apps/aberp/src/invoice_currency_metadata.rs`'s
//! `tests` module. This file pins the wire-body integration shape that
//! consumes those helpers' outputs.

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, ModificationReference, NavParties,
    StornoReference, SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, RateMetadata, ReadyInvoice, SeriesCode,
    SeriesId,
};
use aberp_nav_xsd_validator::validate_invoice_data;
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

/// EUR chain-child invoice fixture. `unit_price` is wrapped in
/// `Huf(cents)` per the same interim posture as
/// `apps/aberp/tests/nav_xml_eur_body.rs`'s EUR fixture
/// (`finalize_rate`'s doc comment names the lift trigger).
fn build_eur_chain_child() -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 7,
        fiscal_year: 0,
        lines: vec![LineItem {
            description: "EUR storno line".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            unit_price: Huf(1000), // 10.00 EUR per unit
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        }],
        issue_date: OffsetDateTime::now_utc(),
        // PR-84 — chain-child fixture defaults both date fields to issue.
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
    }
}

/// Canonical inherited rate metadata: rate 405.230000, source MNB,
/// date 2026-05-08. The `huf_equivalent_total` field is whatever the
/// chain helper computed against the chain child's gross — its value
/// does NOT appear on the NAV wire body (the wire computes per-amount
/// HUF equivalents independently from the `rate` field per ADR-0037
/// §1.c + C5). We pin it to a sentinel here to surface any future
/// regression that accidentally puts the stamp on the wire.
fn build_inherited_rate_metadata() -> RateMetadata {
    RateMetadata {
        rate: Decimal::from_str("405.230000").unwrap(),
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        huf_equivalent_total: i64::MIN / 4, // sentinel
    }
}

fn build_storno_reference() -> StornoReference {
    StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count: 1, // S369 — single-line base fixture
    }
}

fn build_modification_reference() -> ModificationReference {
    ModificationReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count: 1, // S369 — single-line base fixture
    }
}

#[test]
fn eur_storno_body_carries_inherited_currency_and_rate() {
    let storno = build_eur_chain_child();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = build_storno_reference();
    let rate_metadata = build_inherited_rate_metadata();

    let xml = nav_xml::render_storno_data(
        &storno,
        &series,
        &parties,
        &reference,
        Currency::Eur,
        Some(&rate_metadata),
    )
    .expect("EUR storno render must succeed with inherited rate metadata");

    validate_invoice_data(&xml).expect("EUR storno body must validate against NAV v3.0");

    let body = std::str::from_utf8(&xml).expect("XML is UTF-8");
    assert!(
        body.contains("<currencyCode>EUR</currencyCode>"),
        "EUR storno must carry currencyCode=EUR (C4): {body}"
    );
    assert!(
        body.contains("<exchangeRate>405.230000</exchangeRate>"),
        "EUR storno must carry the inherited rate at 6 decimals (C11 + C6): {body}"
    );
    // Sentinel byte-pin: the per-invoice `huf_equivalent_total` stamp
    // must NOT appear in the wire body. The wire body computes its
    // own per-amount HUF equivalents; the stamp lives on the audit
    // payload only. Anything starting with `-23` (the sign and
    // first decimal digit of `i64::MIN / 4`) flags a leak.
    assert!(
        !body.contains("-2305843009213693952"),
        "the audit-payload huf_equivalent_total sentinel must NOT leak onto the wire: {body}"
    );
}

#[test]
fn eur_storno_body_carries_negative_huf_amounts() {
    let storno = build_eur_chain_child();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = build_storno_reference();
    let rate_metadata = build_inherited_rate_metadata();

    let xml = nav_xml::render_storno_data(
        &storno,
        &series,
        &parties,
        &reference,
        Currency::Eur,
        Some(&rate_metadata),
    )
    .unwrap();
    let body = std::str::from_utf8(&xml).unwrap();

    // The fixture: 2 × 1000 cents = 2000 cents net = €20.00 net.
    // EUR amounts are formatted via `format_native_amount` as
    // `EUROS.CC` (two-decimal cents). Negated: `-20.00` net. With
    // rate 405.230000: -20 EUR → -8104.60 HUF → round-half-even →
    // -8105. We pin both the EUR-native negation and the HUF-side
    // negation — the load-bearing C6 invariant the storno renderer's
    // negation must carry through the inherited rate's HUF conversion.
    assert!(
        body.contains("<lineNetAmount>-20.00</lineNetAmount>"),
        "EUR storno line net (EUR-native format) must be negated: {body}"
    );
    assert!(
        body.contains("<lineNetAmountHUF>-"),
        "EUR storno lineNetAmountHUF must be negative: {body}"
    );
}

#[test]
fn eur_modification_body_carries_inherited_currency_and_rate() {
    let modification = build_eur_chain_child();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = build_modification_reference();
    let rate_metadata = build_inherited_rate_metadata();

    let xml = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Eur,
        Some(&rate_metadata),
    )
    .expect("EUR modification render must succeed with inherited rate metadata");

    validate_invoice_data(&xml).expect("EUR modification body must validate against NAV v3.0");

    let body = std::str::from_utf8(&xml).unwrap();
    assert!(
        body.contains("<currencyCode>EUR</currencyCode>"),
        "EUR modification must carry currencyCode=EUR: {body}"
    );
    assert!(
        body.contains("<exchangeRate>405.230000</exchangeRate>"),
        "EUR modification must carry the inherited rate at 6 decimals (C11 + C6): {body}"
    );
    // Full-replace (NOT negated): line net = 2 × €10.00 = €20.00.
    // EUR-native format is `EUROS.CC` per `format_native_amount`.
    assert!(
        body.contains("<lineNetAmount>20.00</lineNetAmount>"),
        "EUR modification line net (full-replace) must be positive: {body}"
    );
    // S381/F1 — the MODIFY body must NOT carry the v2.0-only
    // <modificationIssueDate> (illegal in NAV v3.0); STORNO and MODIFY
    // bodies are byte-identical at the reference level and the wire
    // operation is derived from the audit ledger, not the body.
    assert!(
        !body.contains("modificationIssueDate"),
        "MODIFY shape must NOT carry <modificationIssueDate> (S381/F1): {body}"
    );
}

/// HUF chain children remain pre-PR-44γ.1 byte-near-identical: the C10
/// invariant prerequisite still holds at the chain-renderer level
/// (HUF storno's body shape is unchanged except for the 6-decimal
/// exchangeRate uniformly applied at PR-44δ / A142).
#[test]
fn huf_storno_back_compat_no_rate_metadata_required() {
    let storno = build_eur_chain_child(); // structure is same; currency tag differs
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = build_storno_reference();

    let xml =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Huf, None)
            .expect("HUF storno render must succeed with no rate metadata (C10 prerequisite)");

    validate_invoice_data(&xml).expect("HUF storno body must validate against NAV v3.0");

    let body = std::str::from_utf8(&xml).unwrap();
    assert!(
        body.contains("<currencyCode>HUF</currencyCode>"),
        "HUF storno must carry currencyCode=HUF: {body}"
    );
    assert!(
        body.contains("<exchangeRate>1.000000</exchangeRate>"),
        "HUF storno's exchangeRate must be 1.000000 (uniform 6-decimal per A142): {body}"
    );
}

/// Loud-fail symmetry on the storno path: EUR + None loud-fails the
/// same way `render_invoice_data` does. The error carries enough
/// context for an operator to identify the missing field.
#[test]
fn eur_storno_without_rate_metadata_loud_fails() {
    let storno = build_eur_chain_child();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = build_storno_reference();

    let err =
        nav_xml::render_storno_data(&storno, &series, &parties, &reference, Currency::Eur, None)
            .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("non-HUF") && msg.contains("rate_metadata"),
        "EUR storno without rate_metadata must loud-fail naming the missing field: {msg}"
    );
}

/// Same loud-fail symmetry for the modification path.
#[test]
fn eur_modification_without_rate_metadata_loud_fails() {
    let modification = build_eur_chain_child();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = minimal_parties();
    let reference = build_modification_reference();

    let err = nav_xml::render_modification_data(
        &modification,
        &series,
        &parties,
        &reference,
        Currency::Eur,
        None,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("non-HUF") && msg.contains("rate_metadata"),
        "EUR modification without rate_metadata must loud-fail naming the missing field: {msg}"
    );
}
