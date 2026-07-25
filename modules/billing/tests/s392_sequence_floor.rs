//! S392 — `AllocateArgs::sequence_floor` + `peek_next_number`.
//!
//! The binary's NAV pre-flight (`apps/aberp` `issue_from_parsed`) probes
//! NAV's `queryInvoiceCheck` for the next candidate numbers, finds the
//! first one NAV's shared TEST endpoint does NOT already hold, and threads
//! it back as `AllocateArgs::sequence_floor` so a fresh local sequence
//! does not collide after a local DB reset (root cause of
//! `INVOICE_NUMBER_NOT_UNIQUE`). These tests pin the two billing-side
//! primitives that make that work:
//!
//! 1. **`peek_next_number`** reads the number the allocator *would* assign
//!    without advancing it (empty bucket → `start_value.max(1)`; after an
//!    allocation → the advanced counter).
//! 2. **`sequence_floor`** forces the reserved number up to
//!    `max(next_number, floor)` — jumping past the skipped range (leaving
//!    deliberate gaps) — and is a no-op when `floor <= next_number` or
//!    `None`.
//!
//! The floor behaviour runs against BOTH adapters (ADR-0006 §Conformance —
//! divergence between the in-memory and DuckDB adapters is itself a bug).

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

// ── Floor behaviour — runs against both adapters ──────────────────────

/// A floor above the stored counter jumps the reserved number to the
/// floor and burns the skipped range; the next allocation continues from
/// floor+1 (gaps left behind on purpose).
fn run_floor_jumps_and_advances<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_continuous_series(store);
    let now = datetime!(2026-06-15 12:00:00 UTC);

    // Fresh bucket (start_value=1) but NAV already holds 1..=49 → floor=50.
    let first = store
        .allocate_and_insert(args(series_id, 1, Some(50)), now)
        .expect("allocate with floor");
    assert_eq!(
        fresh_seq(first),
        50,
        "floor forces the reserved number to 50"
    );

    // Next allocation (no floor) continues from 51 — gap-free from the
    // floor, with 1..=49 deliberately skipped.
    let second = store
        .allocate_and_insert(args(series_id, 1, None), now)
        .expect("allocate after floor jump");
    assert_eq!(fresh_seq(second), 51, "counter advanced to floor+1");
}

/// A floor at or below the stored counter is a no-op — the allocator
/// keeps the normal next number (NAV said the candidate was clear, so no
/// jump is needed).
fn run_floor_below_next_is_ignored<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_continuous_series(store);
    let now = datetime!(2026-06-15 12:00:00 UTC);

    // Burn 1 normally → stored next_number is now 2.
    let first = store
        .allocate_and_insert(args(series_id, 1, None), now)
        .expect("allocate 1");
    assert_eq!(fresh_seq(first), 1);

    // A stale floor of 1 (< stored 2) must not rewind or duplicate.
    let second = store
        .allocate_and_insert(args(series_id, 1, Some(1)), now)
        .expect("allocate with stale floor");
    assert_eq!(fresh_seq(second), 2, "floor <= next_number is a no-op");
}

mod in_memory {
    use super::*;

    #[test]
    fn floor_jumps_and_advances() {
        run_floor_jumps_and_advances(&mut InMemoryBillingStore::new());
    }

    #[test]
    fn floor_below_next_is_ignored() {
        run_floor_below_next_is_ignored(&mut InMemoryBillingStore::new());
    }
}

mod duckdb_backed {
    use super::*;

    fn store() -> DuckDbBillingStore {
        DuckDbBillingStore::open_in_memory().expect("open in-memory DuckDB store")
    }

    #[test]
    fn floor_jumps_and_advances() {
        run_floor_jumps_and_advances(&mut store());
    }

    #[test]
    fn floor_below_next_is_ignored() {
        run_floor_below_next_is_ignored(&mut store());
    }

    /// `peek_next_number` (DuckDB-only — it reads the
    /// `invoice_sequence_state` row) returns `start_value.max(1)` for an
    /// untouched bucket and the advanced counter after an allocation,
    /// WITHOUT itself burning a number.
    #[test]
    fn peek_reflects_counter_without_advancing() {
        let mut store = store();
        store.ensure_schema().expect("ensure_schema");
        let series_id = create_continuous_series(&mut store);
        let now = datetime!(2026-06-15 12:00:00 UTC);

        // Allocate once (start_value=7) → reserved 7, stored next is 8.
        let first = store
            .allocate_and_insert(args(series_id, 7, None), now)
            .expect("allocate");
        assert_eq!(fresh_seq(first), 7);

        let conn = store.into_connection();
        // Peek does not advance: stored next is 8, and a second peek is
        // still 8.
        assert_eq!(
            peek_next_number(&conn, series_id, 2026, 7).expect("peek"),
            8
        );
        assert_eq!(
            peek_next_number(&conn, series_id, 2026, 7).expect("peek again"),
            8,
            "peek is read-only — repeated calls do not advance the counter"
        );
    }

    /// Peeking a bucket that has no state row yet returns the seed
    /// (`start_value.max(1)`), mirroring the allocator's first-INSERT path.
    #[test]
    fn peek_empty_bucket_returns_seed() {
        let mut store = store();
        store.ensure_schema().expect("ensure_schema");
        let series_id = create_continuous_series(&mut store);
        let conn = store.into_connection();
        assert_eq!(
            peek_next_number(&conn, series_id, 2026, 42).expect("peek empty"),
            42,
            "empty bucket peeks at start_value"
        );
        // `start_value = 0` clamps to 1 (mirrors the allocator seed).
        assert_eq!(
            peek_next_number(&conn, series_id, 2026, 0).expect("peek empty zero"),
            1
        );
    }
}
