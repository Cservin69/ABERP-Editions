//! S394 — end-to-end: operator sets the "next invoice number" override,
//! then issuance honours it.
//!
//! Threads the override through the SAME surfaces the live system uses:
//!
//! 1. `aberp::numbering::write_numbering_section` — the exact function the
//!    SPA's `PUT /api/seller/numbering` handler calls to persist
//!    `[seller.numbering].start_value` to seller.toml.
//! 2. `aberp::numbering::read_numbering_template` — the exact call the
//!    binary's issue path (`apps/aberp/src/issue_invoice.rs`) makes to load
//!    the template before building `AllocateArgs`.
//! 3. `billing::allocate_in_tx` driven with `start_value =
//!    template.start_value` — exactly how the issue path wires it.
//!
//! The bug: with rows already issued (stored `next_number = 41`), the
//! allocator dropped the operator's `start_value` of 56 and reserved 41.
//! This pins that the override now wins end-to-end, and that the
//! operator-visible rendered number is `…/00056`.

use aberp::numbering::{self, NumberingTemplate, ResetPolicy as NumberingResetPolicy, Segment};
use aberp_billing::{
    self as billing, AllocateArgs, AllocateOutcome, BillingStore, CustomerId, DraftInvoice,
    DuckDbBillingStore, Huf, IdempotencyKey, InvoiceId, InvoiceSeries, LineItem, ResetPolicy,
    SeriesCode, SeriesId,
};
use time::macros::datetime;
use time::OffsetDateTime;

fn unique_tmpdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aberp-s394-e2e-{}-{}-{:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tmpdir");
    dir
}

/// `INV-<year>/<counter:5>` with the OnYearChange policy — a realistic
/// Hungarian template carrying a Year segment (required by the validator
/// for OnYearChange).
fn template(start_value: u64) -> NumberingTemplate {
    NumberingTemplate {
        segments: vec![
            Segment::Literal("INV-".to_string()),
            Segment::Year {
                digits: numbering::YearDigits::Four,
            },
            Segment::Literal("/".to_string()),
            Segment::Counter { pad_width: 5 },
        ],
        reset_policy: NumberingResetPolicy::OnYearChange,
        start_value,
    }
}

fn series_id_for(store: &mut DuckDbBillingStore) -> SeriesId {
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: SeriesCode::new("INV").expect("series code"),
        // Mirror the template's OnYearChange policy → AnnualOnFiscalYear.
        reset_policy: ResetPolicy::AnnualOnFiscalYear,
        fiscal_year: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
    };
    store.create_series(&series).expect("create series");
    series.id
}

fn args(series_id: SeriesId, start_value: u64) -> AllocateArgs {
    AllocateArgs {
        series_id,
        draft: DraftInvoice {
            id: InvoiceId::new(),
            series_id,
            customer_id: CustomerId::new(),
            lines: vec![LineItem {
                description: "Widget".to_string(),
                quantity: rust_decimal::Decimal::from(1),
                unit_price: Huf(1_000),
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            }],
            issue_date: datetime!(2026-06-15 12:00:00 UTC),
            payment_deadline: datetime!(2026-06-15 12:00:00 UTC).date(),
            delivery_date: datetime!(2026-06-15 12:00:00 UTC).date(),
        },
        idempotency_key: IdempotencyKey::new(),
        currency: billing::Currency::Huf,
        rate_metadata: None,
        bank_snapshot: None,
        invoice_note: None,
        email_recipient_override: None,
        start_value,
        // Production-shaped: no NAV pre-flight floor (the seam this test
        // exercises is the operator override, not the S392 NAV skip).
        sequence_floor: None,
    }
}

fn fresh_seq(outcome: AllocateOutcome) -> u64 {
    match outcome {
        AllocateOutcome::Fresh { invoice, .. } => invoice.sequence_number,
        AllocateOutcome::Replay { .. } => panic!("unexpected Replay"),
    }
}

/// Operator sets override=56 via the seller.toml surface; with rows already
/// up to 40, the next issued invoice is numbered 56 (not 41), and renders
/// as `INV-2026/00056`.
#[test]
fn operator_override_honoured_end_to_end() {
    // ── 1. Operator saves "next number = 56" (the SPA persistence surface).
    let dir = unique_tmpdir("override");
    let seller_toml = dir.join("seller.toml");
    numbering::write_numbering_section(&seller_toml, &template(56)).expect("persist template");

    // ── 2. Binary loads the template at issue time.
    let loaded = numbering::read_numbering_template(&seller_toml).expect("read template");
    assert_eq!(loaded.start_value, 56, "override persisted + read back");

    // ── 3. A bucket that already has invoices up to 40 (stored next=41).
    let mut store = DuckDbBillingStore::open_in_memory().expect("open store");
    store.ensure_schema().expect("ensure_schema");
    let series_id = series_id_for(&mut store);
    let now = datetime!(2026-06-15 12:00:00 UTC);
    for expected in 1..=40 {
        let o = store
            .allocate_and_insert(args(series_id, 1), now)
            .expect("seed allocation");
        assert_eq!(fresh_seq(o), expected);
    }

    // ── 4. Issue with the operator's override threaded through, exactly as
    //       issue_invoice.rs wires `start_value: template.start_value`.
    let issued = store
        .allocate_and_insert(args(series_id, loaded.start_value), now)
        .expect("issue with override");
    let number = fresh_seq(issued);
    assert_eq!(
        number, 56,
        "operator set 56 → system uses 56 (was 41 pre-S394)"
    );

    // ── 5. The operator-visible rendered number reflects 56.
    let rendered = loaded.render(2026, number);
    assert_eq!(
        rendered, "INV-2026/00056",
        "rendered invoice number honours override"
    );
}

/// Control: leaving the override at the default (1) keeps the gap-free
/// increment-by-1 stream — issuance is byte-identical to pre-S394.
#[test]
fn default_override_keeps_gap_free_sequence() {
    let dir = unique_tmpdir("default");
    let seller_toml = dir.join("seller.toml");
    numbering::write_numbering_section(&seller_toml, &template(1)).expect("persist default");
    let loaded = numbering::read_numbering_template(&seller_toml).expect("read template");

    let mut store = DuckDbBillingStore::open_in_memory().expect("open store");
    store.ensure_schema().expect("ensure_schema");
    let series_id = series_id_for(&mut store);
    let now = datetime!(2026-06-15 12:00:00 UTC);
    for expected in 1..=5 {
        let o = store
            .allocate_and_insert(args(series_id, loaded.start_value), now)
            .expect("allocate default");
        assert_eq!(fresh_seq(o), expected, "default override stays gap-free");
    }
}
