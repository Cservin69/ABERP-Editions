//! PR-73a / hotfix — pin that `DuckDbBillingStore::ensure_schema`
//! upgrades a pre-PR-73 `invoice` table by ADDing the five
//! `bank_account_*` columns introduced by `MIGRATE_PR_73_SQL`.
//!
//! The regression this pins: PR-73 added the migration constant + ran
//! it inside `ensure_schema`, but the `serve` binary never called
//! `ensure_schema` at boot. Operators with a pre-PR-73 `aberp.duckdb`
//! hit a 500 on the first `list_invoices` request because
//! `load_invoice_bank_snapshot_in_tx` referenced columns that did not
//! yet exist on the row. The fix (`serve::run` now opens the store +
//! calls `ensure_schema` as a boot step) relies on this migration
//! being idempotent on an OLD schema — not just on a fresh one.

use duckdb::Connection;

use aberp_billing::{BillingStore, DuckDbBillingStore};

/// Create a minimal pre-PR-73 `invoice` table — the six columns that
/// existed before PR-44γ added currency metadata. `ensure_schema`
/// will be a no-op for the `CREATE TABLE IF NOT EXISTS` line and
/// must then layer both `MIGRATE_PR_44C_SQL` and `MIGRATE_PR_73_SQL`
/// on top via `ADD COLUMN IF NOT EXISTS`.
const OLD_INVOICE_TABLE_SQL: &str = "
CREATE TABLE invoice (
    id              VARCHAR PRIMARY KEY,
    series_id       VARCHAR,
    customer_id     VARCHAR,
    sequence_number BIGINT,
    fiscal_year     INTEGER,
    issue_date      DATE
);
";

const PR73_COLUMNS: &[&str] = &[
    "bank_account_id",
    "bank_account_currency",
    "bank_account_number",
    "bank_account_bank_name",
    "bank_account_swift_bic",
];

#[test]
fn ensure_schema_adds_pr73_bank_account_columns_to_pre_pr73_invoice_table() {
    let conn = Connection::open_in_memory().expect("open in-memory duckdb");
    conn.execute_batch(OLD_INVOICE_TABLE_SQL)
        .expect("seed pre-PR-73 invoice table");

    // Sanity: the bank_account_* columns must NOT exist on the seeded
    // schema — otherwise the pin can pass for the wrong reason.
    let seeded_columns = invoice_column_names(&conn);
    for col in PR73_COLUMNS {
        assert!(
            !seeded_columns.contains(&col.to_string()),
            "pre-condition: pre-PR-73 invoice table must not yet have column `{col}`, got {seeded_columns:?}"
        );
    }

    let mut store = DuckDbBillingStore::from_connection(conn);
    store
        .ensure_schema()
        .expect("ensure_schema on pre-PR-73 DB must succeed via ADD COLUMN IF NOT EXISTS");

    let conn = store.into_connection();
    let post_columns = invoice_column_names(&conn);
    for col in PR73_COLUMNS {
        assert!(
            post_columns.contains(&col.to_string()),
            "post-condition: ensure_schema must add PR-73 column `{col}`, got {post_columns:?}"
        );
    }
}

#[test]
fn ensure_schema_is_idempotent_when_pr73_columns_already_present() {
    // Fresh DB: ensure_schema runs the create + both migrations on a
    // clean slate. Calling it a SECOND time must be a no-op (the
    // `ADD COLUMN IF NOT EXISTS` ladder is the load-bearing piece on
    // every boot after the first).
    let mut store = DuckDbBillingStore::open_in_memory().expect("open in-memory store");
    store.ensure_schema().expect("first ensure_schema");
    store
        .ensure_schema()
        .expect("second ensure_schema must be idempotent on already-migrated DB");

    let conn = store.into_connection();
    let columns = invoice_column_names(&conn);
    for col in PR73_COLUMNS {
        assert!(
            columns.contains(&col.to_string()),
            "PR-73 column `{col}` must still be present after idempotent re-run, got {columns:?}"
        );
    }
}

fn invoice_column_names(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'invoice'",
        )
        .expect("prepare information_schema lookup");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query column names");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect column names")
}
