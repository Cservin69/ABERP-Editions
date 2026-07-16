//! PR-90 / ADR-0045 §2 — annual-reset conformance for the sequence
//! allocator.
//!
//! The pre-PR-90 allocator loud-failed with
//! `BillingError::AnnualResetUnimplemented` whenever a series carried
//! `ResetPolicy::AnnualOnFiscalYear`. PR-90 lifts that gate by keying
//! the bucket on the invoice's immutable issue-date year. These tests
//! pin the resulting behaviour:
//!
//! 1. **Year roll resets the counter.** The first invoice of a new
//!    fiscal year starts fresh from `start_value` (default 1) — gap-free
//!    within each year.
//! 2. **Gap-free within the new-year bucket.** After the reset, the
//!    counter increments by 1 within the new year — no rewind, no
//!    skip.
//! 3. **`Never` policy is unaffected.** A series with
//!    `ResetPolicy::Never` continues to run a single continuous bucket
//!    across the year boundary — byte-identical to the pre-PR-90
//!    behaviour for legacy `INV-default` tenants.
//! 4. **The fiscal year derives from the draft's `issue_date`.** Not
//!    wall-clock at allocate time; the rendered Year segment and the
//!    counter's reset-year therefore agree by construction.
//! 5. **`start_value > 1` seeds the bucket on first INSERT.** Subsequent
//!    allocations within the same bucket increment from the stored
//!    counter, preserving the §169 gap-free invariant.
//! 6. **Mid-stream `Never → OnYearChange` flip.** Once the series row's
//!    policy is updated (the binary's `ensure_series` does this on
//!    every issuance), the next allocation lands in the issue-year
//!    bucket. Pre-flip allocations stay in the `fiscal_year = 0`
//!    bucket — they are not retroactively re-bucketed.
//!
//! Each invariant runs against BOTH the in-memory adapter and the
//! DuckDB adapter (ADR-0006 §Conformance — divergence between adapters
//! is itself a bug).

use time::macros::datetime;
use time::OffsetDateTime;

use aberp_billing::{
    AllocateArgs, AllocateOutcome, BillingStore, CustomerId, DraftInvoice, DuckDbBillingStore, Huf,
    IdempotencyKey, InMemoryBillingStore, InvoiceId, InvoiceSeries, LineItem, ResetPolicy,
    SeriesCode, SeriesId,
};

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

fn series_code() -> SeriesCode {
    SeriesCode::new("ABERP").expect("series code is valid")
}

fn create_annual_series<S: BillingStore + ?Sized>(store: &mut S) -> SeriesId {
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: series_code(),
        reset_policy: ResetPolicy::AnnualOnFiscalYear,
        fiscal_year: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
    };
    store.create_series(&series).expect("create series");
    series.id
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

fn allocate_at(series_id: SeriesId, issue_date: OffsetDateTime, start_value: u64) -> AllocateArgs {
    AllocateArgs {
        series_id,
        draft: DraftInvoice {
            id: InvoiceId::new(),
            series_id,
            customer_id: CustomerId::new(),
            lines: vec![one_line()],
            issue_date,
            payment_deadline: issue_date.date(),
            delivery_date: issue_date.date(),
        },
        idempotency_key: IdempotencyKey::new(),
        currency: aberp_billing::Currency::Huf,
        rate_metadata: None,
        bank_snapshot: None,
        invoice_note: None,
        email_recipient_override: None,
        start_value,
        sequence_floor: None,
    }
}

fn fresh_seq(outcome: AllocateOutcome) -> u64 {
    match outcome {
        AllocateOutcome::Fresh { invoice, .. } => invoice.sequence_number,
        AllocateOutcome::Replay { .. } => panic!("fresh issuance unexpectedly Replay"),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Invariant runners — parametrised over adapters
// ──────────────────────────────────────────────────────────────────────

/// Invariant 1 + 2 — year roll resets to start_value=1, gap-free within
/// each year. Ervin's primary template (`ABERP-{Year}/{Counter:6}`,
/// start_value=1) renders `…2026/000123` then `…2027/000001` on Jan 1.
fn run_year_roll_resets_to_one<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_annual_series(store);
    let mid_2026 = datetime!(2026-06-15 12:00:00 UTC);
    let new_year_2027 = datetime!(2027-01-01 09:00:00 UTC);

    // Issue 3 invoices in 2026.
    let mut seqs_2026 = Vec::new();
    for _ in 0..3 {
        let out = store
            .allocate_and_insert(allocate_at(series_id, mid_2026, 1), mid_2026)
            .expect("allocate in 2026");
        seqs_2026.push(fresh_seq(out));
    }
    assert_eq!(seqs_2026, vec![1, 2, 3], "gap-free within 2026 bucket");

    // First invoice of 2027 — counter resets to start_value (1).
    let first_2027 = store
        .allocate_and_insert(allocate_at(series_id, new_year_2027, 1), new_year_2027)
        .expect("allocate first of 2027");
    assert_eq!(
        fresh_seq(first_2027),
        1,
        "first invoice of new fiscal year resets to start_value (Hungarian convention)"
    );

    // Next two 2027 invoices: 2, 3 — gap-free within the new bucket.
    let mut seqs_2027 = vec![1];
    for _ in 0..2 {
        let out = store
            .allocate_and_insert(allocate_at(series_id, new_year_2027, 1), new_year_2027)
            .expect("allocate further 2027");
        seqs_2027.push(fresh_seq(out));
    }
    assert_eq!(seqs_2027, vec![1, 2, 3], "gap-free within 2027 bucket");

    // Reservation table holds 3 rows for 2026 + 3 rows for 2027,
    // segregated by fiscal_year — the UNIQUE(series_id, fiscal_year,
    // number) constraint allows 2026/1 to coexist with 2027/1.
    let reservations = store.list_reservations(series_id).expect("list");
    assert_eq!(reservations.len(), 6, "6 reservations across two years");
    let mut by_year: std::collections::BTreeMap<i32, Vec<u64>> = std::collections::BTreeMap::new();
    for r in reservations {
        by_year.entry(r.fiscal_year).or_default().push(r.number);
    }
    assert_eq!(
        by_year.get(&2026).cloned().unwrap_or_default(),
        vec![1, 2, 3]
    );
    assert_eq!(
        by_year.get(&2027).cloned().unwrap_or_default(),
        vec![1, 2, 3]
    );
}

/// Invariant 3 — `Never` policy preserves continuous numbering across
/// the year boundary. The pre-PR-89 `INV-default` migration tenant
/// MUST see byte-identical behaviour (no surprise reset on Jan 1).
fn run_never_policy_is_continuous_across_years<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_continuous_series(store);

    // 2 in 2026, 2 in 2027 — under Never the counter is global to the
    // series, so the numbers run 1, 2, 3, 4 regardless of issue year.
    let dates = [
        datetime!(2026-12-30 12:00:00 UTC),
        datetime!(2026-12-31 23:59:00 UTC),
        datetime!(2027-01-01 00:01:00 UTC),
        datetime!(2027-01-15 10:00:00 UTC),
    ];
    let mut seqs = Vec::new();
    for d in dates {
        let out = store
            .allocate_and_insert(allocate_at(series_id, d, 1), d)
            .expect("allocate");
        seqs.push(fresh_seq(out));
    }
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4],
        "Never policy must run continuous across Jan 1 (pre-PR-90 behaviour preserved)"
    );

    // All 4 reservations land in fiscal_year=0 (the Never bucket).
    let reservations = store.list_reservations(series_id).expect("list");
    assert_eq!(reservations.len(), 4);
    for r in reservations {
        assert_eq!(
            r.fiscal_year, 0,
            "Never policy reservations live in the fiscal_year=0 bucket"
        );
    }
}

/// Invariant 4 — fiscal year derives from `draft.issue_date.year()`,
/// not from the wall-clock `now` parameter. A storno issued in early
/// 2027 whose draft.issue_date was stamped Dec 31 2026 (rare but
/// possible under the PR-84 operator-supplied date model) must bucket
/// to 2026, not 2027.
fn run_fiscal_year_from_issue_date_not_wall_clock<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_annual_series(store);
    let issue_in_2026 = datetime!(2026-12-31 23:00:00 UTC);
    let wall_clock_in_2027 = datetime!(2027-01-02 10:00:00 UTC);

    // `now` (the tx-time parameter) is 2027, but `draft.issue_date` is
    // 2026 — the invoice must bucket to 2026 (the year stamped on the
    // wire is what counts).
    let out = store
        .allocate_and_insert(allocate_at(series_id, issue_in_2026, 1), wall_clock_in_2027)
        .expect("allocate");
    let invoice = match out {
        AllocateOutcome::Fresh { invoice, .. } => invoice,
        _ => panic!("Fresh"),
    };
    assert_eq!(
        invoice.fiscal_year, 2026,
        "fiscal_year derives from draft.issue_date.year(), not the tx-time `now`"
    );
    assert_eq!(invoice.sequence_number, 1, "fresh 2026 bucket starts at 1");

    // The reservation row carries the same fiscal_year=2026.
    let reservations = store.list_reservations(series_id).expect("list");
    assert_eq!(reservations.len(), 1);
    assert_eq!(reservations[0].fiscal_year, 2026);
    assert_eq!(reservations[0].number, 1);
}

/// Invariant 5 — `start_value > 1` seeds the bucket on first INSERT.
/// Operator migration case: continuing an external sequence at e.g.
/// 1247. The first invoice burns 1247; the second burns 1248
/// (gap-free); year-roll buckets re-apply start_value (per-bucket
/// seed, not one-time).
fn run_start_value_seeds_each_bucket<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_annual_series(store);
    let mid_2026 = datetime!(2026-06-15 12:00:00 UTC);
    let mid_2027 = datetime!(2027-06-15 12:00:00 UTC);

    // First 2026 invoice burns 1247 (the operator's migration seed).
    let first_2026 = store
        .allocate_and_insert(allocate_at(series_id, mid_2026, 1247), mid_2026)
        .expect("allocate first 2026");
    assert_eq!(fresh_seq(first_2026), 1247, "first INSERT uses start_value");

    // Second 2026 invoice burns 1248 — gap-free continues from the
    // seed.
    let second_2026 = store
        .allocate_and_insert(allocate_at(series_id, mid_2026, 1247), mid_2026)
        .expect("allocate second 2026");
    assert_eq!(fresh_seq(second_2026), 1248, "gap-free from seeded value");

    // First 2027 invoice — new bucket, re-applies start_value (per the
    // ADR-0045 §2 per-bucket-seed semantics — for steady-state
    // operators start_value stays at 1 so this rarely matters in
    // production, but the regulation-adjacent contract is pinned).
    let first_2027 = store
        .allocate_and_insert(allocate_at(series_id, mid_2027, 1247), mid_2027)
        .expect("allocate first 2027");
    assert_eq!(
        fresh_seq(first_2027),
        1247,
        "new-year bucket re-applies start_value (per-bucket seed)"
    );
}

/// Invariant 6 — mid-stream `Never → OnYearChange` flip. The binary's
/// `ensure_series` syncs the series row's policy from the operator's
/// template choice on every issuance; this test exercises the
/// resulting allocator behaviour.
fn run_mid_stream_policy_flip<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_continuous_series(store);
    let mid_2026 = datetime!(2026-06-15 12:00:00 UTC);

    // 2 invoices under Never — land in fiscal_year=0 bucket.
    for _ in 0..2 {
        store
            .allocate_and_insert(allocate_at(series_id, mid_2026, 1), mid_2026)
            .expect("allocate under Never");
    }

    // Operator flips template to OnYearChange. `ensure_series` would
    // call `update_series_reset_policy`; replicate it here.
    store
        .update_series_reset_policy(series_id, ResetPolicy::AnnualOnFiscalYear)
        .expect("update reset_policy");

    // Next 2026 issuance lands in the fiscal_year=2026 bucket — a
    // fresh bucket that starts at start_value (1). Pre-flip
    // allocations stay in fiscal_year=0 (no retroactive re-bucket).
    let post_flip = store
        .allocate_and_insert(allocate_at(series_id, mid_2026, 1), mid_2026)
        .expect("allocate post-flip");
    let invoice = match post_flip {
        AllocateOutcome::Fresh { invoice, .. } => invoice,
        _ => panic!("Fresh"),
    };
    assert_eq!(invoice.fiscal_year, 2026, "post-flip bucket = issue year");
    assert_eq!(
        invoice.sequence_number, 1,
        "fresh-bucket counter starts at start_value"
    );

    let reservations = store.list_reservations(series_id).expect("list");
    let mut by_year: std::collections::BTreeMap<i32, Vec<u64>> = std::collections::BTreeMap::new();
    for r in reservations {
        by_year.entry(r.fiscal_year).or_default().push(r.number);
    }
    assert_eq!(by_year.get(&0).cloned().unwrap_or_default(), vec![1, 2]);
    assert_eq!(by_year.get(&2026).cloned().unwrap_or_default(), vec![1]);
}

// ──────────────────────────────────────────────────────────────────────
// Per-adapter wrappers
// ──────────────────────────────────────────────────────────────────────

mod in_memory {
    use super::*;

    fn store() -> InMemoryBillingStore {
        InMemoryBillingStore::new()
    }

    #[test]
    fn year_roll_resets_to_one() {
        run_year_roll_resets_to_one(&mut store());
    }

    #[test]
    fn never_policy_is_continuous_across_years() {
        run_never_policy_is_continuous_across_years(&mut store());
    }

    #[test]
    fn fiscal_year_from_issue_date_not_wall_clock() {
        run_fiscal_year_from_issue_date_not_wall_clock(&mut store());
    }

    #[test]
    fn start_value_seeds_each_bucket() {
        run_start_value_seeds_each_bucket(&mut store());
    }

    #[test]
    fn mid_stream_policy_flip() {
        run_mid_stream_policy_flip(&mut store());
    }
}

mod duckdb_backed {
    use super::*;

    fn store() -> DuckDbBillingStore {
        DuckDbBillingStore::open_in_memory().expect("open in-memory DuckDB store")
    }

    #[test]
    fn year_roll_resets_to_one() {
        run_year_roll_resets_to_one(&mut store());
    }

    #[test]
    fn never_policy_is_continuous_across_years() {
        run_never_policy_is_continuous_across_years(&mut store());
    }

    #[test]
    fn fiscal_year_from_issue_date_not_wall_clock() {
        run_fiscal_year_from_issue_date_not_wall_clock(&mut store());
    }

    #[test]
    fn start_value_seeds_each_bucket() {
        run_start_value_seeds_each_bucket(&mut store());
    }

    #[test]
    fn mid_stream_policy_flip() {
        run_mid_stream_policy_flip(&mut store());
    }
}
