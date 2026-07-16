//! PR-82 — migration pin for the two buyer-facing note columns.
//!
//! Creates a DuckDB instance with the **pre-PR-82** schema (no
//! `invoice.invoice_note` column, no `invoice_line.note` column),
//! runs `ensure_schema()`, and asserts that the columns are added
//! idempotently AND that pre-PR-82 rows survive intact with NULL in
//! both new columns.
//!
//! Mirrors the PR-73a / PR-44γ migration-pin pattern: the migration
//! must hold against a fresh DB (CREATE TABLE wins) AND against an
//! old DB (ADD COLUMN IF NOT EXISTS wins) AND on re-run (idempotent).
//!
//! Why it's separated from the main `sequence_allocator.rs` test
//! suite: the migration boundary is the one place where the schema
//! shape is *observable* (every other test interacts only via the
//! `BillingStore` trait, which abstracts the columns). Keeping the
//! pin in its own file makes the regulatory boundary visible in the
//! test runner output and gives a future schema-changing PR a clean
//! place to extend without re-reading the unrelated allocator
//! invariants.

use aberp_billing::{
    AllocateArgs, AllocateOutcome, BillingStore, Currency, CustomerId, DraftInvoice,
    DuckDbBillingStore, Huf, IdempotencyKey, InvoiceId, InvoiceSeries, LineItem, ResetPolicy,
    SeriesCode, SeriesId,
};
use duckdb::Connection;
use time::OffsetDateTime;

/// Open a fresh in-process DuckDB and write the **pre-PR-82** schema
/// directly (lifted from `CREATE_TABLES_SQL` minus the two PR-82
/// columns plus the prior PR-73 / PR-44γ migration set already
/// applied — that's the schema state a tenant DB issued before PR-82
/// would carry on disk).
fn write_pre_pr82_schema(conn: &Connection) {
    conn.execute_batch(
        r#"
CREATE TABLE invoice_series (
    id           VARCHAR NOT NULL PRIMARY KEY,
    code         VARCHAR NOT NULL UNIQUE,
    reset_policy VARCHAR NOT NULL CHECK (reset_policy IN ('never','annual_on_fiscal_year')),
    fiscal_year  INTEGER,
    created_at   VARCHAR NOT NULL
);

CREATE TABLE invoice_sequence_state (
    series_id   VARCHAR NOT NULL,
    fiscal_year INTEGER NOT NULL,
    next_number BIGINT  NOT NULL CHECK (next_number >= 1),
    updated_at  VARCHAR NOT NULL,
    PRIMARY KEY (series_id, fiscal_year)
);

CREATE TABLE invoice_sequence_reservation (
    id          VARCHAR NOT NULL PRIMARY KEY,
    series_id   VARCHAR NOT NULL,
    fiscal_year INTEGER NOT NULL,
    number      BIGINT  NOT NULL,
    invoice_id  VARCHAR NOT NULL UNIQUE,
    status      VARCHAR NOT NULL CHECK (status IN ('reserved','used','voided')),
    void_reason VARCHAR,
    reserved_at VARCHAR NOT NULL,
    used_at     VARCHAR,
    voided_at   VARCHAR,
    UNIQUE (series_id, fiscal_year, number)
);

CREATE TABLE invoice (
    id              VARCHAR NOT NULL PRIMARY KEY,
    series_id       VARCHAR NOT NULL,
    customer_id     VARCHAR NOT NULL,
    issue_date      VARCHAR NOT NULL,
    sequence_number BIGINT  NOT NULL,
    fiscal_year     INTEGER NOT NULL,
    idempotency_key VARCHAR NOT NULL UNIQUE,
    currency             VARCHAR,
    exchange_rate        DECIMAL(18, 6),
    exchange_rate_source VARCHAR,
    exchange_rate_date   DATE,
    huf_equivalent_total DECIMAL(18, 0),
    bank_account_id        VARCHAR,
    bank_account_currency  VARCHAR,
    bank_account_number    VARCHAR,
    bank_account_bank_name VARCHAR,
    bank_account_swift_bic VARCHAR,
    UNIQUE (series_id, fiscal_year, sequence_number)
);

CREATE TABLE invoice_line (
    invoice_id            VARCHAR NOT NULL,
    ordinal               INTEGER NOT NULL,
    description           VARCHAR NOT NULL,
    quantity              INTEGER NOT NULL CHECK (quantity >= 0),
    unit_price            BIGINT  NOT NULL,
    vat_rate_basis_points INTEGER NOT NULL,
    PRIMARY KEY (invoice_id, ordinal)
);
        "#,
    )
    .expect("write pre-PR-82 schema");
}

/// Insert one invoice + one line under the pre-PR-82 schema so the
/// post-migration assertion can verify the row survived intact.
fn insert_pre_pr82_invoice(conn: &Connection) -> String {
    // Series first (FK by convention only; ADR-0019 — no real FK).
    let series_id = "srs_PR82MIGRATIONFIXTUREXXXXXX";
    conn.execute(
        "INSERT INTO invoice_series (id, code, reset_policy, fiscal_year, created_at)
         VALUES (?, 'PRE-PR-82', 'never', NULL, '2026-05-01T00:00:00Z');",
        [series_id],
    )
    .expect("seed series");
    let invoice_id = "inv_PR82MIGRATIONFIXTUREXXXXXX";
    conn.execute(
        "INSERT INTO invoice
         (id, series_id, customer_id, issue_date, sequence_number, fiscal_year,
          idempotency_key, currency, exchange_rate, exchange_rate_source,
          exchange_rate_date, huf_equivalent_total,
          bank_account_id, bank_account_currency, bank_account_number,
          bank_account_bank_name, bank_account_swift_bic)
         VALUES (?, ?, 'cus_FIXTUREXXXXXXXXXXXXXXXXX',
                 '2026-05-01T00:00:00Z', 1, 0,
                 'idem_PR82MIGRATIONXXXXXXXXXXXX',
                 'HUF', NULL, NULL, NULL, NULL,
                 NULL, NULL, NULL, NULL, NULL);",
        [invoice_id, series_id],
    )
    .expect("seed invoice");
    conn.execute(
        "INSERT INTO invoice_line
         (invoice_id, ordinal, description, quantity, unit_price, vat_rate_basis_points)
         VALUES (?, 0, 'pre-pr82-line', 1, 1000, 2700);",
        [invoice_id],
    )
    .expect("seed line");
    invoice_id.to_string()
}

/// PR-82 — old-schema DB gains `invoice.invoice_note` +
/// `invoice_line.note` after `ensure_schema()`, and the pre-PR-82
/// row's data is unchanged (with NULL in both new columns).
#[test]
fn pre_pr82_schema_gains_note_columns_after_ensure_schema() {
    let store = DuckDbBillingStore::open_in_memory().expect("open in-memory DB");
    // Reach for the inner connection to write the pre-PR-82 schema.
    let conn = store.into_connection();
    write_pre_pr82_schema(&conn);
    let pre_invoice_id = insert_pre_pr82_invoice(&conn);

    // Verify the columns do NOT exist yet (sanity check on the
    // fixture).
    let probe = conn
        .prepare("SELECT invoice_note FROM invoice WHERE id = ?;")
        .map(|_| true)
        .unwrap_or(false);
    assert!(
        !probe,
        "fixture invariant: invoice.invoice_note must NOT exist before ensure_schema()"
    );

    // Now run `ensure_schema()` via the store, the same boot-time
    // entry point `serve.rs` and `issue_invoice.rs` use.
    let mut store = DuckDbBillingStore::from_connection(conn);
    store
        .ensure_schema()
        .expect("ensure_schema() must add the PR-82 columns idempotently");
    let conn = store.into_connection();

    // Column-presence pin — the columns now exist (the SELECT
    // succeeds).
    let invoice_note: Option<String> = conn
        .prepare("SELECT invoice_note FROM invoice WHERE id = ?;")
        .expect("invoice.invoice_note column exists post-migration")
        .query_row([&pre_invoice_id], |r| r.get::<_, Option<String>>(0))
        .expect("read invoice_note");
    assert!(
        invoice_note.is_none(),
        "pre-PR-82 row must keep invoice_note NULL after migration (no backfill); got {invoice_note:?}"
    );

    let line_note: Option<String> = conn
        .prepare("SELECT note FROM invoice_line WHERE invoice_id = ? AND ordinal = 0;")
        .expect("invoice_line.note column exists post-migration")
        .query_row([&pre_invoice_id], |r| r.get::<_, Option<String>>(0))
        .expect("read line note");
    assert!(
        line_note.is_none(),
        "pre-PR-82 line must keep note NULL after migration; got {line_note:?}"
    );

    // Original line data must survive verbatim — the migration is
    // additive only.
    let description: String = conn
        .prepare("SELECT description FROM invoice_line WHERE invoice_id = ? AND ordinal = 0;")
        .expect("prepare description select")
        .query_row([&pre_invoice_id], |r| r.get::<_, String>(0))
        .expect("read description");
    assert_eq!(description, "pre-pr82-line");
}

/// PR-82 — `ensure_schema()` is idempotent. Running it twice in a row
/// must not fail (the `ADD COLUMN IF NOT EXISTS` posture mirrors
/// PR-44γ / PR-73 — same trap, same guard).
#[test]
fn ensure_schema_is_idempotent_against_post_pr82_db() {
    let mut store = DuckDbBillingStore::open_in_memory().expect("open in-memory DB");
    store.ensure_schema().expect("first ensure_schema");
    // Re-run — must be a no-op, not an error.
    store
        .ensure_schema()
        .expect("second ensure_schema must succeed (idempotent)");
    drop(store);
}

/// PR-82 — the round-trip path persists `LineItem.note` through
/// `allocate_in_tx` and reads it back via `load_ready_invoice_by_id`.
/// Belt-and-braces against a schema/SELECT drift where the migration
/// adds the column but the read path forgets to consume it.
#[test]
fn line_note_round_trips_through_allocate_and_load() {
    let mut store = DuckDbBillingStore::open_in_memory().expect("open in-memory DB");
    store.ensure_schema().expect("ensure_schema");

    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: SeriesCode::new("INV-PR82".to_string()).unwrap(),
        reset_policy: ResetPolicy::Never,
        fiscal_year: None,
        created_at: OffsetDateTime::now_utc(),
    };
    store.create_series(&series).expect("create series");

    let invoice_id = InvoiceId::new();
    let now = OffsetDateTime::now_utc();
    let draft = DraftInvoice {
        id: invoice_id,
        series_id: series.id,
        customer_id: CustomerId::new(),
        lines: vec![LineItem {
            description: "annotated-line".to_string(),
            quantity: rust_decimal::Decimal::from(3),
            unit_price: Huf(1_500),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: Some("Please ship to dock B".to_string()),
            unit: None,
        }],
        issue_date: now,
        // PR-84 — round-trip pin defaults both dates to the issue date.
        payment_deadline: now.date(),
        delivery_date: now.date(),
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
                invoice_note: Some("PR-82 round-trip global note".to_string()),
                email_recipient_override: None,
                start_value: 1,
                sequence_floor: None,
            },
            OffsetDateTime::now_utc(),
        )
        .expect("allocate_and_insert");

    let invoice = match outcome {
        AllocateOutcome::Fresh { invoice, .. } => invoice,
        AllocateOutcome::Replay { .. } => panic!("fresh-issue expected"),
    };

    // Reach back into the underlying connection to load the row in
    // a tx (the public `load_ready_invoice_by_id` helper needs one).
    let mut conn = store.into_connection();
    let tx = conn.transaction().expect("open read tx");
    let loaded = aberp_billing::load_ready_invoice_by_id(&tx, &invoice.id.to_prefixed_string())
        .expect("load_ready_invoice_by_id")
        .expect("invoice row");
    assert_eq!(
        loaded.0.lines[0].note.as_deref(),
        Some("Please ship to dock B"),
        "PR-82 line note must round-trip through DuckDB"
    );
    // Invoice-level note via the dedicated helper.
    let stored_invoice_note =
        aberp_billing::load_invoice_note_in_tx(&tx, &invoice.id.to_prefixed_string())
            .expect("load_invoice_note_in_tx");
    assert_eq!(
        stored_invoice_note.as_deref(),
        Some("PR-82 round-trip global note"),
        "PR-82 invoice note must round-trip through DuckDB"
    );
    tx.commit().expect("commit read tx");
}
