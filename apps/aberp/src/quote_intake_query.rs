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

use std::collections::HashMap;

use anyhow::Result;
use duckdb::{params, Connection};

use aberp_quote_intake::log_table::flip_stock_alert_to_true;

use crate::quote_stock_alert::{coerce_stock_alert, recompute_stock_alert};

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
    /// S256 / PR-245 — closed-vocab intake state: `staged` (pickable),
    /// `error` (malformed — surfaced with `intake_error` + retry/dismiss
    /// actions), or `irrelevant` (operator-dismissed). Defaults to
    /// `staged` for pre-S256 rows.
    pub intake_state: String,
    /// S256 / PR-245 — operator-readable mapping-failure message on an
    /// `error`-state row; `None` otherwise.
    pub intake_error: Option<String>,
    // ── S271 / PR-260 — auto-quoting projection columns ────────────────
    /// Customer email, populated by the storefront's quote pipeline
    /// (separate repo). NULL on every row until the storefront PR ships.
    pub customer_email: Option<String>,
    /// Closed-vocab material grade (matches the operator's
    /// `quoting_materials.grade` PK). NULL until storefront populates.
    pub material_grade: Option<String>,
    /// Storefront-supplied integer quantity (the legacy
    /// `quantity_summary` String above stays for back-compat with the
    /// in-the-blob field; this is the typed column the auto-quote
    /// engine reads).
    pub quantity_canonical: Option<i64>,
    /// Total price the storefront engine computed at acceptance, in
    /// EUR.
    pub total_price_eur: Option<f64>,
    /// Quote validity expiry, as ISO `YYYY-MM-DD`. The storefront's
    /// pipeline derives this from operator-default-validity vs
    /// lead-time at acceptance time; ABERP-side this is read-only.
    pub valid_until: Option<String>,
    /// EVE addendum 2: snapshot of the material's `stock_status` at
    /// the moment the quote transitioned `priced → accepted`
    /// storefront-side. The `stock_alert` recompute compares this
    /// against the current `quoting_materials.stock_status` to decide
    /// whether the operator has to acknowledge a downgrade before
    /// DEAL.
    pub stock_status_at_accept: Option<String>,
    /// EVE addendum 2: TRUE iff the material has downgraded since
    /// `stock_status_at_accept`. Sticky — once TRUE, only the
    /// operator's REFRESH token (S272+) untriggers it. NULL coerced
    /// to FALSE.
    pub stock_alert: bool,
    // ── S272 / PR-261 — DEAL-saga projection ───────────────────────────
    /// `Some(<iso-ts>)` once the operator submitted a valid DEAL token
    /// (with REFRESH ack if `stock_alert` was TRUE) and the saga
    /// committed. The SPA renders a "DEAL issued" chip + the SO/WO ids
    /// instead of the DEAL gate when set. Single-use: the saga's CAS
    /// guarantees this column flips NULL → `Some` exactly once.
    pub deal_issued_at: Option<String>,
    /// `so_<ULID>` placeholder minted by the DEAL saga. Surfaced to the
    /// SPA for the post-deal "→ Sales Order" link affordance.
    pub deal_sales_order_id: Option<String>,
    /// `wo_<ULID>` placeholder minted by the DEAL saga. Surfaced to the
    /// SPA for the post-deal "→ Work Order" link affordance.
    pub deal_work_order_id: Option<String>,
}

/// S271 / PR-260 — one row whose `stock_alert` transitioned FALSE → TRUE
/// in this call. The SPA list route iterates over this list and writes
/// one `QuoteStockAlertTriggered` audit entry per row, attributing the
/// snapshot + current stock_status so a future operator can reconstruct
/// the WHY.
#[derive(Debug, Clone)]
pub struct StockAlertTriggered {
    pub quote_id: String,
    pub material_grade: String,
    pub snapshot_status: String,
    pub current_status: String,
}

/// Combined output of [`list_quote_intake_rows`]: the row list AND any
/// stock_alert transitions the recompute pass found. The route layer
/// emits the audit entries; the SPA receives only `rows`.
#[derive(Debug, Clone)]
pub struct QuoteIntakeListing {
    pub rows: Vec<QuoteIntakeRow>,
    pub newly_triggered_alerts: Vec<StockAlertTriggered>,
}

/// List staged quote-intake rows for a tenant, ordered by intake time
/// descending. Caps at 500 rows to keep the SPA table responsive;
/// pagination is named-deferred to S212.
///
/// S271 / PR-260 — also runs the EVE-addendum-2 `stock_alert` recompute
/// per row: reads `stock_status_at_accept` from the quote and compares
/// against the live `quoting_materials.stock_status` for the quote's
/// `material_grade`. Transitions FALSE → TRUE are persisted via
/// `flip_stock_alert_to_true` AND surfaced in
/// [`QuoteIntakeListing::newly_triggered_alerts`] so the route layer
/// can emit the audit entry. Sticky transitions (already-TRUE rows) do
/// not re-emit. Per the brief: "stock_alert detection runs on read,
/// not on schedule" — high-traffic quotes get recomputed more often,
/// low-traffic quotes get recomputed when the operator opens the tab.
pub fn list_quote_intake_rows(conn: &Connection, tenant_id: &str) -> Result<QuoteIntakeListing> {
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
    // S256 / PR-245 — additive `intake_state` + `intake_error` columns.
    // Same lazy-backstop posture as S255 for a fresh tenant DB.
    conn.execute_batch(S256_MIGRATION_BACKSTOP)?;
    // S271 / PR-260 — additive auto-quoting projection columns. Same
    // lazy-backstop posture. NONE carries a SQL DEFAULT — see the
    // log_table.rs gotcha note for why (DuckDB clobbers DEFAULT-bearing
    // columns on every replay).
    conn.execute_batch(S271_MIGRATION_BACKSTOP)?;
    // S272 / PR-261 — additive DEAL-saga columns. Same lazy-backstop
    // posture as S255–S271 — a tenant whose daemon never spawned still
    // gets the columns the moment the SPA opens the Quotes tab.
    conn.execute_batch(S272_MIGRATION_BACKSTOP)?;

    // Build the (grade → current stock_status) lookup ONCE for the whole
    // tenant. The recompute pass is per-row but the catalogue read is
    // O(catalogue_size); pulling it once amortises across all 500 rows.
    let current_stock_by_grade = read_current_stock_status_by_grade(conn, tenant_id)?;

    let mut stmt = conn.prepare(
        "SELECT quote_id, invoice_id, received_at, intake_at,
                status_writeback_at, raw_payload, prepared_draft,
                picked_up_drf_id,
                COALESCE(intake_state, 'staged'), intake_error,
                customer_email, material_grade, quantity,
                total_price_eur, valid_until, stock_status_at_accept,
                stock_alert,
                CAST(deal_issued_at AS VARCHAR),
                deal_sales_order_id, deal_work_order_id
           FROM quote_intake_log
          WHERE tenant_id = ?1
          ORDER BY intake_at DESC
          LIMIT 500",
    )?;
    let mut rows = stmt.query(params![tenant_id])?;
    let mut out = Vec::new();
    let mut pending_alerts: Vec<StockAlertTriggered> = Vec::new();
    while let Some(row) = rows.next()? {
        let quote_id: String = row.get(0)?;
        let invoice_id: String = row.get(1)?;
        let received_at: String = row.get(2)?;
        let intake_at: String = row.get(3)?;
        let status_writeback_at: Option<String> = row.get(4).ok();
        let raw_payload: String = row.get(5).unwrap_or_default();
        let prepared_draft: String = row.get(6).unwrap_or_default();
        let picked_up_drf_id: Option<String> = row.get(7).ok().flatten();
        let intake_state: String = row
            .get::<_, String>(8)
            .unwrap_or_else(|_| "staged".to_string());
        let intake_error: Option<String> = row.get(9).ok().flatten();
        let customer_email: Option<String> = row.get(10).ok().flatten();
        let material_grade: Option<String> = row.get(11).ok().flatten();
        let quantity_canonical: Option<i64> = row.get(12).ok().flatten();
        let total_price_eur: Option<f64> = row.get(13).ok().flatten();
        let valid_until: Option<String> = row.get(14).ok().flatten();
        let stock_status_at_accept: Option<String> = row.get(15).ok().flatten();
        let stored_alert_db: Option<bool> = row.get(16).ok().flatten();
        let deal_issued_at: Option<String> = row.get(17).ok().flatten();
        let deal_sales_order_id: Option<String> = row.get(18).ok().flatten();
        let deal_work_order_id: Option<String> = row.get(19).ok().flatten();

        // S271 — recompute alert. Sticky on TRUE; downgrades trigger.
        let stored_alert = coerce_stock_alert(stored_alert_db);
        let current_status_for_quote = material_grade
            .as_deref()
            .and_then(|g| current_stock_by_grade.get(g).map(String::as_str));
        let next_alert = recompute_stock_alert(
            stock_status_at_accept.as_deref(),
            current_status_for_quote,
            stored_alert,
        );
        let stock_alert = if next_alert && !stored_alert {
            // FALSE → TRUE transition; persist + queue the audit emit
            // for the route layer. `flip_stock_alert_to_true` returns
            // true on exactly the first transition (sticky in DB), so
            // a parallel SPA reload between recompute and persist
            // resolves to one audit emit per row.
            let flipped = flip_stock_alert_to_true(conn, tenant_id, &quote_id)
                .map_err(|e| anyhow::anyhow!("persist stock_alert flip: {e}"))?;
            if flipped {
                pending_alerts.push(StockAlertTriggered {
                    quote_id: quote_id.clone(),
                    material_grade: material_grade.clone().unwrap_or_default(),
                    snapshot_status: stock_status_at_accept.clone().unwrap_or_default(),
                    current_status: current_status_for_quote
                        .map(str::to_string)
                        .unwrap_or_default(),
                });
            }
            true
        } else {
            next_alert
        };

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
            intake_state,
            intake_error,
            customer_email,
            material_grade,
            quantity_canonical,
            total_price_eur,
            valid_until,
            stock_status_at_accept,
            stock_alert,
            deal_issued_at,
            deal_sales_order_id,
            deal_work_order_id,
        });
    }
    Ok(QuoteIntakeListing {
        rows: out,
        newly_triggered_alerts: pending_alerts,
    })
}

/// Look up the current `stock_status` of every `quoting_materials` row
/// for the tenant. Returns an empty map if the table doesn't exist yet
/// (a fresh tenant that has staged a quote via the daemon but never
/// opened Settings → Material Catalogue).
fn read_current_stock_status_by_grade(
    conn: &Connection,
    tenant_id: &str,
) -> Result<HashMap<String, String>> {
    // The Material Catalogue's `ensure_schema` runs on each CRUD write,
    // not on every read; an empty catalogue here is fine. We simply
    // detect the no-table case via prepare-and-recover.
    let mut map: HashMap<String, String> = HashMap::new();
    let mut stmt = match conn.prepare(
        "SELECT grade, stock_status FROM quoting_materials
          WHERE tenant_id = ?1",
    ) {
        Ok(s) => s,
        Err(duckdb::Error::DuckDBFailure(_, Some(msg)))
            if msg.contains("does not exist") || msg.contains("Table") =>
        {
            return Ok(map);
        }
        Err(e) => return Err(e.into()),
    };
    let mut rows = stmt.query(params![tenant_id])?;
    while let Some(row) = rows.next()? {
        let grade: String = row.get(0)?;
        let stock_status: String = row.get(1)?;
        map.insert(grade, stock_status);
    }
    Ok(map)
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

/// S256 / PR-245 — additive `intake_state` + `intake_error` columns.
/// Mirrors the daemon-write side's `log_table::S256_MIGRATION_SQL`; run
/// lazily on every list call so a fresh tenant whose daemon never
/// spawned still gets the columns. `DEFAULT 'staged'` backfills pre-S256
/// rows.
const S256_MIGRATION_BACKSTOP: &str = "
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS intake_state VARCHAR DEFAULT 'staged';
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS intake_error VARCHAR;
";

/// S271 / PR-260 — additive auto-quoting projection columns. Mirrors
/// the daemon-write side's `log_table::S271_MIGRATION_SQL`. NO SQL
/// DEFAULT on `stock_alert` — DuckDB re-applies DEFAULT on every replay
/// of `ALTER TABLE ... ADD COLUMN IF NOT EXISTS ... DEFAULT V`, which
/// would clobber the sticky TRUE that `flip_stock_alert_to_true` writes.
/// The app layer coerces NULL → FALSE via `quote_stock_alert::
/// coerce_stock_alert`.
const S271_MIGRATION_BACKSTOP: &str = "
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

/// S272 / PR-261 — additive DEAL-saga columns. Mirrors the daemon-write
/// side's `log_table::S272_MIGRATION_SQL`. NO SQL DEFAULTs (same DuckDB
/// DEFAULT-on-replay clobber trap pinned in S271's `stock_alert`); the
/// single-use invariant rides the app-layer CAS on `deal_issued_at IS NULL`.
const S272_MIGRATION_BACKSTOP: &str = "
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS deal_issued_at TIMESTAMP;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS deal_sales_order_id VARCHAR;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS deal_work_order_id VARCHAR;
ALTER TABLE quote_intake_log
    ADD COLUMN IF NOT EXISTS refresh_acked_at TIMESTAMP;
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
        let rows = list_quote_intake_rows(&conn, "t1").unwrap().rows;
        assert!(rows.is_empty());
    }

    #[test]
    fn list_filters_by_tenant_and_orders_desc() {
        let conn = open_mem();
        insert_test_row(&conn, "t1", "q-a", "2026-01-01T00:00:00Z", "{}", "{}", None);
        insert_test_row(&conn, "t1", "q-b", "2026-01-02T00:00:00Z", "{}", "{}", None);
        insert_test_row(&conn, "t2", "q-c", "2026-01-03T00:00:00Z", "{}", "{}", None);
        let rows = list_quote_intake_rows(&conn, "t1").unwrap().rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].quote_id, "q-b");
        assert_eq!(rows[1].quote_id, "q-a");
    }

    #[test]
    fn extracts_contact_and_material_from_raw_payload() {
        let conn = open_mem();
        let raw = r#"{"contact":{"name":"Acme Co","email":"buy@acme.test","company":"Acme"},"material":"steel rod","quantity":5,"notes":"urgent"}"#;
        insert_test_row(&conn, "t1", "q-1", "2026-01-01T00:00:00Z", raw, "{}", None);
        let rows = list_quote_intake_rows(&conn, "t1").unwrap().rows;
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
        let rows = list_quote_intake_rows(&conn, "t1").unwrap().rows;
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
        let rows = list_quote_intake_rows(&conn, "t1").unwrap().rows;
        let by_id: std::collections::HashMap<_, _> =
            rows.into_iter().map(|r| (r.quote_id.clone(), r)).collect();
        assert!(by_id["q-pending"].status_writeback_at.is_none());
        assert_eq!(
            by_id["q-done"].status_writeback_at.as_deref(),
            Some("2026-01-02T00:01:00Z")
        );
    }

    // ── S271 / PR-260 — EVE-addendum-2 stock_alert recompute pin ──────

    /// Helper: seed a `quoting_materials` row for stock_alert lookups.
    fn seed_material(conn: &Connection, tenant: &str, grade: &str, stock_status: &str) {
        // Mirror the table schema enough to satisfy the recompute's
        // SELECT — we don't need every column for the recompute path.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS quoting_materials (
                grade                   VARCHAR NOT NULL PRIMARY KEY,
                tenant_id               VARCHAR NOT NULL,
                display_name            VARCHAR NOT NULL,
                density_g_cm3           DOUBLE  NOT NULL,
                cost_per_kg_eur         DOUBLE  NOT NULL,
                machinability_index     DOUBLE  NOT NULL DEFAULT 1.0,
                carbide_life_multiplier DOUBLE  NOT NULL DEFAULT 1.0,
                stock_status            VARCHAR NOT NULL,
                lead_time_default_days  INTEGER NOT NULL,
                quote_multiplier        DOUBLE  NOT NULL DEFAULT 1.0,
                notes                   VARCHAR,
                updated_at              VARCHAR NOT NULL,
                updated_by_actor        VARCHAR NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO quoting_materials (
                 grade, tenant_id, display_name, density_g_cm3,
                 cost_per_kg_eur, stock_status, lead_time_default_days,
                 updated_at, updated_by_actor
             ) VALUES (?1, ?2, ?3, 2.7, 5.0, ?4, 2, 'now', 'test')",
            params![grade, tenant, format!("{grade} (test)"), stock_status],
        )
        .unwrap();
    }

    /// Helper: insert a quote_intake_log row with the S271 fields the
    /// recompute reads — `material_grade` + `stock_status_at_accept`.
    /// The S271 migration runs lazily inside `list_quote_intake_rows`,
    /// but the test backfills the columns BEFORE that first call so the
    /// helper applies the migration eagerly.
    fn insert_accepted_quote(
        conn: &Connection,
        tenant: &str,
        quote_id: &str,
        grade: &str,
        stock_status_at_accept: &str,
    ) {
        insert_test_row(
            conn,
            tenant,
            quote_id,
            "2026-01-01T00:00:00Z",
            "{}",
            "{}",
            None,
        );
        conn.execute_batch(S271_MIGRATION_BACKSTOP).unwrap();
        conn.execute(
            "UPDATE quote_intake_log
                SET material_grade = ?1,
                    stock_status_at_accept = ?2
              WHERE quote_id = ?3 AND tenant_id = ?4",
            params![grade, stock_status_at_accept, quote_id, tenant],
        )
        .unwrap();
    }

    /// EVE addendum 2 acceptance criterion — the cut report's load-
    /// bearing pin: "stock_alert column: PASS — sticky downgrade test
    /// passes both directions."
    ///
    /// Phase 1 — accept at in_stock, catalogue downgrades to source_1_2d:
    /// the recompute MUST trigger `stock_alert = TRUE` AND surface the
    /// transition via `newly_triggered_alerts` so the route emits one
    /// `QuoteStockAlertTriggered` audit entry. The flip MUST persist.
    ///
    /// Phase 2 — catalogue RECOVERS to in_stock: the recompute MUST
    /// LEAVE `stock_alert = TRUE` (sticky) AND MUST NOT re-emit a
    /// transition (`newly_triggered_alerts.is_empty()`). Only the
    /// operator REFRESH (S272+) untriggers the column.
    #[test]
    fn s271_stock_alert_sticky_downgrade_and_recovery() {
        let conn = open_mem();
        // Catalogue: material at in_stock.
        seed_material(&conn, "t1", "6061-T6", "in_stock");
        // Quote: accepted at in_stock.
        insert_accepted_quote(&conn, "t1", "q-alpha", "6061-T6", "in_stock");

        // Sanity: with snapshot == current, the recompute is a no-op.
        let listing0 = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(listing0.rows.len(), 1);
        assert!(!listing0.rows[0].stock_alert, "equal-tier → no alert");
        assert!(
            listing0.newly_triggered_alerts.is_empty(),
            "no transition recorded on equal tier"
        );

        // Phase 1 — catalogue downgrades. List route triggers alert.
        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'source_1_2d'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();
        let listing1 = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(listing1.rows.len(), 1);
        assert!(listing1.rows[0].stock_alert, "downgrade must trigger alert");
        assert_eq!(
            listing1.newly_triggered_alerts.len(),
            1,
            "one transition emitted"
        );
        let alert = &listing1.newly_triggered_alerts[0];
        assert_eq!(alert.quote_id, "q-alpha");
        assert_eq!(alert.material_grade, "6061-T6");
        assert_eq!(alert.snapshot_status, "in_stock");
        assert_eq!(alert.current_status, "source_1_2d");

        // Persistence pin: the second list call sees stock_alert TRUE
        // in the DB and does NOT re-emit the transition (idempotent).
        let listing2 = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(listing2.rows[0].stock_alert, "alert persisted");
        assert!(
            listing2.newly_triggered_alerts.is_empty(),
            "no re-emit on a sticky-TRUE row"
        );

        // Phase 2 — catalogue RECOVERS to in_stock. Sticky pin: the
        // alert MUST stay TRUE; no new transition emitted.
        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'in_stock'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();
        let listing3 = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(
            listing3.rows[0].stock_alert,
            "stock_alert STAYS TRUE after recovery (EVE addendum 2 sticky rule)"
        );
        assert!(
            listing3.newly_triggered_alerts.is_empty(),
            "recovery does NOT emit a fresh transition"
        );

        // Even a "downgrade-after-recovery" round-trip must not re-emit.
        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'source_3_7d'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();
        let listing4 = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(listing4.rows[0].stock_alert, "still sticky");
        assert!(
            listing4.newly_triggered_alerts.is_empty(),
            "a second downgrade after a sticky-TRUE row emits no fresh entry"
        );
    }

    /// A quote that has NOT been accepted yet (NULL `stock_status_at_accept`)
    /// must not trigger an alert even if the catalogue's stock_status is
    /// at a bad tier — the recompute requires a snapshot to compare
    /// against.
    #[test]
    fn s271_unaccepted_quote_never_triggers() {
        let conn = open_mem();
        seed_material(&conn, "t1", "6061-T6", "special_order");
        insert_test_row(
            &conn,
            "t1",
            "q-unaccepted",
            "2026-01-01T00:00:00Z",
            "{}",
            "{}",
            None,
        );
        // Backfill the grade BUT NOT the snapshot.
        conn.execute_batch(S271_MIGRATION_BACKSTOP).unwrap();
        conn.execute(
            "UPDATE quote_intake_log SET material_grade = '6061-T6'
              WHERE quote_id = 'q-unaccepted' AND tenant_id = 't1'",
            [],
        )
        .unwrap();
        let listing = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(listing.rows.len(), 1);
        assert!(
            !listing.rows[0].stock_alert,
            "no snapshot → no alert (pre-acceptance row)"
        );
        assert!(listing.newly_triggered_alerts.is_empty());
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
        let rows = list_quote_intake_rows(&conn, "t1").unwrap().rows;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contact_name.is_none());
        assert!(rows[0].material.is_none());
    }
}
