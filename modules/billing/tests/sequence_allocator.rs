//! Sequence-allocator conformance test for ADR-0009 §3.
//!
//! Verifies the four invariants that make the allocator regulator-safe:
//!
//! 1. **Gap-free under happy-path**. N sequential issuances produce
//!    contiguous numbers 1..=N.
//! 2. **Idempotent under retry**. Reissuing the same `IdempotencyKey`
//!    returns the original outcome unchanged — no new number is burned.
//! 3. **Void path preserves gap-free**. A voided reservation does NOT
//!    free its number; the next issuance gets the next number, not the
//!    voided one.
//! 4. **Unknown series fails loud**. Issuing against a series code that
//!    was never created surfaces [`BillingError::SeriesNotFound`], not a
//!    silent no-op.
//!
//! Each invariant is asserted against BOTH the in-memory adapter and the
//! DuckDB adapter — same trait, same test, two backends. Per ADR-0006
//! §Conformance, divergence between adapters is itself a bug.

use time::OffsetDateTime;

use aberp_billing::{
    BillingError, BillingStore, Clock, CustomerId, DuckDbBillingStore, Huf, IdempotencyKey,
    InMemoryBillingStore, InvoiceId, InvoiceSeries, IssueInvoiceCommand, IssueInvoiceOutcome,
    LineItem, ResetPolicy, SeriesCode, SeriesId,
};

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

/// Fixed clock — every issuance gets the same timestamp. Real clocks are
/// non-deterministic; for these tests we need bit-stable outcomes.
#[derive(Debug)]
struct FixedClock(OffsetDateTime);

impl Clock for FixedClock {
    fn now_utc(&self) -> OffsetDateTime {
        self.0
    }
}

fn fixed_clock() -> FixedClock {
    FixedClock(OffsetDateTime::UNIX_EPOCH)
}

fn series_code() -> SeriesCode {
    SeriesCode::new("INV-test").expect("series code is valid")
}

fn create_default_series<S: BillingStore + ?Sized>(store: &mut S) -> SeriesId {
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
        quantity: 1,
        unit_price: Huf(1_000),
        vat_rate_basis_points: 2700, // 27% Hungarian standard rate
    }
}

fn command_with_key(key: IdempotencyKey) -> IssueInvoiceCommand {
    IssueInvoiceCommand {
        idempotency_key: key,
        series_code: series_code(),
        customer_id: CustomerId::new(),
        lines: vec![one_line()],
    }
}

// ──────────────────────────────────────────────────────────────────────
// Invariant tests — parametrized over both adapters via a helper.
// ──────────────────────────────────────────────────────────────────────

fn run_gap_free<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_default_series(store);
    let clock = fixed_clock();

    let mut numbers = Vec::new();
    for _ in 0..3 {
        let outcome =
            aberp_billing::issue_invoice(store, &clock, command_with_key(IdempotencyKey::new()))
                .expect("issue");
        match outcome {
            IssueInvoiceOutcome::Fresh { invoice, .. } => numbers.push(invoice.sequence_number),
            IssueInvoiceOutcome::Replay { .. } => panic!("fresh issuances should not Replay"),
        }
    }

    assert_eq!(numbers, vec![1, 2, 3], "gap-free contiguous numbering");

    let reservations = store.list_reservations(series_id).expect("list");
    assert_eq!(reservations.len(), 3);
    assert_eq!(
        reservations.iter().map(|r| r.number).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "reservation table matches the issued numbers",
    );
}

fn run_idempotent_retry<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    create_default_series(store);
    let clock = fixed_clock();

    let key = IdempotencyKey::new();
    let first =
        aberp_billing::issue_invoice(store, &clock, command_with_key(key)).expect("first issue");
    let first_invoice = match &first {
        IssueInvoiceOutcome::Fresh { invoice, .. } => invoice.clone(),
        _ => panic!("first call should be Fresh"),
    };

    // Re-issue the SAME command (same idempotency key) — must not burn
    // a new number.
    let replay =
        aberp_billing::issue_invoice(store, &clock, command_with_key(key)).expect("replay issue");
    match replay {
        IssueInvoiceOutcome::Replay { invoice, .. } => {
            assert_eq!(invoice.sequence_number, first_invoice.sequence_number);
            assert_eq!(invoice.id, first_invoice.id);
        }
        IssueInvoiceOutcome::Fresh { .. } => {
            panic!("retry with same idempotency key must Replay, not Fresh")
        }
    }

    // The third issuance with a DIFFERENT key gets the next number (2),
    // not 1 — proving the replay did not advance the counter.
    let next = aberp_billing::issue_invoice(store, &clock, command_with_key(IdempotencyKey::new()))
        .expect("next issue");
    match next {
        IssueInvoiceOutcome::Fresh { invoice, .. } => {
            assert_eq!(invoice.sequence_number, 2);
        }
        _ => panic!("a fresh idempotency key must produce Fresh"),
    }
}

fn run_void_preserves_gap_free<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let series_id = create_default_series(store);
    let clock = fixed_clock();

    // Issue 1, 2, 3. We only need each invoice's ID for the void step;
    // `InvoiceId` is `Copy`, `ReadyInvoice` is not — push the id directly.
    let mut invoice_ids: Vec<InvoiceId> = Vec::new();
    for _ in 0..3 {
        let outcome =
            aberp_billing::issue_invoice(store, &clock, command_with_key(IdempotencyKey::new()))
                .expect("issue");
        invoice_ids.push(outcome.invoice().id);
    }
    let invoice_2_id = invoice_ids[1];

    // Void invoice #2's reservation.
    store
        .void_reservation(
            invoice_2_id,
            "operator cancelled".to_string(),
            clock.now_utc(),
        )
        .expect("void");

    // Issue a fourth invoice — it must get sequence number 4, not 2.
    let next = aberp_billing::issue_invoice(store, &clock, command_with_key(IdempotencyKey::new()))
        .expect("next issue");
    match next {
        IssueInvoiceOutcome::Fresh { invoice, .. } => {
            assert_eq!(
                invoice.sequence_number, 4,
                "voided number must NOT be reused — Hungarian gap-free rule"
            );
        }
        _ => panic!("fresh"),
    }

    // The reservation table should still hold all four numbers.
    let reservations = store.list_reservations(series_id).expect("list");
    let numbers: Vec<u64> = reservations.iter().map(|r| r.number).collect();
    assert_eq!(numbers, vec![1, 2, 3, 4]);
}

fn run_unknown_series_fails_loud<S: BillingStore + ?Sized>(store: &mut S) {
    store.ensure_schema().expect("ensure_schema");
    let clock = fixed_clock();
    // Note: no `create_default_series` here — the series doesn't exist.
    let result =
        aberp_billing::issue_invoice(store, &clock, command_with_key(IdempotencyKey::new()));
    match result {
        Err(BillingError::SeriesNotFound(code)) => {
            assert_eq!(code, "INV-test");
        }
        other => {
            panic!("unknown series must surface SeriesNotFound, got {other:?} — fail-loud broken")
        }
    }
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
    fn gap_free() {
        run_gap_free(&mut store());
    }

    #[test]
    fn idempotent_retry() {
        run_idempotent_retry(&mut store());
    }

    #[test]
    fn void_preserves_gap_free() {
        run_void_preserves_gap_free(&mut store());
    }

    #[test]
    fn unknown_series_fails_loud() {
        run_unknown_series_fails_loud(&mut store());
    }
}

mod duckdb_backed {
    use super::*;

    fn store() -> DuckDbBillingStore {
        DuckDbBillingStore::open_in_memory().expect("open in-memory DuckDB store")
    }

    #[test]
    fn gap_free() {
        run_gap_free(&mut store());
    }

    #[test]
    fn idempotent_retry() {
        run_idempotent_retry(&mut store());
    }

    #[test]
    fn void_preserves_gap_free() {
        run_void_preserves_gap_free(&mut store());
    }

    #[test]
    fn unknown_series_fails_loud() {
        run_unknown_series_fails_loud(&mut store());
    }
}
