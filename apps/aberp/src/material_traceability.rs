//! S432 (ADR-0085) — operator-facing material chain-of-custody report.
//!
//! Given a `material_id` (= `quoting_materials` / `inventory_balances`
//! material_grade) OR a `heat_lot_number`, assemble what the install HAS
//! recorded about that lot's traceability:
//!
//!   * the material balance row (grade, heat lot, MTR URL, who/when assigned),
//!   * the auto-quotes that priced that grade (`quote_pricing_jobs`),
//!   * the Work Orders that originated from those quotes
//!     (`work_orders.source_quote_id`).
//!
//! ## What is sparse (by design)
//!
//! WO → physical material consumption is NOT fully wired (the DEAL saga books a
//! per-grade reservation; per-WO lot consumption is a later slice). So the WO
//! linkage is *via the originating quote's grade*, not a hard consumption
//! record, and the downstream invoice chain is not tracked at all. Those fields
//! render as `(not tracked yet)` placeholders rather than being omitted — when
//! an auditor pulls this report, the absence must be VISIBLE, not silent
//! (CLAUDE.md rule 12).
//!
//! The query itself fires `material.traceability_viewed` (who inspected the
//! chain, when) — the AS9100D §8.5.2 record-of-access anchor. Audit emission is
//! at the route layer (mirrors `material_inventory::append_heat_lot_events`).

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::material_inventory::{ensure_schema as ensure_inventory_schema, read_balance, Balance};

/// How the operator addressed the lot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceQueryKind {
    /// Look up by material grade directly.
    MaterialId,
    /// Look up by the assigned heat lot number.
    HeatLot,
}

impl TraceQueryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TraceQueryKind::MaterialId => "material_id",
            TraceQueryKind::HeatLot => "heat_lot",
        }
    }
}

/// One quote that priced the traced grade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteRef {
    pub quote_id: String,
    pub state: String,
}

/// One Work Order whose originating quote priced the traced grade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkOrderRef {
    pub wo_id: String,
    pub wo_number: String,
    pub state: String,
    pub source_quote_id: String,
}

/// The chain-of-custody report. `material` is `None` when nothing resolves to
/// the query (unknown grade / unassigned heat lot).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceReport {
    pub query_kind: TraceQueryKind,
    pub query_value: String,
    /// The resolved grade, when the query matched a balance row.
    pub material_id: Option<String>,
    /// The full balance row (carries heat lot + MTR + assignment metadata).
    pub material: Option<Balance>,
    pub quotes: Vec<QuoteRef>,
    pub work_orders: Vec<WorkOrderRef>,
    /// S432 v1 — downstream invoice traceability is NOT recorded. Always empty;
    /// the SPA renders the `invoices_note` placeholder so the gap is visible.
    pub invoices: Vec<String>,
    /// Operator-facing note naming what is not yet tracked.
    pub invoices_note: &'static str,
}

const NOT_TRACKED: &str = "(not tracked yet)";

/// Resolve the grade the query addresses. For [`TraceQueryKind::MaterialId`]
/// the value IS the grade (when a balance row exists). For
/// [`TraceQueryKind::HeatLot`] we scan `inventory_balances` for the row whose
/// `heat_lot_number` matches.
fn resolve_grade(
    conn: &Connection,
    tenant: &str,
    kind: TraceQueryKind,
    value: &str,
) -> Result<Option<String>> {
    match kind {
        TraceQueryKind::MaterialId => {
            // Confirm the grade actually has a balance row; otherwise no match.
            Ok(read_balance(conn, tenant, value)?.map(|_| value.to_string()))
        }
        TraceQueryKind::HeatLot => {
            let mut stmt = conn
                .prepare(
                    "SELECT material_grade FROM inventory_balances
                      WHERE tenant_id = ?1 AND heat_lot_number = ?2
                      ORDER BY material_grade LIMIT 1",
                )
                .context("prepare resolve_grade by heat lot")?;
            let mut rows = stmt
                .query(params![tenant, value])
                .context("query resolve_grade by heat lot")?;
            match rows.next().context("read resolve_grade row")? {
                Some(r) => Ok(Some(r.get::<_, String>(0).context("get material_grade")?)),
                None => Ok(None),
            }
        }
    }
}

/// Assemble the traceability report. Read-only; no audit (route layer fires
/// `material.traceability_viewed`). Unit-testable end to end.
pub fn trace(
    conn: &Connection,
    tenant: &str,
    kind: TraceQueryKind,
    value: &str,
) -> Result<TraceReport> {
    ensure_inventory_schema(conn)?;
    let value = value.trim().to_string();

    let grade = resolve_grade(conn, tenant, kind, &value)?;
    let material = match &grade {
        Some(g) => read_balance(conn, tenant, g)?,
        None => None,
    };

    let (quotes, work_orders) = match &grade {
        Some(g) => (
            quotes_for_grade(conn, tenant, g)?,
            work_orders_for_grade(conn, tenant, g)?,
        ),
        None => (Vec::new(), Vec::new()),
    };

    Ok(TraceReport {
        query_kind: kind,
        query_value: value,
        material_id: grade,
        material,
        quotes,
        work_orders,
        invoices: Vec::new(),
        invoices_note: NOT_TRACKED,
    })
}

/// Quotes (`quote_pricing_jobs`) that priced this grade.
fn quotes_for_grade(conn: &Connection, tenant: &str, grade: &str) -> Result<Vec<QuoteRef>> {
    let mut stmt = conn
        .prepare(
            "SELECT quote_id, state FROM quote_pricing_jobs
              WHERE tenant_id = ?1 AND material_grade = ?2
              ORDER BY quote_id",
        )
        .context("prepare quotes_for_grade")?;
    let rows = stmt
        .query_map(params![tenant, grade], |r| {
            Ok(QuoteRef {
                quote_id: r.get::<_, String>(0)?,
                state: r.get::<_, String>(1)?,
            })
        })
        .context("query quotes_for_grade")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read quote row")?);
    }
    Ok(out)
}

/// Work Orders whose originating quote priced this grade (the sparse linkage —
/// per-WO physical consumption is not yet recorded).
fn work_orders_for_grade(
    conn: &Connection,
    tenant: &str,
    grade: &str,
) -> Result<Vec<WorkOrderRef>> {
    let mut stmt = conn
        .prepare(
            "SELECT wo.wo_id, wo.wo_number, wo.state, wo.source_quote_id
               FROM work_orders wo
               JOIN quote_pricing_jobs q
                 ON q.quote_id = wo.source_quote_id AND q.tenant_id = wo.tenant_id
              WHERE wo.tenant_id = ?1 AND q.material_grade = ?2
              ORDER BY wo.wo_id",
        )
        .context("prepare work_orders_for_grade")?;
    let rows = stmt
        .query_map(params![tenant, grade], |r| {
            Ok(WorkOrderRef {
                wo_id: r.get::<_, String>(0)?,
                wo_number: r.get::<_, String>(1)?,
                state: r.get::<_, String>(2)?,
                source_quote_id: r.get::<_, String>(3)?,
            })
        })
        .context("query work_orders_for_grade")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read wo row")?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material_inventory::assign_heat_lot;
    use aberp_audit_ledger::ensure_schema as audit_ensure_schema;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        audit_ensure_schema(&conn).unwrap();
        ensure_inventory_schema(&conn).unwrap();
        aberp_work_orders::ensure_schema(&conn).unwrap();
        crate::quote_pricing_jobs::ensure_schema(&conn).unwrap();
        conn
    }

    fn seed_balance(conn: &Connection, grade: &str) {
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('t', ?1, 100.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
            params![grade],
        )
        .unwrap();
    }

    fn seed_quote(conn: &Connection, quote_id: &str, grade: &str) {
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, customer_company, material_grade, quantity,
                cad_filename, cad_local_path, attempt_n
             ) VALUES (?1, 't', 'priced', '2026-06-06T00:00:00Z', '2026-06-06T00:00:00Z',
                       'a@b.c', 'N', 'Co', ?2, 1, 'p.stl', '/tmp/p.stl', 1)",
            params![quote_id, grade],
        )
        .unwrap();
    }

    fn seed_wo(conn: &Connection, wo_id: &str, source_quote_id: &str) {
        conn.execute(
            "INSERT INTO work_orders (
                wo_id, tenant_id, wo_number, product_id, qty_target, state,
                created_at, source_quote_id
             ) VALUES (?1, 't', 'WO-1', 'prd_1', '1', 'created', '2026-06-06T00:00:00Z', ?2)",
            params![wo_id, source_quote_id],
        )
        .unwrap();
    }

    #[test]
    fn trace_by_material_id_returns_linked_quotes_and_wos() {
        let conn = open_conn();
        seed_balance(&conn, "Ti-6Al-4V");
        assign_heat_lot(&conn, "t", "Ti-6Al-4V", "HEAT-1", "", "op").unwrap();
        seed_quote(&conn, "q1", "Ti-6Al-4V");
        seed_wo(&conn, "wo1", "q1");

        let rep = trace(&conn, "t", TraceQueryKind::MaterialId, "Ti-6Al-4V").unwrap();
        assert_eq!(rep.material_id.as_deref(), Some("Ti-6Al-4V"));
        assert_eq!(
            rep.material.unwrap().heat_lot_number.as_deref(),
            Some("HEAT-1")
        );
        assert_eq!(rep.quotes.len(), 1);
        assert_eq!(rep.quotes[0].quote_id, "q1");
        assert_eq!(rep.work_orders.len(), 1);
        assert_eq!(rep.work_orders[0].wo_id, "wo1");
        // Sparse fields surfaced, not omitted.
        assert!(rep.invoices.is_empty());
        assert_eq!(rep.invoices_note, "(not tracked yet)");
    }

    #[test]
    fn trace_by_heat_lot_resolves_grade() {
        let conn = open_conn();
        seed_balance(&conn, "304");
        assign_heat_lot(&conn, "t", "304", "HEAT-XYZ", "", "op").unwrap();
        let rep = trace(&conn, "t", TraceQueryKind::HeatLot, "HEAT-XYZ").unwrap();
        assert_eq!(rep.material_id.as_deref(), Some("304"));
    }

    #[test]
    fn trace_unknown_value_returns_empty_with_placeholders() {
        let conn = open_conn();
        let rep = trace(&conn, "t", TraceQueryKind::MaterialId, "NOPE").unwrap();
        assert_eq!(rep.material_id, None);
        assert!(rep.material.is_none());
        assert!(rep.quotes.is_empty());
        assert!(rep.work_orders.is_empty());
        assert_eq!(rep.invoices_note, "(not tracked yet)");
    }
}
