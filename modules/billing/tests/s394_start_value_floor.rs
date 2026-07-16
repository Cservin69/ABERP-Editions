//! S394 — the operator's `start_value` is honoured as an allocation FLOOR,
//! not just a first-INSERT seed.
//!
//! Bug: an operator set the "next invoice number" override (which the SPA
//! persists to `[seller.numbering].start_value`) to 56, but the system kept
//! issuing the stored counter (40) instead. Root cause: `allocate_in_tx`
//! only applied `start_value` on the FIRST INSERT into a
//! `(series, fiscal_year)` bucket; once a counter row existed the override
//! was silently dropped. The fix makes the allocator reserve
//! `max(next_number, start_value, sequence_floor)` on EVERY allocation, so
//! raising `start_value` above the live counter takes effect immediately
//! (operator mental model: "I set 56 → next is 56"), while the default (1)
//! stays a no-op (gap-free §169 behaviour, byte-identical to pre-S394).
//!
//! Runs against BOTH adapters (ADR-0006 §Conformance — divergence between
//! the in-memory and DuckDB adapters is itself a bug). `peek_next_number`
//! (DuckDB-only) must apply the same floor so the S392 NAV pre-flight probes
//! the number the allocator will actually reserve.

use time::macros::datetime;
use time::OffsetDateTime;

use aberp_billing::{
    peek_next_number, AllocateArgs, AllocateOutcome, BillingStore, CustomerId, DraftInvoice,
    DuckDbBillingStore, Huf, IdempotencyKey, InMemoryBillingStore, InvoiceId, InvoiceSeries,
    LineItem, ResetPolicy, SeriesCode, SeriesId,
};

fn series_code() -> SeriesCode {
    SeriesCode::new("ABERP").expect("series code is valid")
}

fn create_continuous_series<S: BillingStore + ?Sized>(store: &mut S) -> SeriesId {
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: series_code(),
        reset_policy: ResetPolicy::Never,
        fiscal_year: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
    };
    store.create_series(&series).expect("create series");
    series.id
}

fn one_line() -> LineItem {
    LineItem {
        description: "Test widget".to_string(),
        quantity: rust_decimal::Decimal::from(1),
        unit_price: Huf(1_000),
        vat_rate_basis_points: 2700,
        vat_rate_kind: aberp_billing::VatRateKind::Percent,
        note: None,
        unit: None,
    }
}

fn args(series_id: SeriesId, start_value: u64, sequence_floor: Option<u64>) -> AllocateArgs {
    AllocateArgs {
        series_id,
        draft: DraftInvoice {
            id: InvoiceId::new(),
            series_id,
            customer_id: CustomerId::new(),
            lines: vec![one_line()],
            issue_date: datetime!(2026-06-15 12:00:00 UTC),
            payment_deadline: datetime!(2026-06-15 12:00:00 UTC).date(),
            delivery_date: datetime!(2026-06-15 12:00:00 UTC).date(),
        },
        idempotency_key: IdempotencyKey::new(),
        currency: aberp_billing::Currency::Huf,
        rate_metadata: None,
        bank_snapshot: None,
        invoice_note: None,
        email_recipient_override: None,
        start_value,
        sequence_floor,
    }
}

fn fresh_seq(outcome: AllocateOutcome) -> u64 {
    match outcome {
        AllocateOutcome::Fresh { invoice, .. } => invoice.sequence_number,
        AllocateOutcome::Replay { .. } => panic!("fresh issuance unexpectedly Replay"),
    }
}

/// Burn `n` invoices at `start_value=1` so the bucket's stored `next_number`
/// becomes `n + 1` — i.e. "rows up to `n` exist". Returns the series id.
fn seed_rows_up_to<S: BillingStore + ?Sized>(store: &mut S, n: u64) -> SeriesId {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_continuous_series(store);
    let now = datetime!(2026-06-15 12:00:00 UTC);
    for expected in 1..=n {
        let outcome = store
            .allocate_and_insert(args(series_id, 1, None), now)
            .expect("seed allocation");
        assert_eq!(fresh_seq(outcome), expected, "seeding burns 1,2,3,…");
    }
    series_id
}

// ── Item D — allocator floor semantics, both adapters ─────────────────

/// Empty store + override=56 → 56 (the seed path already did this; pinned
/// so a regression in the unified floor can't silently break it).
fn run_empty_store_override<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_continuous_series(store);
    let now = datetime!(2026-06-15 12:00:00 UTC);
    let outcome = store
        .allocate_and_insert(args(series_id, 56, None), now)
        .expect("allocate on empty bucket");
    assert_eq!(fresh_seq(outcome), 56, "empty bucket + override=56 → 56");
}

/// Existing rows up to 40 (stored next_number=41) + override=56 → 56. This
/// is the bug case: pre-S394 the allocator ignored start_value here and
/// returned 41. The skipped range 41..=55 is burned as deliberate gaps; the
/// next allocation continues from 57.
fn run_existing_below_override<S: BillingStore + ?Sized>(store: &mut S) {
    let series_id = seed_rows_up_to(store, 40);
    let now = datetime!(2026-06-15 12:00:00 UTC);

    let jumped = store
        .allocate_and_insert(args(series_id, 56, None), now)
        .expect("allocate with override above counter");
    assert_eq!(
        fresh_seq(jumped),
        56,
        "override=56 floors the reserved number to 56 even though next_number was 41"
    );

    // Counter advanced to 57 — gap-free from the override, 41..=55 vacated.
    let after = store
        .allocate_and_insert(args(series_id, 56, None), now)
        .expect("allocate after override jump");
    assert_eq!(fresh_seq(after), 57, "counter continues from floor+1");
}

/// Existing rows up to 60 (stored next_number=61) + override=56 → 61. The
/// override is a FLOOR, not an absolute: when the live counter already
/// exceeds it, the counter wins (never rewind — §169 forbids duplicates).
fn run_existing_above_override<S: BillingStore + ?Sized>(store: &mut S) {
    let series_id = seed_rows_up_to(store, 60);
    let now = datetime!(2026-06-15 12:00:00 UTC);

    let outcome = store
        .allocate_and_insert(args(series_id, 56, None), now)
        .expect("allocate with override below counter");
    assert_eq!(
        fresh_seq(outcome),
        61,
        "override=56 is a no-op when next_number (61) already exceeds it"
    );
}

/// The default override (start_value=1) is a no-op on every allocation:
/// 1,2,3 with no jumps — the pre-S394 gap-free §169 stream is unchanged.
fn run_default_start_value_is_gap_free<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_continuous_series(store);
    let now = datetime!(2026-06-15 12:00:00 UTC);
    for expected in 1..=3 {
        let outcome = store
            .allocate_and_insert(args(series_id, 1, None), now)
            .expect("allocate default");
        assert_eq!(
            fresh_seq(outcome),
            expected,
            "default start_value stays gap-free"
        );
    }
}

/// Override floor + a higher NAV `sequence_floor` compose: the larger wins.
fn run_override_and_nav_floor_compose<S: BillingStore + ?Sized>(store: &mut S) {
    let series_id = seed_rows_up_to(store, 40); // stored next_number = 41
    let now = datetime!(2026-06-15 12:00:00 UTC);

    // override=56 but NAV pre-flight says 70 is the first clear number.
    let outcome = store
        .allocate_and_insert(args(series_id, 56, Some(70)), now)
        .expect("allocate with override + nav floor");
    assert_eq!(
        fresh_seq(outcome),
        70,
        "max(next_number=41, start_value=56, nav_floor=70) = 70"
    );
}

mod in_memory {
    use super::*;

    #[test]
    fn empty_store_override() {
        run_empty_store_override(&mut InMemoryBillingStore::new());
    }
    #[test]
    fn existing_below_override() {
        run_existing_below_override(&mut InMemoryBillingStore::new());
    }
    #[test]
    fn existing_above_override() {
        run_existing_above_override(&mut InMemoryBillingStore::new());
    }
    #[test]
    fn default_start_value_is_gap_free() {
        run_default_start_value_is_gap_free(&mut InMemoryBillingStore::new());
    }
    #[test]
    fn override_and_nav_floor_compose() {
        run_override_and_nav_floor_compose(&mut InMemoryBillingStore::new());
    }
}

mod duckdb_backed {
    use super::*;

    fn store() -> DuckDbBillingStore {
        DuckDbBillingStore::open_in_memory().expect("open in-memory DuckDB store")
    }

    #[test]
    fn empty_store_override() {
        run_empty_store_override(&mut store());
    }
    #[test]
    fn existing_below_override() {
        run_existing_below_override(&mut store());
    }
    #[test]
    fn existing_above_override() {
        run_existing_above_override(&mut store());
    }
    #[test]
    fn default_start_value_is_gap_free() {
        run_default_start_value_is_gap_free(&mut store());
    }
    #[test]
    fn override_and_nav_floor_compose() {
        run_override_and_nav_floor_compose(&mut store());
    }

    /// `peek_next_number` applies the same floor so the S392 NAV pre-flight
    /// probes the number the allocator will actually reserve. Stored
    /// next_number=41, override=56 → peek returns 56 (not 41).
    #[test]
    fn peek_floors_by_start_value() {
        let mut store = store();
        let series_id = seed_rows_up_to(&mut store, 40); // stored next_number = 41
        let conn = store.into_connection();

        assert_eq!(
            peek_next_number(&conn, series_id, 2026, 56).expect("peek with override"),
            56,
            "peek floors stored 41 up to override 56 (NAV-probe consistency)"
        );
        // A start_value at/below the stored counter is a no-op.
        assert_eq!(
            peek_next_number(&conn, series_id, 2026, 1).expect("peek default"),
            41,
            "peek is a no-op when start_value <= stored next_number"
        );
    }
}
