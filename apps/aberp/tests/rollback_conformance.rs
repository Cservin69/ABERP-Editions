//! Rollback conformance for the PR-6 single-transaction issuance path.
//!
//! These tests pin the load-bearing claim of PR-6: a failure between
//! `billing::allocate_in_tx` and `tx.commit()` — whether by `Err(_)`
//! return or by panic — leaves the tenant DuckDB in **exactly** its
//! pre-call state. Both halves (billing rows and audit-ledger rows)
//! roll back together. ADR-0008 §Storage: "If the state change rolls
//! back, the ledger entry rolls back too."
//!
//! Two failure modes are exercised, per the session-6 test-style
//! decision ("both — panic test + drop test"):
//!
//! 1. **Drop-without-commit.** The orchestrator returns before calling
//!    `tx.commit()`; `Transaction::drop` rolls back. This is the
//!    failure mode any `Err(_)` from `allocate_in_tx` or `append_in_tx`
//!    inside `run_single_tx` produces.
//! 2. **Panic-injection.** A panic unwinds across the transaction;
//!    `Transaction::drop` still runs during unwind, and the same
//!    rollback applies. `std::panic::catch_unwind` captures the panic
//!    so the test can inspect the post-state.
//!
//! Per CLAUDE.md rule 9, each test asserts the precise post-state — all
//! five mutating tables empty (billing + audit) plus a clean
//! `verify_chain` — not just "no panic propagated".

use aberp::nav_xml;
use aberp_audit_ledger::{
    self as audit_ledger, Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_billing::{
    self as billing, AllocateArgs, BillingStore, CustomerId, DraftInvoice, DuckDbBillingStore, Huf,
    IdempotencyKey, InvoiceId, InvoiceSeries, LineItem, ResetPolicy, SeriesCode, SeriesId,
};
use duckdb::Connection;
use std::panic::AssertUnwindSafe;
use time::OffsetDateTime;

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

const TEST_BINARY_HASH: BinaryHash = BinaryHash::from_bytes([0xCD; 32]);

fn tenant() -> TenantId {
    TenantId::new("tenant-rollback-test").expect("test tenant id is valid")
}

fn series_code() -> SeriesCode {
    SeriesCode::new("ROLL".to_string()).expect("test series code valid")
}

/// Per-test temp DuckDB path. Matches the pattern used by the
/// audit-ledger crate's tamper tests.
fn temp_db_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-rollback-conformance-{}-{}-{:?}.duckdb",
        std::process::id(),
        tag,
        std::thread::current().id(),
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Run the same idempotent pre-tx setup the binary does: open the
/// tenant DB through `DuckDbBillingStore`, create billing schema, seed
/// the series, take the `Connection` back, create audit schema. Returns
/// the bare `Connection` ready for `Connection::transaction()`.
fn pre_tx_setup(path: &std::path::Path) -> (Connection, InvoiceSeries) {
    let mut billing = DuckDbBillingStore::open(path).expect("open billing store");
    billing.ensure_schema().expect("ensure billing schema");
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: series_code(),
        reset_policy: ResetPolicy::Never,
        fiscal_year: None,
        created_at: OffsetDateTime::now_utc(),
    };
    billing.create_series(&series).expect("create series");
    let conn = billing.into_connection();
    audit_ledger::ensure_schema(&conn).expect("ensure audit-ledger schema");
    (conn, series)
}

fn build_allocate_args(series_id: SeriesId) -> AllocateArgs {
    AllocateArgs {
        series_id,
        // PR-73 — rollback conformance test doesn't exercise the bank
        // picker; `bank_snapshot: None` keeps the test's allocation
        // shape pre-PR-73 (NULL across the five bank columns).
        draft: DraftInvoice {
            id: InvoiceId::new(),
            series_id,
            customer_id: CustomerId::new(),
            lines: vec![LineItem {
                description: "rollback-line".to_string(),
                quantity: rust_decimal::Decimal::from(1),
                unit_price: Huf(1_000),
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            }],
            issue_date: OffsetDateTime::now_utc(),
            // PR-84 — rollback conformance fixture defaults both date
            // fields to the issue date.
            payment_deadline: OffsetDateTime::now_utc().date(),
            delivery_date: OffsetDateTime::now_utc().date(),
        },
        idempotency_key: IdempotencyKey::new(),
        // PR-44γ — rollback conformance exercises the HUF path. The
        // C10 byte-identical invariant prerequisite is preserved here:
        // HUF rows carry no rate metadata.
        currency: aberp_billing::Currency::Huf,
        rate_metadata: None,
        bank_snapshot: None,
        invoice_note: None,
        email_recipient_override: None,
        start_value: 1,
        sequence_floor: None,
    }
}

fn ledger_meta() -> LedgerMeta {
    LedgerMeta::new(tenant(), TEST_BINARY_HASH)
}

/// Count rows in each of the five tables the issuance path mutates.
/// Returns `(seq_state, reservation, invoice, invoice_line, audit_ledger)`.
fn row_counts(path: &std::path::Path) -> (u64, u64, u64, u64, u64) {
    let conn = Connection::open(path).expect("re-open for inspection");
    let count = |sql: &str| -> u64 {
        let mut stmt = conn.prepare(sql).expect("prepare count");
        let mut rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .expect("query_map count");
        rows.next().expect("count row").expect("count value") as u64
    };
    (
        count("SELECT COUNT(*) FROM invoice_sequence_state"),
        count("SELECT COUNT(*) FROM invoice_sequence_reservation"),
        count("SELECT COUNT(*) FROM invoice"),
        count("SELECT COUNT(*) FROM invoice_line"),
        count("SELECT COUNT(*) FROM audit_ledger"),
    )
}

// ──────────────────────────────────────────────────────────────────────
// Test 1 — drop-without-commit
// ──────────────────────────────────────────────────────────────────────

#[test]
fn drop_without_commit_rolls_back_billing_and_audit() {
    let path = temp_db_path("drop");
    let (mut conn, series) = pre_tx_setup(&path);

    // Run the same call sequence as `run_single_tx` *except* the final
    // `tx.commit()`. Letting `tx` drop at the end of the block engages
    // DuckDB's Drop-rolls-back contract.
    {
        let tx = conn.transaction().expect("begin tx");
        let _outcome = billing::allocate_in_tx(
            &tx,
            build_allocate_args(series.id),
            OffsetDateTime::now_utc(),
        )
        .expect("allocate_in_tx Ok");
        audit_ledger::append_in_tx(
            &tx,
            &ledger_meta(),
            EventKind::InvoiceSequenceReserved,
            b"{\"test\":\"first\"}".to_vec(),
            Actor::test_only(),
            Some("idem-drop-1".to_string()),
        )
        .expect("append_in_tx first Ok");
        // No tx.commit(); tx drops here → rollback.
    }
    drop(conn);

    let (seq_state, reservations, invoices, lines, audit) = row_counts(&path);
    assert_eq!(
        seq_state, 0,
        "invoice_sequence_state should be empty after rollback"
    );
    assert_eq!(
        reservations, 0,
        "invoice_sequence_reservation should be empty after rollback"
    );
    assert_eq!(invoices, 0, "invoice should be empty after rollback");
    assert_eq!(lines, 0, "invoice_line should be empty after rollback");
    assert_eq!(audit, 0, "audit_ledger should be empty after rollback");

    // The audit chain over zero entries is the empty-genesis case, which
    // verifies cleanly per the audit-ledger crate's own conformance.
    let ledger = Ledger::open(&path, tenant(), TEST_BINARY_HASH).expect("re-open ledger");
    let verified = ledger.verify_chain().expect("verify_chain Ok");
    assert_eq!(verified, 0, "verify_chain should report 0 entries");

    let _ = std::fs::remove_file(&path);
}

// ──────────────────────────────────────────────────────────────────────
// Test 1b/1c — S375 render-fault BEFORE commit rolls back (atomicity)
//
// S375 moved the NAV-XML render+XSD-validate+write step from AFTER the
// commit to BEFORE it (inside `run_single_tx`'s `render_and_write`
// closure). Pre-S375 a render/write failure left a committed
// invoice/storno row + audit draft with NO XML on disk — a phantom
// "Ready" row whose Submit was broken. These tests pin the new
// contract: a render/write `Err` arriving before `tx.commit()` rolls
// back BOTH the billing allocation and the audit appends, exactly like
// the drop/panic tests above (the render `Err` IS just an early
// drop-without-commit).
//
// They replicate the in-tx call sequence rather than calling the
// crate-private `run_single_tx` directly — the same approach Test 1/2
// take. The write fault is real: `write_to_path` targets a directory,
// so `File::create` fails the way an out-of-space / permission / XSD
// failure would.
// ──────────────────────────────────────────────────────────────────────

#[test]
fn render_fault_before_commit_rolls_back_invoice_issuance() {
    let path = temp_db_path("render-fault-invoice");
    let (mut conn, series) = pre_tx_setup(&path);

    // A real directory — `write_to_path` (File::create) cannot create a
    // file AT a directory path, so it returns Err. This stands in for
    // the render/XSD-validate/disk-write failure the S375 closure runs
    // before commit.
    let fault_dir = path.with_extension("faultdir");
    std::fs::create_dir_all(&fault_dir).expect("create fault dir");

    {
        let tx = conn.transaction().expect("begin tx");
        let _outcome = billing::allocate_in_tx(
            &tx,
            build_allocate_args(series.id),
            OffsetDateTime::now_utc(),
        )
        .expect("allocate_in_tx Ok");
        // The issue path's Fresh-branch shape: two audit appends.
        audit_ledger::append_in_tx(
            &tx,
            &ledger_meta(),
            EventKind::InvoiceSequenceReserved,
            b"{\"k\":\"seq\"}".to_vec(),
            Actor::test_only(),
            Some("idem-render-inv".to_string()),
        )
        .expect("append InvoiceSequenceReserved Ok");
        audit_ledger::append_in_tx(
            &tx,
            &ledger_meta(),
            EventKind::InvoiceDraftCreated,
            b"{\"k\":\"draft\"}".to_vec(),
            Actor::test_only(),
            Some("idem-render-inv".to_string()),
        )
        .expect("append InvoiceDraftCreated Ok");

        // S375 — the in-tx render+write step. Its Err makes
        // `run_single_tx` return BEFORE `tx.commit()`; here we emulate
        // that by NOT committing after the fault.
        let render_result = nav_xml::write_to_path(&fault_dir, b"<InvoiceData/>");
        assert!(
            render_result.is_err(),
            "fault injection: writing XML to a directory path must fail"
        );
        // No tx.commit(): tx drops → rollback of allocate + both appends.
    }
    drop(conn);

    let (seq_state, reservations, invoices, lines, audit) = row_counts(&path);
    assert_eq!(seq_state, 0, "seq_state empty after render-fault rollback");
    assert_eq!(
        reservations, 0,
        "reservation empty after render-fault rollback"
    );
    assert_eq!(invoices, 0, "invoice empty after render-fault rollback");
    assert_eq!(lines, 0, "invoice_line empty after render-fault rollback");
    assert_eq!(
        audit, 0,
        "audit_ledger empty after render-fault rollback — no committed-but-XML-less row"
    );

    let ledger = Ledger::open(&path, tenant(), TEST_BINARY_HASH).expect("re-open ledger");
    assert_eq!(ledger.verify_chain().expect("verify_chain Ok"), 0);

    let _ = std::fs::remove_dir_all(&fault_dir);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn render_fault_before_commit_rolls_back_storno_issuance() {
    let path = temp_db_path("render-fault-storno");
    let (mut conn, series) = pre_tx_setup(&path);

    let fault_dir = path.with_extension("faultdir");
    std::fs::create_dir_all(&fault_dir).expect("create fault dir");

    {
        let tx = conn.transaction().expect("begin tx");
        let _outcome = billing::allocate_in_tx(
            &tx,
            build_allocate_args(series.id),
            OffsetDateTime::now_utc(),
        )
        .expect("allocate_in_tx Ok");
        // The storno path's Fresh-branch shape: THREE audit appends
        // (Reserved + DraftCreated + StornoIssued).
        for (kind, k) in [
            (EventKind::InvoiceSequenceReserved, "seq"),
            (EventKind::InvoiceDraftCreated, "draft"),
            (EventKind::InvoiceStornoIssued, "storno"),
        ] {
            audit_ledger::append_in_tx(
                &tx,
                &ledger_meta(),
                kind,
                format!("{{\"k\":\"{k}\"}}").into_bytes(),
                Actor::test_only(),
                Some("idem-render-storno".to_string()),
            )
            .expect("append Ok");
        }

        // S375 — render+write fault before commit.
        let render_result = nav_xml::write_to_path(&fault_dir, b"<InvoiceData/>");
        assert!(
            render_result.is_err(),
            "fault injection: writing storno XML to a directory path must fail"
        );
        // No tx.commit(): tx drops → rollback of allocate + all three appends.
    }
    drop(conn);

    let (seq_state, reservations, invoices, lines, audit) = row_counts(&path);
    assert_eq!(
        seq_state, 0,
        "seq_state empty after storno render-fault rollback"
    );
    assert_eq!(
        reservations, 0,
        "reservation empty after storno render-fault rollback"
    );
    assert_eq!(
        invoices, 0,
        "invoice empty after storno render-fault rollback"
    );
    assert_eq!(
        lines, 0,
        "invoice_line empty after storno render-fault rollback"
    );
    assert_eq!(
        audit, 0,
        "audit_ledger empty after storno render-fault rollback — no committed chain-link without XML"
    );

    let ledger = Ledger::open(&path, tenant(), TEST_BINARY_HASH).expect("re-open ledger");
    assert_eq!(ledger.verify_chain().expect("verify_chain Ok"), 0);

    let _ = std::fs::remove_dir_all(&fault_dir);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn render_fault_before_commit_rolls_back_modification_issuance() {
    // S381/E (task_8de31599) — issue_modification.rs was the last
    // un-ported sibling of the pre-S375 storno bug: its render+write ran
    // post-commit, so a render/validate/write failure left a committed
    // modification row + audit chain-link with no XML on disk. This pins
    // the new contract — the render `Err` arriving before `tx.commit()`
    // rolls BOTH the billing allocation and the (Reserved + DraftCreated
    // + ModificationIssued) appends back, like the storno test above.
    let path = temp_db_path("render-fault-modification");
    let (mut conn, series) = pre_tx_setup(&path);

    let fault_dir = path.with_extension("faultdir");
    std::fs::create_dir_all(&fault_dir).expect("create fault dir");

    {
        let tx = conn.transaction().expect("begin tx");
        let _outcome = billing::allocate_in_tx(
            &tx,
            build_allocate_args(series.id),
            OffsetDateTime::now_utc(),
        )
        .expect("allocate_in_tx Ok");
        // The modification path's Fresh-branch shape: THREE audit appends
        // (Reserved + DraftCreated + ModificationIssued).
        for (kind, k) in [
            (EventKind::InvoiceSequenceReserved, "seq"),
            (EventKind::InvoiceDraftCreated, "draft"),
            (EventKind::InvoiceModificationIssued, "modif"),
        ] {
            audit_ledger::append_in_tx(
                &tx,
                &ledger_meta(),
                kind,
                format!("{{\"k\":\"{k}\"}}").into_bytes(),
                Actor::test_only(),
                Some("idem-render-modif".to_string()),
            )
            .expect("append Ok");
        }

        // S375 — render+write fault before commit.
        let render_result = nav_xml::write_to_path(&fault_dir, b"<InvoiceData/>");
        assert!(
            render_result.is_err(),
            "fault injection: writing modification XML to a directory path must fail"
        );
        // No tx.commit(): tx drops → rollback of allocate + all three appends.
    }
    drop(conn);

    let (seq_state, reservations, invoices, lines, audit) = row_counts(&path);
    assert_eq!(
        seq_state, 0,
        "seq_state empty after modification render-fault rollback"
    );
    assert_eq!(
        reservations, 0,
        "reservation empty after modification render-fault rollback"
    );
    assert_eq!(
        invoices, 0,
        "invoice empty after modification render-fault rollback"
    );
    assert_eq!(
        lines, 0,
        "invoice_line empty after modification render-fault rollback"
    );
    assert_eq!(
        audit, 0,
        "audit_ledger empty after modification render-fault rollback — no committed chain-link without XML"
    );

    let ledger = Ledger::open(&path, tenant(), TEST_BINARY_HASH).expect("re-open ledger");
    assert_eq!(ledger.verify_chain().expect("verify_chain Ok"), 0);

    let _ = std::fs::remove_dir_all(&fault_dir);
    let _ = std::fs::remove_file(&path);
}

// ──────────────────────────────────────────────────────────────────────
// Test 2 — panic-injected mid-issuance
// ──────────────────────────────────────────────────────────────────────

#[test]
fn panic_between_appends_rolls_back_billing_and_audit() {
    let path = temp_db_path("panic");
    let (conn, series) = pre_tx_setup(&path);

    // Drive the same call sequence as `run_single_tx` under
    // `catch_unwind`, with a panic injected between the first and
    // second `append_in_tx`. The transaction must roll back even
    // though it never returned an `Err`.
    //
    // `AssertUnwindSafe` is correct here: we're not retaining any
    // mutated state across the unwind boundary; the Connection and
    // Transaction are constructed and dropped inside the closure.
    // `path` is intentionally NOT captured (it's used after
    // catch_unwind to inspect the post-state).
    let series_id = series.id;
    let result = std::panic::catch_unwind(AssertUnwindSafe(move || {
        let mut conn = conn;
        let tx = conn.transaction().expect("begin tx");
        let _outcome = billing::allocate_in_tx(
            &tx,
            build_allocate_args(series_id),
            OffsetDateTime::now_utc(),
        )
        .expect("allocate_in_tx Ok");
        audit_ledger::append_in_tx(
            &tx,
            &ledger_meta(),
            EventKind::InvoiceSequenceReserved,
            b"{\"test\":\"first\"}".to_vec(),
            Actor::test_only(),
            Some("idem-panic-1".to_string()),
        )
        .expect("append_in_tx first Ok");

        // ── Failure injection: panic between the two audit appends,
        //    AFTER allocate_in_tx has burned a number and the first
        //    audit entry has been written. The transaction has not
        //    committed; `Transaction::drop` runs during unwind.
        panic!("rollback conformance panic-injection");
    }));
    assert!(
        result.is_err(),
        "catch_unwind should observe the injected panic"
    );

    // After the unwind, every write the doomed transaction made must be
    // gone — both billing and audit halves. This is the contract PR-6
    // exists to defend.
    let (seq_state, reservations, invoices, lines, audit) = row_counts(&path);
    assert_eq!(
        seq_state, 0,
        "invoice_sequence_state should be empty after panic rollback"
    );
    assert_eq!(
        reservations, 0,
        "invoice_sequence_reservation should be empty after panic rollback"
    );
    assert_eq!(invoices, 0, "invoice should be empty after panic rollback");
    assert_eq!(
        lines, 0,
        "invoice_line should be empty after panic rollback"
    );
    assert_eq!(
        audit, 0,
        "audit_ledger should be empty after panic rollback"
    );

    let ledger = Ledger::open(&path, tenant(), TEST_BINARY_HASH).expect("re-open ledger");
    let verified = ledger.verify_chain().expect("verify_chain Ok");
    assert_eq!(verified, 0, "verify_chain should report 0 entries");

    let _ = std::fs::remove_file(&path);
}
