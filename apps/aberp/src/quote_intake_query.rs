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

use anyhow::{Context, Result};
use duckdb::{params, Connection};

use crate::quote_pdf_rerender_queue::QuotePdfRerenderQueue;
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

    // S329 / 🔴1 — arm the stock-alert trigger in production. The
    // `stock_status_at_accept` baseline (and the closed-vocab
    // `material_grade` the recompute keys on) had NO production writer:
    // every prior writer lived inside `#[cfg(test)]`, so in prod the
    // column was always NULL → `recompute_stock_alert` always returned
    // FALSE → the whole S318/S323/S325 customer-banner arc was dead.
    // Project both from the authoritative `quote_pricing_jobs` row (same
    // `quote_id`) at the moment the operator first observes the
    // already-accepted, fully-priced quote. See `project_accept_snapshot`.
    project_accept_snapshot(conn, tenant_id)?;

    // Build the (grade → current stock_status) lookup ONCE for the whole
    // tenant. The recompute pass is per-row but the catalogue read is
    // O(catalogue_size); pulling it once amortises across all 500 rows.
    let current_stock_by_grade = read_current_stock_status_by_grade(conn, tenant_id)?;

    // S413 — exclude TERMINAL intake states from the operator's actionable
    // Quotes queue. `refused` (S403 operator REFUSE-with-reason) and
    // `irrelevant` (operator Dismiss of a parse-error / dead-letter row)
    // are both final dispositions on which no further operator action is
    // possible — the refuse handler itself blocks ("only staged rows can be
    // refused"). They previously stayed in the listing; a `refused` row in
    // particular had no SPA branch and rendered with the full staged
    // action set, inviting a click that always 409'd. Filtering them in the
    // query (the [[trust-code-not-operator]] defence — code, not operator
    // memory) is the one-click final disposition: the row leaves the queue
    // and never comes back. The closed vocab lives in
    // `aberp_quote_intake::log_table::{STATE_REFUSED, STATE_IRRELEVANT}`;
    // matched here as literals to mirror the `next_actionable_job`
    // convention in `quote_pricing_jobs`. `staged` (actionable), `error`
    // (retry-parse / dismiss actionable) and DEAL'd rows stay visible.
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
            AND COALESCE(intake_state, 'staged') NOT IN ('refused', 'irrelevant')
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
        //
        // S275 / F6 — narrow the recompute to rows the operator can still
        // ACT on: only `staged` rows that are un-dealt + un-picked-up. A
        // dealt row already locked the material commit (the REFRESH gate
        // is moot after the saga commits); an `error` / `irrelevant` row
        // is not pickable; a picked-up row already minted a draft and the
        // alert no longer routes operator action. Skipping these stops
        // post-DEAL catalogue churn from emitting noise audit entries and
        // keeps the stored `stock_alert` aligned with rows the operator
        // can still REFRESH-ack.
        let stored_alert = coerce_stock_alert(stored_alert_db);
        let is_actionable =
            intake_state == "staged" && deal_issued_at.is_none() && picked_up_drf_id.is_none();
        let current_status_for_quote = material_grade
            .as_deref()
            .and_then(|g| current_stock_by_grade.get(g).map(String::as_str));
        let next_alert = if is_actionable {
            recompute_stock_alert(
                stock_status_at_accept.as_deref(),
                current_status_for_quote,
                stored_alert,
            )
        } else {
            stored_alert
        };
        let stock_alert = if is_actionable && next_alert && !stored_alert {
            // S275 / F2 — record the transition for the caller's
            // per-row tx to persist + emit. The route layer (or the
            // test-only persist helper) calls `flip_and_audit_in_tx`
            // which runs the guarded UPDATE + audit append in one tx;
            // the race between concurrent recompute passes is settled
            // by DuckDB's `rows_affected == 1` semantics, not by the
            // memo-snapshot here.
            pending_alerts.push(StockAlertTriggered {
                quote_id: quote_id.clone(),
                material_grade: material_grade.clone().unwrap_or_default(),
                snapshot_status: stock_status_at_accept.clone().unwrap_or_default(),
                current_status: current_status_for_quote
                    .map(str::to_string)
                    .unwrap_or_default(),
            });
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

/// S325 / PR-25 — persist each newly-triggered `stock_alert` transition
/// AND schedule the customer-facing PDF re-render.
///
/// For every entry in `alerts` this runs ONE tx that:
/// 1. flips `quote_intake_log.stock_alert` FALSE → TRUE under the guarded
///    CAS UPDATE (`flip_and_audit_in_tx`) + appends the
///    `QuoteStockAlertTriggered` audit row (existing S275 behaviour);
/// 2. on a confirmed flip (this call won the race), appends the new
///    `QuotePdfRerenderEnqueued` audit row IN THE SAME TX, so the customer
///    flag flip and the re-render intent are atomic — either both land or
///    neither.
/// After the tx commits, the quote-id is pushed into the in-memory
/// `rerender_queue` (AFTER commit, so a rolled-back flip never leaves a
/// phantom queue entry). The [`crate::quote_pdf_rerender_daemon`] drains
/// the queue and re-renders + re-POSTs `priced.pdf` with the stock-alert
/// banner (S318 capability) on the next cycle.
///
/// Extracted from the `handle_list_quote_intake` route so the read-side
/// detection → enqueue path is unit-testable without standing up an HTTP
/// server. Returns the quote-ids actually enqueued (a confirmed flip that
/// won the race); a lost race or a `None`/empty `alerts` returns an empty
/// vec.
pub fn persist_alerts_and_enqueue_rerender(
    conn: &mut Connection,
    tenant: &aberp_audit_ledger::TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
    operator_login: &str,
    alerts: &[StockAlertTriggered],
    rerender_queue: &QuotePdfRerenderQueue,
) -> Result<Vec<String>> {
    if alerts.is_empty() {
        return Ok(Vec::new());
    }
    aberp_audit_ledger::ensure_schema(conn)
        .context("ensure audit ledger schema for stock_alert + rerender emit")?;
    let ledger_meta = aberp_audit_ledger::LedgerMeta::new(tenant.clone(), binary_hash);
    let mut enqueued: Vec<String> = Vec::new();
    for alert in alerts {
        let payload = serde_json::json!({
            "quote_id": alert.quote_id,
            "material_grade": alert.material_grade,
            "snapshot_status": alert.snapshot_status,
            "current_status": alert.current_status,
        });
        let bytes =
            serde_json::to_vec(&payload).context("serialize QuoteStockAlertTriggered payload")?;
        let actor = aberp_audit_ledger::Actor::from_local_cli(
            ulid::Ulid::new().to_string(),
            operator_login,
        );
        // Per-(quote, transition) idempotency key: a transient retry of
        // the list route cannot land two audit entries for one flip.
        let idempotency_key = format!("stock_alert_triggered:{}", alert.quote_id);
        let tx = conn
            .transaction()
            .context("begin tx for stock_alert flip + rerender enqueue")?;
        let flipped = aberp_quote_intake::log_table::flip_and_audit_in_tx(
            &tx,
            tenant.as_str(),
            &alert.quote_id,
            &ledger_meta,
            bytes,
            actor,
            idempotency_key,
        )
        .context("flip + QuoteStockAlertTriggered audit in tx")?;
        if !flipped {
            // Lost the race — another process already flipped the row.
            // Drop the tx so neither audit row lands; the winner enqueued.
            drop(tx);
            continue;
        }
        // The flip landed. Record the re-render intent in the SAME tx.
        let rr_payload = serde_json::json!({
            "quote_id": alert.quote_id,
            "material_grade": alert.material_grade,
            "snapshot_status": alert.snapshot_status,
            "current_status": alert.current_status,
        });
        let rr_bytes = serde_json::to_vec(&rr_payload)
            .context("serialize QuotePdfRerenderEnqueued payload")?;
        let rr_actor = aberp_audit_ledger::Actor::from_local_cli(
            ulid::Ulid::new().to_string(),
            operator_login,
        );
        aberp_audit_ledger::append_in_tx(
            &tx,
            &ledger_meta,
            aberp_audit_ledger::EventKind::QuotePdfRerenderEnqueued,
            rr_bytes,
            rr_actor,
            Some(format!("pdf_rerender_enqueued:{}", alert.quote_id)),
        )
        .context("append QuotePdfRerenderEnqueued in tx")?;
        tx.commit()
            .context("commit stock_alert flip + rerender-enqueue tx")?;
        // Enqueue AFTER commit — never hold an id whose flip rolled back.
        rerender_queue.enqueue(&alert.quote_id);
        enqueued.push(alert.quote_id.clone());
    }
    Ok(enqueued)
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

/// S329 / 🔴1 — the production writer for `stock_status_at_accept` (and
/// the closed-vocab `material_grade`) on `quote_intake_log`.
///
/// # Why this is the seam
///
/// A `quote_intake_log` row exists only because the quote-intake daemon
/// fetched the quote from the storefront's *approved* feed — i.e. the
/// customer has already accepted. The closed-vocab `material_grade` the
/// recompute keys on is not in the intake payload (which carries only the
/// free-text `material_preference`); it lives on the auto-quote pipeline's
/// `quote_pricing_jobs` row, keyed on the SAME `quote_id`, where it was
/// validated against the catalogue before the priced PDF was posted. So
/// the only place both the grade AND the live catalogue are reachable is
/// here, ABERP-side, on the operator's Quotes-tab read.
///
/// For every row still missing its snapshot, join the matching `posted`
/// pricing job and the catalogue, then write `material_grade` +
/// `stock_status_at_accept = <current stock_status>` in one guarded,
/// write-once UPDATE (`stock_status_at_accept IS NULL`). Gating on
/// `state = 'posted'` guarantees the re-render artifacts already exist, so
/// a later FALSE→TRUE flip cannot enqueue a quote the daemon would then
/// fail as `artifacts_missing`.
///
/// Best-effort: a fresh tenant with no `quote_pricing_jobs` table yet
/// simply has nothing to project (the recompute stays a no-op).
fn project_accept_snapshot(conn: &Connection, tenant_id: &str) -> Result<()> {
    // Guard the no-table case (a tenant that staged intake rows via the
    // daemon but never ran the auto-quote pipeline) the same way
    // `read_current_stock_status_by_grade` guards the catalogue: detect
    // the missing table via prepare-and-recover rather than a schema
    // create, so the read path never mutates the pricing-jobs schema.
    let mut stmt = match conn.prepare(
        "UPDATE quote_intake_log
            SET material_grade = j.material_grade,
                stock_status_at_accept = m.stock_status
           FROM quote_pricing_jobs j, quoting_materials m
          WHERE quote_intake_log.tenant_id = ?1
            AND quote_intake_log.stock_status_at_accept IS NULL
            AND j.tenant_id = quote_intake_log.tenant_id
            AND j.quote_id = quote_intake_log.quote_id
            AND j.state = 'posted'
            AND m.tenant_id = quote_intake_log.tenant_id
            AND m.grade = j.material_grade",
    ) {
        Ok(s) => s,
        Err(duckdb::Error::DuckDBFailure(_, Some(msg)))
            if msg.contains("does not exist") || msg.contains("Table") =>
        {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    stmt.execute(params![tenant_id])
        .context("project stock_status_at_accept snapshot from quote_pricing_jobs")?;
    Ok(())
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

    /// S413 — TERMINAL intake states (`refused` = S403 operator
    /// REFUSE-with-reason, `irrelevant` = operator Dismiss) drop out of the
    /// actionable Quotes queue; `staged` (actionable) and `error`
    /// (retry-parse / dismiss actionable) stay. Defends the operator from a
    /// no-op click on a row that can take no further action.
    #[test]
    fn list_excludes_terminal_refused_and_irrelevant_states() {
        let conn = open_mem();
        for (qid, at) in [
            ("q-staged", "2026-01-05T00:00:00Z"),
            ("q-error", "2026-01-04T00:00:00Z"),
            ("q-refused", "2026-01-03T00:00:00Z"),
            ("q-irrelevant", "2026-01-02T00:00:00Z"),
        ] {
            insert_test_row(&conn, "t1", qid, at, "{}", "{}", None);
        }
        // The S256 `intake_state` column lands lazily inside
        // `list_quote_intake_rows`; apply the backstop now so the UPDATE
        // below can reference it (mirrors the S256 test setup).
        conn.execute_batch(S256_MIGRATION_BACKSTOP).unwrap();
        // Flip the two terminal rows + the error row to their states.
        for (qid, state) in [
            ("q-error", "error"),
            ("q-refused", "refused"),
            ("q-irrelevant", "irrelevant"),
        ] {
            conn.execute(
                "UPDATE quote_intake_log SET intake_state = ?1 WHERE quote_id = ?2",
                params![state, qid],
            )
            .unwrap();
        }
        let rows = list_quote_intake_rows(&conn, "t1").unwrap().rows;
        let ids: std::collections::HashSet<_> = rows.iter().map(|r| r.quote_id.as_str()).collect();
        assert!(ids.contains("q-staged"), "staged is actionable");
        assert!(ids.contains("q-error"), "error is retry/dismiss actionable");
        assert!(!ids.contains("q-refused"), "refused is terminal → filtered");
        assert!(
            !ids.contains("q-irrelevant"),
            "irrelevant is terminal → filtered"
        );
        assert_eq!(rows.len(), 2);
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

        // S275 / F2 — the recompute pass NO LONGER persists in-place.
        // The route (or this test) drives the persist via a CAS
        // UPDATE; subsequent list calls then see stored=true and
        // sticky-short-circuit without re-emitting. Without the
        // persist, the recompute would re-push pending_alerts on
        // every list call — pinned below.
        let listing_no_persist = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(
            listing_no_persist.rows[0].stock_alert,
            "alert still in-memory"
        );
        assert_eq!(
            listing_no_persist.newly_triggered_alerts.len(),
            1,
            "without the route's persist, recompute re-pushes the transition"
        );
        // Run the route's CAS UPDATE so subsequent recomputes
        // sticky-short-circuit (the audit emit is exercised in
        // log_table.rs's own `flip_and_audit_in_tx` test).
        let flipped =
            aberp_quote_intake::log_table::flip_stock_alert_to_true(&conn, "t1", "q-alpha")
                .unwrap();
        assert!(flipped, "CAS UPDATE matched the FALSE→TRUE transition");
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

    /// S275 / F6 — a row that's already been DEALt no longer takes a
    /// stock_alert recompute hit. A post-DEAL catalogue downgrade is
    /// noise: the material commit is locked, the REFRESH gate doesn't
    /// route anywhere, and emitting a `QuoteStockAlertTriggered` audit
    /// entry adds zero forensic value while costing one ledger row.
    #[test]
    fn s275_recompute_skips_dealt_rows() {
        let conn = open_mem();
        seed_material(&conn, "t1", "6061-T6", "in_stock");
        insert_accepted_quote(&conn, "t1", "q-dealt", "6061-T6", "in_stock");

        // Mark the row DEALt — what the saga's CAS would write.
        conn.execute_batch(S272_MIGRATION_BACKSTOP).unwrap();
        conn.execute(
            "UPDATE quote_intake_log
                SET deal_issued_at = TIMESTAMP '2026-06-06 12:00:00',
                    deal_sales_order_id = 'so_TEST',
                    deal_work_order_id = 'wo_TEST'
              WHERE quote_id = 'q-dealt' AND tenant_id = 't1'",
            [],
        )
        .unwrap();

        // Now downgrade the catalogue. Pre-F6 this would flip
        // stock_alert TRUE on the dealt row + queue an audit entry.
        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'special_order'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();

        let listing = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(listing.rows.len(), 1);
        assert!(
            !listing.rows[0].stock_alert,
            "dealt row stays un-flipped: recompute is a no-op post-DEAL"
        );
        assert!(
            listing.newly_triggered_alerts.is_empty(),
            "no audit entry queued for a dealt row's catalogue downgrade"
        );
    }

    /// S275 / F6 — a picked-up row (operator already minted a draft from
    /// the quote) also skips the recompute. The DEAL gate isn't the
    /// operator's next action; alerting on the row doesn't route anywhere.
    #[test]
    fn s275_recompute_skips_picked_up_rows() {
        let conn = open_mem();
        seed_material(&conn, "t1", "6061-T6", "in_stock");
        insert_accepted_quote(&conn, "t1", "q-picked", "6061-T6", "in_stock");
        // Lazy schema backstops only land when `list_quote_intake_rows`
        // runs; apply S255 manually before the test UPDATE writes the
        // column.
        conn.execute_batch(S255_MIGRATION_BACKSTOP).unwrap();
        conn.execute(
            "UPDATE quote_intake_log
                SET picked_up_drf_id = 'drf_TEST'
              WHERE quote_id = 'q-picked' AND tenant_id = 't1'",
            [],
        )
        .unwrap();

        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'special_order'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();

        let listing = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(!listing.rows[0].stock_alert);
        assert!(listing.newly_triggered_alerts.is_empty());
    }

    /// S275 / F6 — `error`-state rows are skipped too: they aren't
    /// pickable and the operator's next action is retry/dismiss, not DEAL.
    #[test]
    fn s275_recompute_skips_error_rows() {
        let conn = open_mem();
        seed_material(&conn, "t1", "6061-T6", "in_stock");
        insert_accepted_quote(&conn, "t1", "q-err", "6061-T6", "in_stock");
        // Lazy schema backstops land inside `list_quote_intake_rows`;
        // apply S256 manually so the UPDATE finds `intake_state` /
        // `intake_error` columns.
        conn.execute_batch(S256_MIGRATION_BACKSTOP).unwrap();
        conn.execute(
            "UPDATE quote_intake_log
                SET intake_state = 'error', intake_error = 'bad'
              WHERE quote_id = 'q-err' AND tenant_id = 't1'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'special_order'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();
        let listing = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(!listing.rows[0].stock_alert);
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

    // ── S325 / PR-25 — read-side PDF re-render enqueue ────────────────

    fn test_tenant() -> aberp_audit_ledger::TenantId {
        aberp_audit_ledger::TenantId::new("t1").unwrap()
    }

    fn audit_count(conn: &Connection, kind: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM audit_ledger WHERE kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// A FALSE → TRUE stock_alert transition, once persisted, enqueues the
    /// quote-id into the PDF re-render queue AND emits exactly one
    /// `quote.pdf_rerender_enqueued` audit row alongside the existing
    /// `quote.stock_alert_triggered` row.
    #[test]
    fn s325_recompute_false_to_true_enqueues_pdf_rerender() {
        let mut conn = open_mem();
        seed_material(&conn, "t1", "6061-T6", "in_stock");
        insert_accepted_quote(&conn, "t1", "q-alpha", "6061-T6", "in_stock");

        // Catalogue downgrades → recompute detects the transition.
        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'source_1_2d'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();
        let listing = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(listing.newly_triggered_alerts.len(), 1);

        let queue = QuotePdfRerenderQueue::new();
        let enqueued = persist_alerts_and_enqueue_rerender(
            &mut conn,
            &test_tenant(),
            aberp_audit_ledger::BinaryHash::from_bytes([0u8; 32]),
            "op",
            &listing.newly_triggered_alerts,
            &queue,
        )
        .unwrap();

        assert_eq!(enqueued, vec!["q-alpha".to_string()]);
        assert!(queue.contains("q-alpha"), "quote enqueued for re-render");
        assert_eq!(queue.len(), 1);
        assert_eq!(audit_count(&conn, "quote.stock_alert_triggered"), 1);
        assert_eq!(audit_count(&conn, "quote.pdf_rerender_enqueued"), 1);

        // The flip persisted, so a second list pass sees sticky-TRUE and
        // emits NO new transition → nothing further to enqueue.
        let listing2 = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(listing2.newly_triggered_alerts.is_empty());
        assert!(listing2.rows[0].stock_alert);
    }

    /// Seed a `posted` `quote_pricing_jobs` row carrying the closed-vocab
    /// `material_grade` — the authoritative source the S329 🔴1 projection
    /// reads to arm the stock-alert trigger.
    fn seed_posted_pricing_job(conn: &Connection, tenant: &str, quote_id: &str, grade: &str) {
        crate::quote_pricing_jobs::ensure_schema(conn).unwrap();
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, material_grade, quantity,
                cad_filename, cad_local_path, feature_graph_hash,
                feature_graph_json, breakdown_json, pdf_path,
                total_price_eur, valid_until_iso, attempt_n
             ) VALUES (?1, ?2, 'posted', 'now', 'now',
                'c@example.com', 'Cust', ?3, 5,
                'part.stl', '/tmp/part.stl', 'blake3:abc',
                '{}', '{}', '/tmp/priced.pdf', 21.0, '2026-07-06', 0)",
            params![quote_id, tenant, grade],
        )
        .unwrap();
    }

    /// S329 / 🔴1 — the production writer for `stock_status_at_accept`.
    /// Before this fix every writer lived inside `#[cfg(test)]`, so the
    /// column was always NULL in prod and the customer-banner arc was dead.
    ///
    /// Phase 1 — a staged intake row (no `material_grade`, no snapshot) +
    /// a matching `posted` pricing job + an `in_stock` catalogue: the
    /// list-route projection MUST populate `material_grade` and snapshot
    /// `stock_status_at_accept = 'in_stock'` (write-once).
    ///
    /// Phase 2 — the snapshot now armed, a catalogue downgrade MUST drive
    /// `recompute_stock_alert` to a real FALSE→TRUE flip end-to-end —
    /// proving the arc fires, not just that one column is written.
    #[test]
    fn s329_stock_status_at_accept_snapshot_persisted_on_approval() {
        let conn = open_mem();
        seed_material(&conn, "t1", "6061-T6", "in_stock");
        // A plain staged intake row: NO material_grade, NO snapshot. This
        // is exactly the prod shape — the intake daemon writes only the
        // base columns.
        insert_test_row(
            &conn,
            "t1",
            "q-acc",
            "2026-01-01T00:00:00Z",
            "{}",
            "{}",
            None,
        );
        seed_posted_pricing_job(&conn, "t1", "q-acc", "6061-T6");

        // First list pass runs the projection.
        let listing0 = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(listing0.rows.len(), 1);
        assert_eq!(
            listing0.rows[0].material_grade.as_deref(),
            Some("6061-T6"),
            "material_grade projected from the posted pricing job"
        );
        assert_eq!(
            listing0.rows[0].stock_status_at_accept.as_deref(),
            Some("in_stock"),
            "stock_status_at_accept snapshotted from the live catalogue"
        );
        assert!(!listing0.rows[0].stock_alert, "no downgrade yet → no alert");
        assert!(listing0.newly_triggered_alerts.is_empty());

        // The snapshot is write-once: a catalogue change does NOT move it.
        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'source_1_2d'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();

        // Phase 2 — the armed trigger now flips FALSE→TRUE end-to-end.
        let listing1 = list_quote_intake_rows(&conn, "t1").unwrap();
        assert_eq!(
            listing1.rows[0].stock_status_at_accept.as_deref(),
            Some("in_stock"),
            "snapshot is write-once: still the at-accept value"
        );
        assert!(
            listing1.rows[0].stock_alert,
            "downgrade vs the snapshot arms the customer banner"
        );
        assert_eq!(listing1.newly_triggered_alerts.len(), 1);
        assert_eq!(
            listing1.newly_triggered_alerts[0].snapshot_status,
            "in_stock"
        );
        assert_eq!(
            listing1.newly_triggered_alerts[0].current_status,
            "source_1_2d"
        );
    }

    /// A pricing job that has NOT reached `posted` (artifacts not yet
    /// rendered) must NOT arm the snapshot — otherwise a flip could
    /// enqueue a re-render the daemon would fail as `artifacts_missing`.
    #[test]
    fn s329_snapshot_skips_pricing_jobs_before_posted() {
        let conn = open_mem();
        seed_material(&conn, "t1", "6061-T6", "in_stock");
        insert_test_row(
            &conn,
            "t1",
            "q-early",
            "2026-01-01T00:00:00Z",
            "{}",
            "{}",
            None,
        );
        crate::quote_pricing_jobs::ensure_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, material_grade, quantity,
                cad_filename, cad_local_path, attempt_n
             ) VALUES ('q-early', 't1', 'pricing', 'now', 'now',
                'c@example.com', 'Cust', '6061-T6', 5,
                'part.stl', '/tmp/part.stl', 0)",
            [],
        )
        .unwrap();
        let listing = list_quote_intake_rows(&conn, "t1").unwrap();
        assert!(
            listing.rows[0].stock_status_at_accept.is_none(),
            "pre-posted job does not arm the snapshot"
        );
    }

    /// Idempotency: re-running the persist with the SAME alerts after the
    /// flip already landed does NOT double-enqueue (the CAS UPDATE matches
    /// zero rows on the second call → lost-race branch) AND the queue's
    /// HashSet collapses a duplicate id to one. No second post can result.
    #[test]
    fn s325_recompute_idempotent_enqueue_no_double_post() {
        let mut conn = open_mem();
        seed_material(&conn, "t1", "6061-T6", "in_stock");
        insert_accepted_quote(&conn, "t1", "q-beta", "6061-T6", "in_stock");
        conn.execute(
            "UPDATE quoting_materials SET stock_status = 'special_order'
              WHERE grade = '6061-T6' AND tenant_id = 't1'",
            [],
        )
        .unwrap();
        let alerts = list_quote_intake_rows(&conn, "t1")
            .unwrap()
            .newly_triggered_alerts;
        assert_eq!(alerts.len(), 1);

        let queue = QuotePdfRerenderQueue::new();
        let tenant = test_tenant();
        let bh = aberp_audit_ledger::BinaryHash::from_bytes([0u8; 32]);

        // First persist: flips + enqueues.
        let first =
            persist_alerts_and_enqueue_rerender(&mut conn, &tenant, bh, "op", &alerts, &queue)
                .unwrap();
        assert_eq!(first.len(), 1);
        // Second persist with the SAME (now stale) alert: the row is
        // already TRUE so the guarded UPDATE matches 0 rows → no enqueue,
        // no second audit row.
        let second =
            persist_alerts_and_enqueue_rerender(&mut conn, &tenant, bh, "op", &alerts, &queue)
                .unwrap();
        assert!(second.is_empty(), "stale re-persist must not re-enqueue");
        assert_eq!(queue.len(), 1, "queue holds a single entry for the id");
        assert_eq!(audit_count(&conn, "quote.pdf_rerender_enqueued"), 1);

        // Belt-and-suspenders: even a direct double-enqueue of the same
        // id collapses to one (HashSet idempotency).
        queue.enqueue("q-beta");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.drain().len(), 1, "single post on drain");
    }
}
