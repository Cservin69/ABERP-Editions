//! S211 / PR-210 — read-side queries over `quote_intake_log`.
//!
//! Surfaces the staged-quote queue to the SPA Quotes tab.
//! The daemon-write side lives in `aberp-quote-intake` crate; this
//! module is the read-side mirror the operator-facing route consumes.
//!
//! # Why the prepared-draft is parsed here, not in the daemon
//!
//! The daemon stores `prepared_draft` as opaque JSON in
//! `quote_intake_log.prepared_draft`. The shape is private to the
//! `aberp-quote-intake::mapping::PreparedDraft` struct. We only
//! surface a handful of operator-facing fields (contact info, material,
//! quantity) for the table row — the full draft is reserved for the
//! S212 pickup modal. So we deserialize lossy via `serde_json::Value`
//! rather than coupling this module to the crate's internal types.

use anyhow::Result;
use duckdb::{params, Connection};

/// Row mirror for the SPA Quotes tab. The `*_summary` fields are
/// extracted lossy from the prepared-draft JSON — missing fields become
/// `None` rather than failing the whole row (defence-in-depth: a
/// future schema change on the daemon-write side shouldn't 500 the
/// list route).
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuoteIntakeRow {
    pub quote_id: String,
    pub invoice_id: String,
    pub received_at: String,
    pub intake_at: String,
    pub status_writeback_at: Option<String>,
    pub contact_name: Option<String>,
    pub contact_email: Option<String>,
    pub contact_company: Option<String>,
    pub material: Option<String>,
    pub quantity: Option<String>,
    pub notes: Option<String>,
    /// S255 / PR-244 — `Some(<drf_id>)` when the operator already
    /// clicked "Create draft invoice" on this row. The SPA renders
    /// the "→ Draft" link instead of the pickup button when set.
    /// `None` for never-picked-up quotes.
    pub picked_up_drf_id: Option<String>,
}

/// List staged quote-intake rows for a tenant, ordered by intake time
/// descending. Caps at 500 rows to keep the SPA table responsive;
/// pagination is named-deferred to S212.
pub fn list_quote_intake_rows(conn: &Connection, tenant_id: &str) -> Result<Vec<QuoteIntakeRow>> {
    // The daemon's log_table::ensure_schema runs on every write, but
    // a fresh tenant whose daemon never started won't have the table
    // yet. Create-if-missing here so the SPA Quotes tab does not 500
    // on a tenant that has only configured the daemon (but never had
    // it spawn).
    conn.execute_batch(SCHEMA_BACKSTOP)?;
    // S255 / PR-244 — additive `picked_up_drf_id` column. Same
    // backstop posture as the base schema above: a fresh tenant DB
    // (or a pre-S255 boot followed by SPA load) gets the column
    // lazily.
    conn.execute_batch(S255_MIGRATION_BACKSTOP)?;
    let mut stmt = conn.prepare(
        "SELECT quote_id, invoice_id, received_at, intake_at,
                status_writeback_at, raw_payload, prepared_draft,
                picked_up_drf_id
           FROM quote_intake_log
          WHERE tenant_id = ?1
          ORDER BY intake_at DESC
          LIMIT 500",
    )?;
    let mut rows = stmt.query(params![tenant_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let quote_id: String = row.get(0)?;
        let invoice_id: String = row.get(1)?;
        let received_at: String = row.get(2)?;
        let intake_at: String = row.get(3)?;
        let status_writeback_at: Option<String> = row.get(4).ok();
        let raw_payload: String = row.get(5).unwrap_or_default();
        let prepared_draft: String = row.get(6).unwrap_or_default();
        let picked_up_drf_id: Option<String> = row.get(7).ok().flatten();

        let summary = extract_row_summary(&raw_payload, &prepared_draft);
        out.push(QuoteIntakeRow {
            quote_id,
            invoice_id,
            received_at,
            intake_at,
            status_writeback_at,
            contact_name: summary.contact_name,
            contact_email: summary.contact_email,
            contact_company: summary.contact_company,
            material: summary.material,
            quantity: summary.quantity,
            notes: summary.notes,
            picked_up_drf_id,
        });
    }
    Ok(out)
}

/// Match the daemon's `log_table::SCHEMA_SQL`. Kept in sync by the
/// integration-test pin (`tests/quote_intake_route.rs`); a divergence
/// surfaces there before reaching prod.
const SCHEMA_BACKSTOP: &str = "
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
";

/// S255 / PR-244 — additive `picked_up_drf_id` column. Mirrors
/// the daemon-write side's `log_table::S255_MIGRATION_SQL`; the
/// route-side mirror runs the ALTER lazily on every list call so a
/// fresh tenant whose daemon never spawned still gets the column.
const S255_MIGRATION_BACKSTOP: &str = "
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS picked_up_drf_id VARCHAR;
";

#[derive(Debug, Default)]
struct RowSummary {
    contact_name: Option<String>,
    contact_email: Option<String>,
    contact_company: Option<String>,
    material: Option<String>,
    quantity: Option<String>,
    notes: Option<String>,
}

/// Lossy field extraction. The raw quote payload carries the
/// operator-facing fields verbatim; the prepared draft is a backup
/// path in case a future daemon version moves the source of truth.
fn extract_row_summary(raw_payload_json: &str, prepared_draft_json: &str) -> RowSummary {
    let mut out = RowSummary::default();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_payload_json) {
        if let Some(contact) = value.get("contact").and_then(|v| v.as_object()) {
            out.contact_name = contact
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            out.contact_email = contact
                .get("email")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            out.contact_company = contact
                .get("company")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        out.material = value
            .get("material")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        out.quantity = value.get("quantity").and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
        out.notes = value
            .get("notes")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }
    if out.contact_name.is_none() || out.material.is_none() {
        if let Ok(draft) = serde_json::from_str::<serde_json::Value>(prepared_draft_json) {
            if out.contact_name.is_none() {
                out.contact_name = draft
                    .pointer("/customer/legal_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            if out.material.is_none() {
                out.material = draft
                    .pointer("/lines/0/description")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mem() -> Connection {
        Connection::open_in_memory().expect("in-memory DuckDB")
    }

    fn insert_test_row(
        conn: &Connection,
        tenant: &str,
        quote_id: &str,
        intake_at: &str,
        raw: &str,
        prepared: &str,
        writeback: Option<&str>,
    ) {
        conn.execute_batch(SCHEMA_BACKSTOP).unwrap();
        conn.execute(
            "INSERT INTO quote_intake_log (
                 quote_id, tenant_id, invoice_id, received_at, intake_at,
                 status_writeback_at, raw_payload, prepared_draft
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                quote_id,
                tenant,
                "inv_TEST",
                "2026-01-01T00:00:00Z",
                intake_at,
                writeback,
                raw,
                prepared,
            ],
        )
        .unwrap();
    }

    #[test]
    fn list_empty_on_fresh_db() {
        let conn = open_mem();
        let rows = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn list_filters_by_tenant_and_orders_desc() {
        let conn = open_mem();
        insert_test_row(&conn, "t1", "q-a", "2026-01-01T00:00:00Z", "{}", "{}", None);
        insert_test_row(&conn, "t1", "q-b", "2026-01-02T00:00:00Z", "{}", "{}", None);
        insert_test_row(&conn, "t2", "q-c", "2026-01-03T00:00:00Z", "{}", "{}", None);
        let rows = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].quote_id, "q-b");
        assert_eq!(rows[1].quote_id, "q-a");
    }

    #[test]
    fn extracts_contact_and_material_from_raw_payload() {
        let conn = open_mem();
        let raw = r#"{"contact":{"name":"Acme Co","email":"buy@acme.test","company":"Acme"},"material":"steel rod","quantity":5,"notes":"urgent"}"#;
        insert_test_row(&conn, "t1", "q-1", "2026-01-01T00:00:00Z", raw, "{}", None);
        let rows = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(rows[0].contact_name.as_deref(), Some("Acme Co"));
        assert_eq!(rows[0].contact_email.as_deref(), Some("buy@acme.test"));
        assert_eq!(rows[0].contact_company.as_deref(), Some("Acme"));
        assert_eq!(rows[0].material.as_deref(), Some("steel rod"));
        assert_eq!(rows[0].quantity.as_deref(), Some("5"));
        assert_eq!(rows[0].notes.as_deref(), Some("urgent"));
    }

    #[test]
    fn falls_back_to_prepared_draft_when_raw_payload_lacks_fields() {
        let conn = open_mem();
        let raw = "{}";
        let prepared = r#"{"customer":{"legal_name":"Backup Inc"},"lines":[{"description":"fallback material"}]}"#;
        insert_test_row(
            &conn,
            "t1",
            "q-1",
            "2026-01-01T00:00:00Z",
            raw,
            prepared,
            None,
        );
        let rows = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(rows[0].contact_name.as_deref(), Some("Backup Inc"));
        assert_eq!(rows[0].material.as_deref(), Some("fallback material"));
    }

    #[test]
    fn surface_writeback_status_round_trips() {
        let conn = open_mem();
        insert_test_row(
            &conn,
            "t1",
            "q-pending",
            "2026-01-01T00:00:00Z",
            "{}",
            "{}",
            None,
        );
        insert_test_row(
            &conn,
            "t1",
            "q-done",
            "2026-01-02T00:00:00Z",
            "{}",
            "{}",
            Some("2026-01-02T00:01:00Z"),
        );
        let rows = list_quote_intake_rows(&conn, "t1").unwrap();
        let by_id: std::collections::HashMap<_, _> =
            rows.into_iter().map(|r| (r.quote_id.clone(), r)).collect();
        assert!(by_id["q-pending"].status_writeback_at.is_none());
        assert_eq!(
            by_id["q-done"].status_writeback_at.as_deref(),
            Some("2026-01-02T00:01:00Z")
        );
    }

    #[test]
    fn malformed_json_does_not_500_the_route() {
        let conn = open_mem();
        insert_test_row(
            &conn,
            "t1",
            "q-1",
            "2026-01-01T00:00:00Z",
            "not json",
            "also not json",
            None,
        );
        let rows = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contact_name.is_none());
        assert!(rows[0].material.is_none());
    }
}
