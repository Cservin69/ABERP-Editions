//! `quote_intake_log` — staging table for fetched approved quotes.
//!
//! No CHECK constraints (per [[no-sql-specific]]); the `PRIMARY KEY`
//! on `quote_id` is the idempotency anchor.
//!
//! Dev nuke recipe: `DROP TABLE quote_intake_log;` — see crate
//! README. NEVER on prod (loses operator pickup queue).

use duckdb::{params, Connection};
use time::OffsetDateTime;

use crate::error::QuoteIntakeError;

pub fn ensure_schema(conn: &Connection) -> Result<(), QuoteIntakeError> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| QuoteIntakeError::Storage(format!("ensure quote_intake_log schema: {e}")))?;
    // S255 / PR-244 — additive migration for the operator-pickup
    // landing column. Idempotent on a post-S255 boot; fills pre-S255
    // rows with NULL (operator never picked them up — equivalent to
    // the post-S255 "fresh row" state).
    conn.execute_batch(S255_MIGRATION_SQL).map_err(|e| {
        QuoteIntakeError::Storage(format!("apply S255 quote_intake_log migration: {e}"))
    })?;
    // S256 / PR-245 — additive `intake_state` + `intake_error` columns.
    // A malformed quote (mapping failure) is now staged as an
    // `error`-state row instead of being silently dropped (brief §A.4),
    // so the operator sees it in the Quotes tab and can retry-parse or
    // mark-irrelevant. Closed vocab is enforced in the app layer (per
    // [[no-sql-specific]]); the DEFAULT backfills pre-S256 rows to
    // `staged` (every prior row was a successful stage).
    conn.execute_batch(S256_MIGRATION_SQL).map_err(|e| {
        QuoteIntakeError::Storage(format!("apply S256 quote_intake_log migration: {e}"))
    })?;
    // S271 / PR-260 — additive auto-quoting projection columns. The
    // storefront's pipeline (separate SvelteKit repo, abenerp.com) pushes
    // these per quote as its state machine advances; ABERP-side they are
    // strictly READ-ONLY (this PR ships the schema + the
    // `stock_alert` recompute; the storefront PR that POPULATES the
    // values is tracked separately). All new columns default to NULL
    // except `stock_alert` (closed-vocab boolean, FALSE by default per
    // EVE addendum 2's stale-stock-banner spec).
    conn.execute_batch(S271_MIGRATION_SQL).map_err(|e| {
        QuoteIntakeError::Storage(format!("apply S271 quote_intake_log migration: {e}"))
    })?;
    // S272 / PR-261 — additive DEAL-saga columns. The DEAL saga writes
    // all four in a single tx (deal_issued_at + deal_sales_order_id +
    // deal_work_order_id + refresh_acked_at). All four are nullable; the
    // single-use invariant lives in the app layer (CAS on
    // `deal_issued_at IS NULL`), per [[no-sql-specific]]. No SQL DEFAULT
    // for the same DuckDB-clobber reason pinned in S271.
    conn.execute_batch(S272_MIGRATION_SQL).map_err(|e| {
        QuoteIntakeError::Storage(format!("apply S272 quote_intake_log migration: {e}"))
    })
}

/// Closed-vocab `intake_state` values. NOT enforced by a DuckDB CHECK
/// (per [[no-sql-specific]]); these constants are the single source of
/// truth the app-layer writers use.
pub const STATE_STAGED: &str = "staged";
pub const STATE_ERROR: &str = "error";
pub const STATE_IRRELEVANT: &str = "irrelevant";

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quote_intake_log (
    quote_id              VARCHAR NOT NULL PRIMARY KEY,
    tenant_id             VARCHAR NOT NULL,
    invoice_id            VARCHAR NOT NULL,
    received_at           VARCHAR NOT NULL,
    intake_at             VARCHAR NOT NULL,
    status_writeback_at   VARCHAR,
    raw_payload           VARCHAR NOT NULL,
    prepared_draft        VARCHAR NOT NULL
);
CREATE INDEX IF NOT EXISTS quote_intake_log_pending_writeback_idx
    ON quote_intake_log (tenant_id, status_writeback_at);
";

/// S255 / PR-244 — `picked_up_drf_id` records the `drf_<ULID>` of the
/// invoice_draft minted when the operator clicked "Create draft
/// invoice" on this quote. NULL means "operator has not picked up
/// this quote yet" — the SPA renders the pickup button; a non-NULL
/// renders the "→ Draft #N" link instead. A re-pickup after S239
/// deletes the prior draft is allowed: the route writes the new
/// `drf_<ULID>` here, overwriting the now-orphaned id. (Idempotency
/// within a single pickup attempt rides on the audit-ledger F8 gate;
/// this column is the operator-facing tag, not the dedup key.)
const S255_MIGRATION_SQL: &str = "
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS picked_up_drf_id VARCHAR;
";

/// S256 / PR-245 — `intake_state` (closed vocab: `staged` / `error` /
/// `irrelevant`) + `intake_error` (operator-readable message for
/// `error`-state rows). The `DEFAULT 'staged'` backfills every pre-S256
/// row, all of which were successful stages.
const S256_MIGRATION_SQL: &str = "
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS intake_state VARCHAR DEFAULT 'staged';
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS intake_error VARCHAR;
";

/// S271 / PR-260 — auto-quoting projection columns. Populated by the
/// storefront pipeline (separate repo) when a quote transitions
/// `priced → accepted`; ABERP-side this PR ships the schema + the
/// app-layer `stock_alert` recompute. All seven columns are additive +
/// nullable. NONE carries a SQL `DEFAULT`.
///
/// **DuckDB gotcha pinned by S271 testing** (worth a `[[no-sql-specific]]`
/// memory): `ALTER TABLE ... ADD COLUMN IF NOT EXISTS col TYPE DEFAULT V`
/// **silently re-applies the DEFAULT V on every replay of the
/// statement**. The `IF NOT EXISTS` correctly guards the column-add,
/// but DuckDB then re-applies the default to existing rows, clobbering
/// any data the app has written since the first migration run. Since
/// `ensure_schema` is called at the top of EVERY writer in this file,
/// a DEFAULT-bearing column would be reset to its default on every
/// `set_*` / `mark_*` call against any other row in the table. We
/// therefore omit DEFAULTs entirely; `stock_alert` is a nullable
/// `BOOLEAN` and the app layer coerces NULL → `false` on read
/// (`coerce_stock_alert` in `quote_stock_alert.rs`). New writes use
/// the explicit `flip_stock_alert_to_true` setter; INSERTs leave the
/// column NULL until the first recompute pass writes TRUE.
///
/// Trade-off flagged in the PR body: until the storefront PR ships,
/// every column is NULL on every existing row and the `stock_alert`
/// recompute is a no-op (no snapshot to compare against). The schema
/// scaffolding lands first so the storefront PR is purely a producer.
const S271_MIGRATION_SQL: &str = "
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS customer_email VARCHAR;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS material_grade VARCHAR;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS quantity INTEGER;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS total_price_eur DOUBLE;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS valid_until DATE;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS stock_status_at_accept VARCHAR;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS stock_alert BOOLEAN;
";

/// S272 / PR-261 — DEAL-saga additive columns. Per ADR-0067 the saga is
/// a single DB transaction that writes all four in one go and emits
/// three audit entries (`QuoteDealIssued`, `QuoteSalesOrderCreated`,
/// `QuoteWorkOrderCreated`).
///
/// - `deal_issued_at TIMESTAMP` — set to `now_utc()` when the saga
///   commits. The single-use invariant rides this column: a `WHERE
///   deal_issued_at IS NULL` CAS ensures replay returns 409
///   `deal_already_issued` per EVE addendum 3.
/// - `deal_sales_order_id VARCHAR` — `so_<ULID>` placeholder. The full
///   SO module is not built yet (named-deferred — see brief pushback
///   #1); this PR mints the id + emits the audit so the future SO
///   module's backfill can adopt these rows.
/// - `deal_work_order_id VARCHAR` — `wo_<ULID>` placeholder. The
///   PR-228 `aberp-work-orders` crate requires `product_id` + at least
///   one routing op, neither of which lives on the quote intake row
///   yet. This PR mints a placeholder + emits the audit; the real WO
///   wire-up follows when the auto-quoting engine plumbs through
///   product+routing.
/// - `refresh_acked_at TIMESTAMP` — set to `now_utc()` ONLY when the
///   saga path consumed an operator REFRESH token (EVE addendum 2 UI
///   half). Stays NULL on a no-stock-alert saga so the audit-walk can
///   prove whether the operator acknowledged the stock change.
///
/// NO SQL DEFAULTs — DuckDB's `ALTER ... ADD COLUMN IF NOT EXISTS ...
/// DEFAULT V` re-applies the default on every replay, which would
/// clobber the single-use marker. Same trap as S271's `stock_alert`.
const S272_MIGRATION_SQL: &str = "
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS deal_issued_at TIMESTAMP;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS deal_sales_order_id VARCHAR;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS deal_work_order_id VARCHAR;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS refresh_acked_at TIMESTAMP;
";

pub fn already_intook(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
) -> Result<Option<String>, QuoteIntakeError> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare("SELECT invoice_id FROM quote_intake_log WHERE quote_id = ?1 AND tenant_id = ?2")
        .map_err(|e| QuoteIntakeError::Storage(format!("prepare already_intook: {e}")))?;
    let mut rows = stmt
        .query(params![quote_id, tenant_id])
        .map_err(|e| QuoteIntakeError::Storage(format!("query already_intook: {e}")))?;
    if let Some(row) = rows
        .next()
        .map_err(|e| QuoteIntakeError::Storage(format!("read already_intook row: {e}")))?
    {
        let invoice_id: String = row
            .get(0)
            .map_err(|e| QuoteIntakeError::Storage(format!("get invoice_id col: {e}")))?;
        Ok(Some(invoice_id))
    } else {
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn insert_intake(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
    invoice_id: &str,
    received_at: &str,
    intake_at: OffsetDateTime,
    raw_payload_json: &str,
    prepared_draft_json: &str,
) -> Result<(), QuoteIntakeError> {
    ensure_schema(conn)?;
    let intake_at_iso = format_iso(intake_at)?;
    conn.execute(
        "INSERT INTO quote_intake_log (
             quote_id, tenant_id, invoice_id,
             received_at, intake_at,
             status_writeback_at,
             raw_payload, prepared_draft,
             intake_state, intake_error
         ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, NULL)",
        params![
            quote_id,
            tenant_id,
            invoice_id,
            received_at,
            intake_at_iso,
            raw_payload_json,
            prepared_draft_json,
            STATE_STAGED,
        ],
    )
    .map_err(|e| QuoteIntakeError::Storage(format!("insert quote_intake_log row: {e}")))?;
    Ok(())
}

/// S256 / PR-245 — stage a quote whose mapping FAILED as an
/// `error`-state row instead of silently dropping it (brief §A.4). The
/// raw payload is preserved verbatim so the operator's retry-parse can
/// re-run the mapping against it; `invoice_id` and `prepared_draft` are
/// placeholders until a successful retry fills them via
/// [`retry_parse_intake`]. Idempotency rides the `quote_id` PRIMARY KEY:
/// a second poll cycle's `already_intook` check sees the error row and
/// skips re-insert.
pub fn insert_error_intake(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
    received_at: &str,
    intake_at: OffsetDateTime,
    raw_payload_json: &str,
    error_message: &str,
) -> Result<(), QuoteIntakeError> {
    ensure_schema(conn)?;
    let intake_at_iso = format_iso(intake_at)?;
    conn.execute(
        "INSERT INTO quote_intake_log (
             quote_id, tenant_id, invoice_id,
             received_at, intake_at,
             status_writeback_at,
             raw_payload, prepared_draft,
             intake_state, intake_error
         ) VALUES (?1, ?2, '', ?3, ?4, NULL, ?5, '{}', ?6, ?7)",
        params![
            quote_id,
            tenant_id,
            received_at,
            intake_at_iso,
            raw_payload_json,
            STATE_ERROR,
            error_message,
        ],
    )
    .map_err(|e| QuoteIntakeError::Storage(format!("insert error quote_intake_log row: {e}")))?;
    Ok(())
}

/// S256 / PR-245 — recovery path for an `error`-state row: a successful
/// re-parse fills `invoice_id` + `prepared_draft` and flips the row back
/// to `staged`, clearing `intake_error`. Guarded on
/// `intake_state = 'error'` so it never clobbers a successfully-staged
/// or picked-up row. Returns the number of rows updated (0 = no matching
/// error row, which the route maps to 404 / no-op).
pub fn retry_parse_intake(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
    invoice_id: &str,
    prepared_draft_json: &str,
) -> Result<usize, QuoteIntakeError> {
    ensure_schema(conn)?;
    let n = conn
        .execute(
            "UPDATE quote_intake_log
                SET invoice_id = ?1,
                    prepared_draft = ?2,
                    intake_state = ?3,
                    intake_error = NULL
              WHERE quote_id = ?4 AND tenant_id = ?5 AND intake_state = ?6",
            params![
                invoice_id,
                prepared_draft_json,
                STATE_STAGED,
                quote_id,
                tenant_id,
                STATE_ERROR,
            ],
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("retry-parse update: {e}")))?;
    Ok(n)
}

/// S256 / PR-245 — operator dismisses a row (typically a dead-letter
/// `error` row that will never parse, e.g. a quote the storefront sent
/// malformed). Flips the row to `irrelevant`; it then drops out of the
/// badge count and the pickup surface. Returns rows updated.
pub fn mark_irrelevant(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
) -> Result<usize, QuoteIntakeError> {
    ensure_schema(conn)?;
    let n = conn
        .execute(
            "UPDATE quote_intake_log
                SET intake_state = ?1
              WHERE quote_id = ?2 AND tenant_id = ?3",
            params![STATE_IRRELEVANT, quote_id, tenant_id],
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("mark-irrelevant update: {e}")))?;
    Ok(n)
}

/// S271 / PR-260 — persist a `stock_alert = TRUE` flip on a quote row.
/// Returns `true` iff THIS call performed the transition (the row's
/// stored value was FALSE or NULL before this call and is TRUE after).
/// A repeat call on an already-flipped row returns `false` (sticky); a
/// call against a non-existent row also returns `false`.
///
/// The audit-emit caller (the SPA list route in `serve.rs`) uses the
/// returned bool as its only-once trigger: exactly one
/// `QuoteStockAlertTriggered` audit entry per row that newly transitions
/// to TRUE.
///
/// Why read-then-write instead of `UPDATE ... WHERE`: DuckDB's UPDATE
/// rowcount reflects the predicate-matched count without surfacing
/// whether the SET actually altered the column — a guarded UPDATE on a
/// no-op row still reports `1`. A separate SELECT pin makes the
/// transition observable in app code without depending on
/// rows-affected semantics, per [[no-sql-specific]].
pub fn flip_stock_alert_to_true(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
) -> Result<bool, QuoteIntakeError> {
    ensure_schema(conn)?;
    let stored: Option<Option<bool>> = conn
        .query_row(
            "SELECT stock_alert FROM quote_intake_log
              WHERE quote_id = ?1 AND tenant_id = ?2",
            params![quote_id, tenant_id],
            |r| r.get::<_, Option<bool>>(0),
        )
        .map(Some)
        .or_else(|e| match e {
            duckdb::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|e| QuoteIntakeError::Storage(format!("read stock_alert: {e}")))?;
    let Some(stored) = stored else {
        return Ok(false); // no matching row → nothing to flip
    };
    if stored.unwrap_or(false) {
        return Ok(false); // sticky
    }
    conn.execute(
        "UPDATE quote_intake_log
            SET stock_alert = TRUE
          WHERE quote_id = ?1 AND tenant_id = ?2",
        params![quote_id, tenant_id],
    )
    .map_err(|e| QuoteIntakeError::Storage(format!("flip stock_alert to TRUE: {e}")))?;
    Ok(true)
}

/// S256 / PR-245 — the SPA sidebar/tab badge count: un-picked-up quotes
/// that are still actionable (`staged`, not yet picked up). `error` and
/// `irrelevant` rows are excluded — an error row isn't pickable, and an
/// irrelevant row was dismissed. Recomputed from DB on every call so the
/// badge survives an app restart (adversarial-review note: don't trust
/// an in-memory counter).
pub fn count_unpicked(conn: &Connection, tenant_id: &str) -> Result<u64, QuoteIntakeError> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT count(*) FROM quote_intake_log
              WHERE tenant_id = ?1
                AND intake_state = ?2
                AND picked_up_drf_id IS NULL",
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("prepare count_unpicked: {e}")))?;
    let n: i64 = stmt
        .query_row(params![tenant_id, STATE_STAGED], |row| row.get(0))
        .map_err(|e| QuoteIntakeError::Storage(format!("query count_unpicked: {e}")))?;
    Ok(n.max(0) as u64)
}

/// S256 / PR-245 — count of `error`-state rows for a tenant (surfaced
/// to the SPA so the operator knows there are dead-letter rows to
/// triage even when none are pickable).
pub fn count_errored(conn: &Connection, tenant_id: &str) -> Result<u64, QuoteIntakeError> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT count(*) FROM quote_intake_log
              WHERE tenant_id = ?1 AND intake_state = ?2",
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("prepare count_errored: {e}")))?;
    let n: i64 = stmt
        .query_row(params![tenant_id, STATE_ERROR], |row| row.get(0))
        .map_err(|e| QuoteIntakeError::Storage(format!("query count_errored: {e}")))?;
    Ok(n.max(0) as u64)
}

/// S256 / PR-245 — the set of `quote_id`s that are currently staged AND
/// un-picked-up. The notifications route intersects this with the
/// `QuoteIntakeRowAdded` audit entries past the catch-up boundary to
/// compute live toast arrivals (belt-and-suspenders cross-check so an
/// already-picked-up quote never replays a toast — brief §B.8).
pub fn list_unpicked_quote_ids(
    conn: &Connection,
    tenant_id: &str,
) -> Result<Vec<String>, QuoteIntakeError> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT quote_id FROM quote_intake_log
              WHERE tenant_id = ?1
                AND intake_state = ?2
                AND picked_up_drf_id IS NULL",
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("prepare list_unpicked_quote_ids: {e}")))?;
    let mut rows = stmt
        .query(params![tenant_id, STATE_STAGED])
        .map_err(|e| QuoteIntakeError::Storage(format!("query list_unpicked_quote_ids: {e}")))?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| QuoteIntakeError::Storage(format!("read unpicked row: {e}")))?
    {
        out.push(
            row.get::<_, String>(0)
                .map_err(|e| QuoteIntakeError::Storage(format!("get quote_id: {e}")))?,
        );
    }
    Ok(out)
}

/// S256 / PR-245 — read the stored raw payload + current state for a
/// quote (used by the retry-parse route to re-run the mapping against
/// the verbatim stored payload). `Ok(None)` when no row matches.
pub fn read_raw_and_state(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
) -> Result<Option<(String, String)>, QuoteIntakeError> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT raw_payload, COALESCE(intake_state, ?3)
               FROM quote_intake_log
              WHERE quote_id = ?1 AND tenant_id = ?2
              LIMIT 1",
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("prepare read_raw_and_state: {e}")))?;
    let mut rows = stmt
        .query(params![quote_id, tenant_id, STATE_STAGED])
        .map_err(|e| QuoteIntakeError::Storage(format!("query read_raw_and_state: {e}")))?;
    let Some(row) = rows
        .next()
        .map_err(|e| QuoteIntakeError::Storage(format!("read read_raw_and_state row: {e}")))?
    else {
        return Ok(None);
    };
    let raw: String = row
        .get(0)
        .map_err(|e| QuoteIntakeError::Storage(format!("get raw_payload: {e}")))?;
    let state: String = row
        .get(1)
        .map_err(|e| QuoteIntakeError::Storage(format!("get intake_state: {e}")))?;
    Ok(Some((raw, state)))
}

pub fn mark_writeback_complete(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
    when: OffsetDateTime,
) -> Result<(), QuoteIntakeError> {
    ensure_schema(conn)?;
    let when_iso = format_iso(when)?;
    conn.execute(
        "UPDATE quote_intake_log
            SET status_writeback_at = ?1
          WHERE quote_id = ?2 AND tenant_id = ?3",
        params![when_iso, quote_id, tenant_id],
    )
    .map_err(|e| QuoteIntakeError::Storage(format!("update writeback timestamp: {e}")))?;
    Ok(())
}

/// S255 / PR-244 — fetch the raw row needed by the operator-pickup
/// route: the prepared-draft JSON, the contact slice (for the SPA's
/// "creating new partner" confirm modal copy), and the existing
/// `picked_up_drf_id` (which the route's idempotency walk reads).
///
/// Returns `Ok(None)` if no row matches the `(tenant, quote_id)` —
/// the route maps this to 404.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickupSourceRow {
    pub raw_payload: String,
    pub prepared_draft: String,
    pub picked_up_drf_id: Option<String>,
}

pub fn read_for_pickup(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
) -> Result<Option<PickupSourceRow>, QuoteIntakeError> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT raw_payload, prepared_draft, picked_up_drf_id
               FROM quote_intake_log
              WHERE quote_id = ?1 AND tenant_id = ?2
              LIMIT 1",
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("prepare read_for_pickup: {e}")))?;
    let mut rows = stmt
        .query(params![quote_id, tenant_id])
        .map_err(|e| QuoteIntakeError::Storage(format!("query read_for_pickup: {e}")))?;
    let Some(row) = rows
        .next()
        .map_err(|e| QuoteIntakeError::Storage(format!("read read_for_pickup row: {e}")))?
    else {
        return Ok(None);
    };
    let raw_payload: String = row
        .get(0)
        .map_err(|e| QuoteIntakeError::Storage(format!("get raw_payload col: {e}")))?;
    let prepared_draft: String = row
        .get(1)
        .map_err(|e| QuoteIntakeError::Storage(format!("get prepared_draft col: {e}")))?;
    let picked_up_drf_id: Option<String> = row
        .get(2)
        .map_err(|e| QuoteIntakeError::Storage(format!("get picked_up_drf_id col: {e}")))?;
    Ok(Some(PickupSourceRow {
        raw_payload,
        prepared_draft,
        picked_up_drf_id,
    }))
}

/// S255 / PR-244 — record the operator-minted `drf_<ULID>` on the
/// quote_intake_log row. Overwrites any prior value: a re-pickup
/// after S239 delete is intentional and the column tracks the LATEST
/// pickup, not the historical pickups (the audit ledger does that).
pub fn set_picked_up_drf_id(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
    drf_id: &str,
) -> Result<(), QuoteIntakeError> {
    ensure_schema(conn)?;
    conn.execute(
        "UPDATE quote_intake_log
            SET picked_up_drf_id = ?1
          WHERE quote_id = ?2 AND tenant_id = ?3",
        params![drf_id, quote_id, tenant_id],
    )
    .map_err(|e| QuoteIntakeError::Storage(format!("update picked_up_drf_id: {e}")))?;
    Ok(())
}

/// S264 / PR-253 (F4) — atomically CLAIM a quote for pickup. This is the
/// compare-and-swap that makes a concurrent double-pickup impossible:
/// `picked_up_drf_id` is set to `drf_id` ONLY when its current value
/// still equals `expected_prior` (the value the caller read before it
/// minted its draft). Returns the rows updated:
///   - `1` — claim won; the column now points at this pickup's draft.
///   - `0` — another pickup changed the column since the caller's read
///           (it won the race); the caller MUST roll back its freshly-
///           minted draft + audit and return the winner's draft instead.
///
/// `IS NOT DISTINCT FROM` is NULL-safe equality, so a first-time pickup
/// (`expected_prior = None`) claims `WHERE picked_up_drf_id IS NULL`, and
/// a re-pickup after an S239 delete (`expected_prior = Some(old_drf)`)
/// claims `WHERE picked_up_drf_id = old_drf` — the legitimate overwrite
/// still works, but ONLY if no concurrent pickup moved the column first.
///
/// This is the guard the route comments USED to claim the audit-ledger
/// "F8 pin" provided — it did not (the ledger has no UNIQUE on
/// `idempotency_key`). Per [[no-sql-specific]] the serialization
/// invariant lives here in the app layer; the `quote_id` PRIMARY KEY is
/// the single row the CAS contends on. A portable backend lacking
/// `IS NOT DISTINCT FROM` spells the predicate
/// `(picked_up_drf_id = ?4 OR (picked_up_drf_id IS NULL AND ?4 IS NULL))`.
pub fn claim_for_pickup_in_tx(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
    drf_id: &str,
    expected_prior: Option<&str>,
) -> Result<usize, QuoteIntakeError> {
    let n = conn
        .execute(
            "UPDATE quote_intake_log
                SET picked_up_drf_id = ?1
              WHERE quote_id = ?2 AND tenant_id = ?3
                AND picked_up_drf_id IS NOT DISTINCT FROM ?4",
            params![drf_id, quote_id, tenant_id, expected_prior],
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("CAS claim picked_up_drf_id: {e}")))?;
    Ok(n)
}

/// S272 / PR-261 — what the DEAL saga reads off the row before deciding
/// whether to mint SO/WO ids. Carries every column the saga's
/// precondition checks consult AND the row's intake state (so the saga
/// can refuse to deal an `error`-state or `irrelevant`-state row). Per
/// `[[no-sql-specific]]` the closed-vocab `intake_state` is enforced in
/// the app layer; the row's stored `intake_state` may be NULL on a
/// pre-S256 install, so the helper coerces NULL → `STATE_STAGED`.
///
/// S273 / PR-262 widens the row with `material_grade` + `quantity` so
/// the saga can hand them to `apps/aberp/src/material_inventory.rs` for
/// the in-tx `committed_qty +=` write. Both columns are nullable in the
/// schema (S271 added them without DEFAULTs); a `None` here means the
/// row pre-dates the storefront's auto-quoting projection write, in
/// which case the material-commit branch of the saga is skipped (per
/// ADR-0069 / brief pushback — graceful fallback so all S272-era rows
/// still deal; no material commit happens until the storefront pipeline
/// populates the columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DealSourceRow {
    pub intake_state: String,
    pub stock_alert: bool,
    pub deal_issued_at: Option<String>,
    /// S273 — material grade the quote was priced against (S271 column).
    /// `None` on pre-storefront rows; the saga skips the material commit.
    pub material_grade: Option<String>,
    /// S273 — quantity requested (S271 column). The saga uses this as
    /// the `inventory_reservations.qty` value for v1 (units, NOT volume —
    /// the units→mm³→kg conversion needs the CAD-extract `FeatureGraph`
    /// volume + the catalogue density, neither of which is plumbed yet).
    /// `None` on pre-storefront rows.
    pub quantity: Option<i64>,
}

pub fn read_for_deal(
    conn: &Connection,
    tenant_id: &str,
    quote_id: &str,
) -> Result<Option<DealSourceRow>, QuoteIntakeError> {
    ensure_schema(conn)?;
    // DuckDB's TIMESTAMP rust binding can't auto-decode into
    // `Option<String>`; CAST to VARCHAR so the row reader is uniform
    // with the other VARCHAR columns. The ISO/RFC3339 round-trip we
    // need (UI + audit walks) survives the cast.
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(intake_state, ?3), stock_alert,
                    CAST(deal_issued_at AS VARCHAR),
                    material_grade, quantity
               FROM quote_intake_log
              WHERE quote_id = ?1 AND tenant_id = ?2
              LIMIT 1",
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("prepare read_for_deal: {e}")))?;
    let mut rows = stmt
        .query(params![quote_id, tenant_id, STATE_STAGED])
        .map_err(|e| QuoteIntakeError::Storage(format!("query read_for_deal: {e}")))?;
    let Some(row) = rows
        .next()
        .map_err(|e| QuoteIntakeError::Storage(format!("read read_for_deal row: {e}")))?
    else {
        return Ok(None);
    };
    let intake_state: String = row
        .get(0)
        .map_err(|e| QuoteIntakeError::Storage(format!("get intake_state: {e}")))?;
    let stock_alert_raw: Option<bool> = row
        .get(1)
        .map_err(|e| QuoteIntakeError::Storage(format!("get stock_alert: {e}")))?;
    let deal_issued_at: Option<String> = row
        .get(2)
        .map_err(|e| QuoteIntakeError::Storage(format!("get deal_issued_at: {e}")))?;
    let material_grade: Option<String> = row
        .get(3)
        .map_err(|e| QuoteIntakeError::Storage(format!("get material_grade: {e}")))?;
    let quantity: Option<i64> = row
        .get(4)
        .map_err(|e| QuoteIntakeError::Storage(format!("get quantity: {e}")))?;
    Ok(Some(DealSourceRow {
        intake_state,
        stock_alert: stock_alert_raw.unwrap_or(false),
        deal_issued_at,
        material_grade,
        quantity,
    }))
}

/// S272 / PR-261 — write the four DEAL columns inside the saga's tx.
/// The CAS guard is `WHERE deal_issued_at IS NULL` so a replay (the
/// EVE-addendum-3 single-use invariant) updates zero rows and the
/// caller can map that to a 409 `deal_already_issued`. Returns the
/// rows-updated count: `1` on a successful DEAL, `0` on a replay
/// against an already-dealt row (the saga's tx then rolls back its
/// audit appends and returns the 409).
///
/// `refresh_acked_at` is `Some` ONLY when the saga consumed an
/// operator REFRESH token; on a no-stock-alert saga it stays NULL so
/// the audit walk can prove acknowledgement.
pub fn mark_deal_issued_in_tx(
    tx: &duckdb::Transaction<'_>,
    tenant_id: &str,
    quote_id: &str,
    sales_order_id: &str,
    work_order_id: &str,
    issued_at: &str,
    refresh_acked_at: Option<&str>,
) -> Result<usize, QuoteIntakeError> {
    let n = tx
        .execute(
            "UPDATE quote_intake_log
                SET deal_issued_at = ?1,
                    deal_sales_order_id = ?2,
                    deal_work_order_id = ?3,
                    refresh_acked_at = ?4
              WHERE quote_id = ?5 AND tenant_id = ?6
                AND deal_issued_at IS NULL",
            params![
                issued_at,
                sales_order_id,
                work_order_id,
                refresh_acked_at,
                quote_id,
                tenant_id,
            ],
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("CAS mark_deal_issued: {e}")))?;
    Ok(n)
}

pub fn list_pending_writebacks(
    conn: &Connection,
    tenant_id: &str,
) -> Result<Vec<String>, QuoteIntakeError> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT quote_id FROM quote_intake_log
              WHERE tenant_id = ?1 AND status_writeback_at IS NULL",
        )
        .map_err(|e| QuoteIntakeError::Storage(format!("prepare list_pending_writebacks: {e}")))?;
    let mut rows = stmt
        .query(params![tenant_id])
        .map_err(|e| QuoteIntakeError::Storage(format!("query list_pending_writebacks: {e}")))?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| QuoteIntakeError::Storage(format!("read pending row: {e}")))?
    {
        let q: String = row
            .get(0)
            .map_err(|e| QuoteIntakeError::Storage(format!("get quote_id col: {e}")))?;
        out.push(q);
    }
    Ok(out)
}

fn format_iso(ts: OffsetDateTime) -> Result<String, QuoteIntakeError> {
    ts.format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| QuoteIntakeError::Storage(format!("format timestamp: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mem() -> Connection {
        Connection::open_in_memory().expect("in-memory DuckDB")
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = open_mem();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    #[test]
    fn already_intook_returns_none_for_fresh_quote() {
        let conn = open_mem();
        assert!(already_intook(&conn, "t1", "q-1").unwrap().is_none());
    }

    #[test]
    fn insert_then_already_intook_returns_some() {
        let conn = open_mem();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        insert_intake(
            &conn,
            "t1",
            "q-1",
            "inv_01ABC",
            "2026-01-01T00:00:00Z",
            now,
            "{}",
            "{}",
        )
        .unwrap();
        assert_eq!(
            already_intook(&conn, "t1", "q-1").unwrap(),
            Some("inv_01ABC".to_string())
        );
        assert!(already_intook(&conn, "t2", "q-1").unwrap().is_none());
    }

    #[test]
    fn double_insert_loud_fails() {
        let conn = open_mem();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        insert_intake(&conn, "t", "q", "inv_A", "r", now, "{}", "{}").unwrap();
        let err = insert_intake(&conn, "t", "q", "inv_B", "r", now, "{}", "{}").unwrap_err();
        assert!(matches!(err, QuoteIntakeError::Storage(_)));
    }

    #[test]
    fn mark_writeback_and_list_pending() {
        let conn = open_mem();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        insert_intake(&conn, "t", "q1", "inv_1", "r", now, "{}", "{}").unwrap();
        insert_intake(&conn, "t", "q2", "inv_2", "r", now, "{}", "{}").unwrap();
        let mut pending = list_pending_writebacks(&conn, "t").unwrap();
        pending.sort();
        assert_eq!(pending, vec!["q1".to_string(), "q2".to_string()]);
        mark_writeback_complete(&conn, "t", "q1", now).unwrap();
        let pending = list_pending_writebacks(&conn, "t").unwrap();
        assert_eq!(pending, vec!["q2".to_string()]);
    }

    // ── S256 / PR-245 — state + recovery + badge count ───────────────

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn count_unpicked_excludes_picked_error_and_irrelevant() {
        let conn = open_mem();
        // staged, un-picked → counts.
        insert_intake(&conn, "t", "q-staged", "inv_A", "r", now(), "{}", "{}").unwrap();
        // staged but picked up → excluded.
        insert_intake(&conn, "t", "q-picked", "inv_B", "r", now(), "{}", "{}").unwrap();
        set_picked_up_drf_id(&conn, "t", "q-picked", "drf_X").unwrap();
        // error → excluded.
        insert_error_intake(&conn, "t", "q-err", "r", now(), "{}", "bad email").unwrap();
        // irrelevant → excluded.
        insert_intake(&conn, "t", "q-irr", "inv_C", "r", now(), "{}", "{}").unwrap();
        mark_irrelevant(&conn, "t", "q-irr").unwrap();
        // other tenant → excluded.
        insert_intake(&conn, "t2", "q-other", "inv_D", "r", now(), "{}", "{}").unwrap();

        assert_eq!(count_unpicked(&conn, "t").unwrap(), 1);
    }

    #[test]
    fn error_row_blocks_reinsert_via_already_intook() {
        let conn = open_mem();
        insert_error_intake(&conn, "t", "q-err", "r", now(), "{\"x\":1}", "no email").unwrap();
        // Daemon precheck sees the error row and skips re-inserting.
        assert!(already_intook(&conn, "t", "q-err").unwrap().is_some());
        let (raw, state) = read_raw_and_state(&conn, "t", "q-err").unwrap().unwrap();
        assert_eq!(raw, "{\"x\":1}");
        assert_eq!(state, STATE_ERROR);
    }

    #[test]
    fn retry_parse_flips_error_to_staged_and_counts() {
        let conn = open_mem();
        insert_error_intake(&conn, "t", "q-err", "r", now(), "{}", "no email").unwrap();
        assert_eq!(count_unpicked(&conn, "t").unwrap(), 0);
        let n = retry_parse_intake(&conn, "t", "q-err", "inv_Z", "{\"ok\":true}").unwrap();
        assert_eq!(n, 1);
        assert_eq!(count_unpicked(&conn, "t").unwrap(), 1);
        // Re-running retry on an already-staged row is a no-op (guarded).
        let again = retry_parse_intake(&conn, "t", "q-err", "inv_Z", "{}").unwrap();
        assert_eq!(again, 0);
    }

    // ── S264 / PR-253 (F4) — pickup CAS ──────────────────────────────

    /// The CAS rejects a STALE-NULL claim against an already-claimed row
    /// (returns 0) and PRESERVES the winner's draft id. Pre-F4 the
    /// writeback was an unconditional `UPDATE ... SET picked_up_drf_id`
    /// with no guard — it would have returned 1 and clobbered the winner
    /// with the loser's draft id, leaving two orphan drafts. This test
    /// fails against that old unconditional UPDATE.
    #[test]
    fn claim_cas_rejects_stale_null_claim_and_preserves_winner() {
        let conn = open_mem();
        insert_intake(&conn, "t", "q", "inv_A", "r", now(), "{}", "{}").unwrap();

        // Winner: claims the fresh (NULL) row → 1 row updated.
        let won = claim_for_pickup_in_tx(&conn, "t", "q", "drf_WINNER", None).unwrap();
        assert_eq!(won, 1, "first claim against a NULL row wins");

        // Loser: read NULL earlier (stale), tries to claim with
        // expected_prior = None, but the column is now drf_WINNER → 0.
        let lost = claim_for_pickup_in_tx(&conn, "t", "q", "drf_LOSER", None).unwrap();
        assert_eq!(lost, 0, "a stale-NULL claim against a claimed row loses");

        // The winner's draft id must be intact (NOT clobbered).
        let row = read_for_pickup(&conn, "t", "q").unwrap().unwrap();
        assert_eq!(
            row.picked_up_drf_id.as_deref(),
            Some("drf_WINNER"),
            "the loser must not overwrite the winner"
        );
    }

    /// The CAS honours the legitimate re-pickup-after-S239-delete
    /// overwrite: `expected_prior = Some(old_drf)` claims the row when it
    /// still holds `old_drf`, but loses if a concurrent re-pickup already
    /// moved it.
    #[test]
    fn claim_cas_overwrites_when_expected_matches_else_loses() {
        let conn = open_mem();
        insert_intake(&conn, "t", "q", "inv_A", "r", now(), "{}", "{}").unwrap();
        set_picked_up_drf_id(&conn, "t", "q", "drf_OLD").unwrap();

        // Re-pickup whose read saw drf_OLD overwrites to drf_NEW.
        let ok = claim_for_pickup_in_tx(&conn, "t", "q", "drf_NEW", Some("drf_OLD")).unwrap();
        assert_eq!(ok, 1, "expected-prior match overwrites the deleted draft");
        let row = read_for_pickup(&conn, "t", "q").unwrap().unwrap();
        assert_eq!(row.picked_up_drf_id.as_deref(), Some("drf_NEW"));

        // A second re-pickup that still expects drf_OLD loses (the column
        // is drf_NEW now).
        let stale = claim_for_pickup_in_tx(&conn, "t", "q", "drf_OTHER", Some("drf_OLD")).unwrap();
        assert_eq!(stale, 0, "a stale expected-prior loses the race");
        let row = read_for_pickup(&conn, "t", "q").unwrap().unwrap();
        assert_eq!(row.picked_up_drf_id.as_deref(), Some("drf_NEW"));
    }

    // ── S271 / PR-260 — auto-quoting projection columns + stock_alert ─

    /// `ensure_schema` runs ALTER ... ADD COLUMN IF NOT EXISTS for each
    /// of the seven auto-quoting projection columns. A fresh DB MUST
    /// expose them via `INSERT ... SELECT` round-trip; a re-ensure call
    /// MUST stay idempotent (no double-add panic).
    #[test]
    fn s271_projection_columns_present_after_ensure_schema() {
        let conn = open_mem();
        ensure_schema(&conn).unwrap();
        // Insert a row that uses every new column, including the
        // NOT-NULL stock_alert (taking the DEFAULT FALSE on the schema).
        insert_intake(&conn, "t", "q1", "inv_A", "r", now(), "{}", "{}").unwrap();
        conn.execute(
            "UPDATE quote_intake_log
                SET customer_email = ?1,
                    material_grade = ?2,
                    quantity = ?3,
                    total_price_eur = ?4,
                    valid_until = DATE '2026-09-01',
                    stock_status_at_accept = ?5
              WHERE quote_id = ?6 AND tenant_id = ?7",
            params![
                "buyer@test",
                "6061-T6",
                7i64,
                12345.67_f64,
                "in_stock",
                "q1",
                "t"
            ],
        )
        .unwrap();
        // Confirm round-trip via SELECT. `stock_alert` carries no SQL
        // DEFAULT (per the DuckDB gotcha pinned above), so a fresh
        // INSERT that doesn't touch the column leaves it NULL.
        let (email, grade, qty, price, snap, alert): (
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<f64>,
            Option<String>,
            Option<bool>,
        ) = conn
            .query_row(
                "SELECT customer_email, material_grade, quantity, total_price_eur,
                        stock_status_at_accept, stock_alert
                   FROM quote_intake_log
                  WHERE quote_id = 'q1' AND tenant_id = 't'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(email.as_deref(), Some("buyer@test"));
        assert_eq!(grade.as_deref(), Some("6061-T6"));
        assert_eq!(qty, Some(7));
        assert_eq!(price, Some(12345.67));
        assert_eq!(snap.as_deref(), Some("in_stock"));
        assert_eq!(alert, None, "stock_alert is NULL on a fresh INSERT");
        // Re-ensure must not panic / double-add.
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    /// `flip_stock_alert_to_true` returns `true` on the first transition
    /// and `false` on every subsequent call — sticky.
    ///
    /// CRITICAL: this also pins the DuckDB DEFAULT-on-replay clobber
    /// trap. An earlier version of this PR added the column with
    /// `DEFAULT FALSE`; every `ensure_schema` replay (each writer
    /// re-runs it at the top per [[no-sql-specific]]'s
    /// migration-idempotency posture) then reset the column to FALSE,
    /// wiping any prior sticky TRUE write. Solution: omit the SQL
    /// DEFAULT entirely; treat NULL as FALSE in the app layer. This
    /// test catches a future re-introduction of the DEFAULT — the
    /// repeated `flip` call would re-fire as `true` instead of
    /// reporting the no-op.
    #[test]
    fn s271_flip_stock_alert_is_idempotent_and_sticky() {
        let conn = open_mem();
        insert_intake(&conn, "t", "q1", "inv_A", "r", now(), "{}", "{}").unwrap();
        // First flip: stored is FALSE → returns true (the transition).
        assert!(flip_stock_alert_to_true(&conn, "t", "q1").unwrap());
        // Second flip: stored is TRUE → returns false (sticky).
        assert!(!flip_stock_alert_to_true(&conn, "t", "q1").unwrap());
        // Quote-id mismatch: no row → returns false.
        assert!(!flip_stock_alert_to_true(&conn, "t", "q-other").unwrap());
        // Tenant mismatch: no row for this (tenant, quote_id) → false.
        assert!(!flip_stock_alert_to_true(&conn, "t-wrong", "q1").unwrap());
        // The stored value remains TRUE after the no-op flips.
        let alert: bool = conn
            .query_row(
                "SELECT stock_alert FROM quote_intake_log
                  WHERE quote_id = 'q1' AND tenant_id = 't'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(alert, "stock_alert remains TRUE after the no-op flips");
    }

    // ── S272 / PR-261 — DEAL-saga columns + read/CAS helpers ─────────

    /// The four S272 columns survive `ensure_schema` round-trips and
    /// start NULL on a fresh INSERT (no SQL DEFAULT — same DuckDB
    /// clobber trap as S271's `stock_alert`).
    #[test]
    fn s272_deal_columns_present_after_ensure_schema() {
        let conn = open_mem();
        ensure_schema(&conn).unwrap();
        insert_intake(&conn, "t", "q1", "inv_A", "r", now(), "{}", "{}").unwrap();
        let (issued, so, wo, ack): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT CAST(deal_issued_at AS VARCHAR),
                        deal_sales_order_id,
                        deal_work_order_id,
                        CAST(refresh_acked_at AS VARCHAR)
                   FROM quote_intake_log
                  WHERE quote_id = 'q1' AND tenant_id = 't'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(issued, None, "deal_issued_at NULL on fresh INSERT");
        assert_eq!(so, None);
        assert_eq!(wo, None);
        assert_eq!(ack, None);
        // Re-ensure must stay idempotent (no double-add panic, no
        // DEFAULT clobber of any prior write).
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    /// `read_for_deal` surfaces the three precondition columns the
    /// saga consults: intake_state (coerced NULL → staged), stock_alert
    /// (coerced NULL → false), and deal_issued_at. S273 widens to five
    /// columns; `material_grade` + `quantity` are NULL on a fresh INSERT
    /// (the storefront's S271 projection writer fills them later).
    #[test]
    fn s272_read_for_deal_surfaces_preconditions() {
        let conn = open_mem();
        insert_intake(&conn, "t", "q1", "inv_A", "r", now(), "{}", "{}").unwrap();
        let row = read_for_deal(&conn, "t", "q1").unwrap().unwrap();
        assert_eq!(row.intake_state, STATE_STAGED);
        assert!(!row.stock_alert);
        assert_eq!(row.deal_issued_at, None);
        // S273 — pre-storefront row has no material_grade / quantity.
        assert_eq!(row.material_grade, None);
        assert_eq!(row.quantity, None);

        // Flip stock_alert TRUE; the helper must surface it.
        assert!(flip_stock_alert_to_true(&conn, "t", "q1").unwrap());
        let row = read_for_deal(&conn, "t", "q1").unwrap().unwrap();
        assert!(row.stock_alert);

        // Tenant + quote_id mismatch → None.
        assert!(read_for_deal(&conn, "t", "q-other").unwrap().is_none());
        assert!(read_for_deal(&conn, "t-wrong", "q1").unwrap().is_none());

        // `irrelevant` intake_state round-trips through the COALESCE.
        mark_irrelevant(&conn, "t", "q1").unwrap();
        let row = read_for_deal(&conn, "t", "q1").unwrap().unwrap();
        assert_eq!(row.intake_state, STATE_IRRELEVANT);
    }

    /// S273 / PR-262 — once the storefront's S271 projection writer has
    /// populated `material_grade` + `quantity`, `read_for_deal` surfaces
    /// them so the saga can hand them to the material-commit branch.
    #[test]
    fn s273_read_for_deal_surfaces_material_grade_and_quantity() {
        let conn = open_mem();
        insert_intake(&conn, "t", "q1", "inv_A", "r", now(), "{}", "{}").unwrap();
        conn.execute(
            "UPDATE quote_intake_log
                SET material_grade = ?1, quantity = ?2
              WHERE quote_id = ?3 AND tenant_id = ?4",
            params!["6061-T6", 12i64, "q1", "t"],
        )
        .unwrap();
        let row = read_for_deal(&conn, "t", "q1").unwrap().unwrap();
        assert_eq!(row.material_grade.as_deref(), Some("6061-T6"));
        assert_eq!(row.quantity, Some(12));
    }

    /// CAS pin — `mark_deal_issued_in_tx` returns 1 on first call (the
    /// row's `deal_issued_at IS NULL`) and 0 on every replay (sticky).
    /// This is the EVE-addendum-3 single-use guard: a second saga call
    /// against the same row gets 0 rows-updated, the saga rolls back,
    /// and the route maps that to 409 `deal_already_issued`.
    #[test]
    fn s272_mark_deal_issued_is_single_use_via_cas() {
        let mut conn = open_mem();
        insert_intake(&conn, "t", "q1", "inv_A", "r", now(), "{}", "{}").unwrap();
        let tx = conn.transaction().unwrap();
        let won = mark_deal_issued_in_tx(
            &tx,
            "t",
            "q1",
            "so_ABC",
            "wo_XYZ",
            "2026-06-06T12:00:00Z",
            Some("2026-06-06T11:59:00Z"),
        )
        .unwrap();
        assert_eq!(won, 1, "first DEAL call wins the CAS");
        tx.commit().unwrap();

        // Replay: deal_issued_at is no longer NULL, CAS rejects.
        let tx = conn.transaction().unwrap();
        let lost = mark_deal_issued_in_tx(
            &tx,
            "t",
            "q1",
            "so_REPLAY",
            "wo_REPLAY",
            "2026-06-06T12:30:00Z",
            None,
        )
        .unwrap();
        assert_eq!(lost, 0, "replay against a dealt row loses the CAS");
        tx.commit().unwrap();

        // The original SO + WO ids remain intact (the replay CAS guarded
        // against a clobber).
        let (so, wo): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT deal_sales_order_id, deal_work_order_id
                   FROM quote_intake_log
                  WHERE quote_id = 'q1' AND tenant_id = 't'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(so.as_deref(), Some("so_ABC"));
        assert_eq!(wo.as_deref(), Some("wo_XYZ"));

        // `read_for_deal` now reports the dealt-at timestamp.
        let row = read_for_deal(&conn, "t", "q1").unwrap().unwrap();
        assert!(
            row.deal_issued_at.is_some(),
            "deal_issued_at carries through read_for_deal after CAS"
        );
    }

    #[test]
    fn mark_irrelevant_idempotent_and_removes_from_count() {
        let conn = open_mem();
        insert_intake(&conn, "t", "q1", "inv_A", "r", now(), "{}", "{}").unwrap();
        assert_eq!(count_unpicked(&conn, "t").unwrap(), 1);
        assert_eq!(mark_irrelevant(&conn, "t", "q1").unwrap(), 1);
        assert_eq!(count_unpicked(&conn, "t").unwrap(), 0);
        // Idempotent: a second mark still matches the row (rows-updated=1)
        // but the state is already irrelevant.
        assert_eq!(mark_irrelevant(&conn, "t", "q1").unwrap(), 1);
        assert_eq!(count_unpicked(&conn, "t").unwrap(), 0);
    }
}
