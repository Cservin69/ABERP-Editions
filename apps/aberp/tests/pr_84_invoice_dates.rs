//! PR-84 — invoice-date pins.
//!
//! Three load-bearing invariants this PR introduces:
//!
//!   1. The NAV emit MUST surface the operator-chosen
//!      `<invoiceDeliveryDate>` and `<paymentDate>` — distinct from
//!      `<invoiceIssueDate>`. Pre-PR-84 the emitter silently mirrored
//!      issue_date for both, which would mis-file the VAT period for
//!      any back- or forward-dated invoice.
//!
//!   2. The DuckDB round-trip MUST preserve the two operator-chosen
//!      dates across the allocator's INSERT and the loader's SELECT
//!      (`load_ready_invoice_by_id`).
//!
//!   3. Pre-PR-84 rows (NULL `payment_deadline` + NULL `delivery_date`
//!      in the DuckDB row, since the migration is additive) MUST
//!      continue to load with both dates falling back to `issue_date`
//!      — the read path's "NULL → issue_date" fallback preserves
//!      byte-on-disk behaviour for invoices issued before the
//!      migration ran.
//!
//! See `modules/billing/src/domain/invoice_dates.rs` for the
//! comfort-zone classifier pinned at the domain layer; this file
//! pins the wire + storage shape.

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, ReadyInvoice, SeriesCode, SeriesId,
};
use time::macros::date;
use time::OffsetDateTime;

fn parties() -> NavParties {
    NavParties {
        supplier: SupplierInfo {
            tax_number: "12345678-1-42".to_string(),
            name: "Test Supplier Kft.".to_string(),
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
            tax_number: Some("87654321-2-13".to_string()),
            name: "Test Buyer Kft.".to_string(),
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Váci utca 19.".to_string(),
            }),
        },
    }
}

/// THE headline pin. PR-84: the NAV emitter's `<invoiceDeliveryDate>`
/// and `<paymentDate>` MUST surface the operator-chosen values, NOT
/// mirror `<invoiceIssueDate>`. A regression that wires the wrong
/// field — or reverts the `write_invoice_detail` signature — would
/// silently mis-file the VAT period for every back- or forward-dated
/// invoice; this pin trips at CI before the regression reaches NAV.
#[test]
fn nav_emit_surfaces_three_distinct_dates() {
    // Operator-supplied dates that differ from each other AND from the
    // server-stamped issue date.
    let issue_dt = time::macros::datetime!(2026-05-27 10:30:00 UTC);
    let invoice = ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 42,
        fiscal_year: 0,
        lines: vec![LineItem {
            description: "Pin line".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(1_000),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        }],
        issue_date: issue_dt,
        // Three distinct calendar dates so the bytes-on-wire
        // assertion below catches a mis-wire to ANY of the other two
        // fields.
        payment_deadline: date!(2026 - 06 - 04), // issue + 8 days
        delivery_date: date!(2026 - 05 - 20),    // BEFORE issue (legitimate back-dating)
    };
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties(), Currency::Huf, None)
        .expect("render");
    let s = String::from_utf8(xml).expect("UTF-8 NAV body");

    // `<invoiceIssueDate>` carries the server-stamped issue date.
    assert!(
        s.contains("<invoiceIssueDate>2026-05-27</invoiceIssueDate>"),
        "wire body must carry the server-stamped issue date; got:\n{s}"
    );
    // `<invoiceDeliveryDate>` carries the operator-chosen delivery
    // date — NOT issue_date. The pre-PR-84 emitter mirrored
    // issue_date here; the regression this pin catches is exactly
    // that mirroring re-appearing.
    assert!(
        s.contains("<invoiceDeliveryDate>2026-05-20</invoiceDeliveryDate>"),
        "wire body must carry the operator-chosen delivery date; got:\n{s}"
    );
    // `<paymentDate>` carries the operator-chosen payment deadline —
    // NOT issue_date. Same regression class as above.
    assert!(
        s.contains("<paymentDate>2026-06-04</paymentDate>"),
        "wire body must carry the operator-chosen payment deadline; got:\n{s}"
    );
}

/// PR-84 — DuckDB round-trip pin. The two operator-supplied dates
/// must survive the allocator's INSERT and the loader's SELECT
/// without drift. A regression that drops either column from the
/// INSERT (or omits the NULL→issue_date fallback in the read path
/// for a fresh DB) surfaces here.
#[test]
fn duckdb_round_trip_preserves_payment_deadline_and_delivery_date() {
    use aberp_billing::{
        AllocateArgs, AllocateOutcome, BillingStore, DraftInvoice, DuckDbBillingStore,
        IdempotencyKey, InvoiceSeries, ResetPolicy,
    };

    let mut store = DuckDbBillingStore::open_in_memory().expect("open in-memory DB");
    store.ensure_schema().expect("ensure schema");

    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: SeriesCode::new("INV-PR84".to_string()).unwrap(),
        reset_policy: ResetPolicy::Never,
        fiscal_year: None,
        created_at: OffsetDateTime::now_utc(),
    };
    store.create_series(&series).expect("create series");

    let now = OffsetDateTime::now_utc();
    // Derive the two operator-chosen dates from `now` with non-zero
    // offsets so they can NEVER coincide with `now.date()`. The
    // assert_ne! pins below check the read path doesn't silently
    // substitute issue_date — a HARDCODED fixture date is a time bomb:
    // PR-84's original `date!(2026-06-15)` collided with the real
    // calendar day on 2026-06-15, tripping `assert_ne!(deadline, today)`.
    // Deriving from `now` makes this calendar-independent forever.
    let payment_deadline = now.date() + time::Duration::days(8);
    let delivery_date = now.date() - time::Duration::days(36);
    let invoice_id = InvoiceId::new();
    let draft = DraftInvoice {
        id: invoice_id,
        series_id: series.id,
        customer_id: CustomerId::new(),
        lines: vec![LineItem {
            description: "round-trip line".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(1_000),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        }],
        issue_date: now,
        payment_deadline,
        delivery_date,
    };

    let outcome = store
        .allocate_and_insert(
            AllocateArgs {
                series_id: series.id,
                draft,
                idempotency_key: IdempotencyKey::new(),
                currency: Currency::Huf,
                rate_metadata: None,
                bank_snapshot: None,
                invoice_note: None,
                email_recipient_override: None,
                start_value: 1,
                sequence_floor: None,
            },
            now,
        )
        .expect("allocate");

    let invoice = match outcome {
        AllocateOutcome::Fresh { invoice, .. } => invoice,
        AllocateOutcome::Replay { .. } => panic!("expected fresh allocation"),
    };

    // The freshly-allocated invoice carries the same dates the draft
    // had — and they are NOT the issue date (so the assertion catches
    // a regression that silently substituted issue_date in the read
    // path's NULL fallback, which would surface as the wrong dates
    // here).
    assert_eq!(invoice.payment_deadline, payment_deadline);
    assert_eq!(invoice.delivery_date, delivery_date);
    assert_ne!(invoice.payment_deadline, now.date());
    assert_ne!(invoice.delivery_date, now.date());
}
