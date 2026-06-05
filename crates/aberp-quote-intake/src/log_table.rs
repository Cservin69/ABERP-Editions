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
    })
}

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
             raw_payload, prepared_draft
         ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7)",
        params![
            quote_id,
            tenant_id,
            invoice_id,
            received_at,
            intake_at_iso,
            raw_payload_json,
            prepared_draft_json,
        ],
    )
    .map_err(|e| QuoteIntakeError::Storage(format!("insert quote_intake_log row: {e}")))?;
    Ok(())
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
}
