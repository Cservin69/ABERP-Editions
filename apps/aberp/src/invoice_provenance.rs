//! ADR-0123 — invoice↔shipment provenance: the defense evidence seam.
//!
//! An outgoing `inv_*` invoice records which `dsp_*` shipment it bills, so an
//! auditor can walk invoice → dispatch → work order → part marks → QC reports
//! → export-control decisions. Before this, that walk was impossible in both
//! directions (ADR-0123 §Context, six verified sites).
//!
//! # Why a table here and not columns on `invoice`
//!
//! The obvious shape is three columns on `modules/billing`'s `invoice` table.
//! That was the design pass's plan and it is **wrong for this repository**:
//! `modules/billing` is shared, and the prod-invoice line is frozen. Ervin's
//! constraint on F1 was explicit — if the seam touches shared code, scope the
//! change to Defense.
//!
//! So the provenance is its own table in `apps/aberp`, keyed by invoice id,
//! exactly as `cui_markings` (`crate::cui_marking`) is its own table rather
//! than columns on `product`. Same reasoning: a Defense compliance concern
//! hangs off a generic entity without reshaping it. `modules/billing` is
//! untouched by ADR-0123.
//!
//! # Why this survives the draft's deletion
//!
//! Ervin's second requirement was that the reference must not be NULLed on
//! delete. `delete_draft_in_tx` (`crate::invoice_draft`) clears
//! `dispatches.spawned_invoice_id` and removes the `invoice_draft` row. It has
//! **no path to this table** — the reference lives on the invoice side, so the
//! property holds by construction rather than by discipline. (Since ADR-0123
//! §D5 the draft is state-flipped rather than deleted on promotion anyway, but
//! that is a second belt: an operator DELETE of a `Staged` draft must also not
//! disturb an invoice that never came from it.)
//!
//! # What writes it
//!
//! Nothing, in ADR-0123 slice 1. [`record_in_tx`] exists so the shape and the
//! round trip can be pinned; its caller is the promote route in slice 2, which
//! derives every field from the `invoice_draft` ROW rather than from a request
//! body (`[[trust-code-not-operator]]`).

use anyhow::{Context, Result};
use duckdb::{params, Connection, Transaction};

/// Defense-scoped provenance table (ADR-0123 §D2, as refined at build time).
///
/// Keyed by `(tenant_id, invoice_id)` — one shipment origin per invoice. The
/// dispatch is NOT unique here: a dispatch legitimately maps to more than one
/// invoice over time (an invoice stornoed and re-issued for the same shipment
/// is the case ADR-0123 §D5 exists for), so a `UNIQUE` on `source_dispatch_id`
/// would refuse an ordinary correction.
pub const INVOICE_SHIPMENT_PROVENANCE_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS invoice_shipment_provenance (
    tenant_id          VARCHAR NOT NULL,
    invoice_id         VARCHAR NOT NULL,
    source_draft_id    VARCHAR NOT NULL,
    source_dispatch_id VARCHAR,
    source_wo_id       VARCHAR,
    recorded_at_utc    VARCHAR NOT NULL,
    PRIMARY KEY (tenant_id, invoice_id)
);
CREATE INDEX IF NOT EXISTS invoice_shipment_provenance_dispatch_idx
    ON invoice_shipment_provenance (tenant_id, source_dispatch_id);
";

/// One invoice's shipment origin, as derived from the draft row it was
/// promoted from.
///
/// `source_dispatch_id` and `source_wo_id` are `Option` because
/// `invoice_draft` itself allows them to be NULL: a draft can be created from
/// a quote pickup rather than a dispatch (`crate::quote_pickup` passes
/// `source_dispatch_id: None`). A promoted quote-origin draft therefore
/// records its draft id and no dispatch — which is a faithful statement, not a
/// gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceShipmentProvenance {
    pub invoice_id: String,
    pub source_draft_id: String,
    pub source_dispatch_id: Option<String>,
    pub source_wo_id: Option<String>,
    pub recorded_at_utc: String,
}

/// Create the table if absent. No-op on a read-only connection, matching
/// `invoice_draft::ensure_schema`'s ADR-0098 C2 posture.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(INVOICE_SHIPMENT_PROVENANCE_SCHEMA_SQL)
        .context("create invoice_shipment_provenance schema")?;
    Ok(())
}

/// Record one invoice's provenance, in the CALLER's transaction.
///
/// Takes a `&Transaction` and never opens one: the promote route writes this
/// inside `run_single_tx`'s existing single transaction (ADR-0123 §D1), so an
/// invoice that commits without its provenance is unrepresentable. A function
/// that opened its own transaction would make that guarantee impossible to
/// state.
///
/// **No caller in slice 1.** See the module docs.
pub fn record_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    provenance: &InvoiceShipmentProvenance,
) -> Result<()> {
    tx.execute(
        "INSERT INTO invoice_shipment_provenance (
            tenant_id, invoice_id, source_draft_id, source_dispatch_id,
            source_wo_id, recorded_at_utc
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6);",
        params![
            tenant,
            &provenance.invoice_id,
            &provenance.source_draft_id,
            provenance.source_dispatch_id.as_deref(),
            provenance.source_wo_id.as_deref(),
            &provenance.recorded_at_utc,
        ],
    )
    .context("INSERT invoice_shipment_provenance")?;
    Ok(())
}

/// Read one invoice's provenance. `None` means this invoice has no recorded
/// shipment origin — which is the honest answer for every invoice issued
/// before ADR-0123, and for every invoice issued through the ordinary form
/// rather than the promote route.
///
/// It is deliberately NOT inferred from anything. ADR-0123 rejected
/// server-side inference (match on partner + product + qty) outright: a
/// heuristic join is wrong exactly when two similar shipments are in flight,
/// and a guess in an evidence trail is worse than an honest absence.
pub fn get_for_invoice(
    conn: &Connection,
    tenant: &str,
    invoice_id: &str,
) -> Result<Option<InvoiceShipmentProvenance>> {
    let mut stmt = conn
        .prepare(
            "SELECT invoice_id, source_draft_id, source_dispatch_id, source_wo_id,
                    recorded_at_utc
               FROM invoice_shipment_provenance
              WHERE tenant_id = ?1 AND invoice_id = ?2
              LIMIT 1;",
        )
        .context("prepare get_for_invoice")?;
    let mut rows = stmt
        .query(params![tenant, invoice_id])
        .context("query get_for_invoice")?;
    let Some(row) = rows.next().context("read get_for_invoice row")? else {
        return Ok(None);
    };
    Ok(Some(InvoiceShipmentProvenance {
        invoice_id: row.get(0).context("col invoice_id")?,
        source_draft_id: row.get(1).context("col source_draft_id")?,
        source_dispatch_id: row.get(2).context("col source_dispatch_id")?,
        source_wo_id: row.get(3).context("col source_wo_id")?,
        recorded_at_utc: row.get(4).context("col recorded_at_utc")?,
    }))
}

/// Every invoice recorded against one dispatch, newest first.
///
/// Plural on purpose: a dispatch maps to more than one invoice whenever an
/// invoice is stornoed and re-issued for the same shipment (ADR-0123 §D5). A
/// singular `get_invoice_for_dispatch` would have to pick one, and picking is
/// what makes an evidence answer wrong.
pub fn list_for_dispatch(
    conn: &Connection,
    tenant: &str,
    dispatch_id: &str,
) -> Result<Vec<InvoiceShipmentProvenance>> {
    let mut stmt = conn
        .prepare(
            "SELECT invoice_id, source_draft_id, source_dispatch_id, source_wo_id,
                    recorded_at_utc
               FROM invoice_shipment_provenance
              WHERE tenant_id = ?1 AND source_dispatch_id = ?2
              ORDER BY recorded_at_utc DESC, invoice_id DESC;",
        )
        .context("prepare list_for_dispatch")?;
    let rows = stmt
        .query_map(params![tenant, dispatch_id], |row| {
            Ok(InvoiceShipmentProvenance {
                invoice_id: row.get(0)?,
                source_draft_id: row.get(1)?,
                source_dispatch_id: row.get(2)?,
                source_wo_id: row.get(3)?,
                recorded_at_utc: row.get(4)?,
            })
        })
        .context("query list_for_dispatch")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read list_for_dispatch row")?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "prov-test";

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c).unwrap();
        c
    }

    fn sample(invoice: &str, dispatch: Option<&str>) -> InvoiceShipmentProvenance {
        InvoiceShipmentProvenance {
            invoice_id: invoice.to_string(),
            source_draft_id: "drf_1".to_string(),
            source_dispatch_id: dispatch.map(str::to_string),
            source_wo_id: Some("wo_1".to_string()),
            recorded_at_utc: "2026-09-11T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn a_recorded_provenance_round_trips() {
        let mut c = conn();
        let p = sample("inv_1", Some("dsp_1"));
        let tx = c.transaction().unwrap();
        record_in_tx(&tx, T, &p).unwrap();
        tx.commit().unwrap();
        assert_eq!(get_for_invoice(&c, T, "inv_1").unwrap(), Some(p));
    }

    /// An invoice with no recorded origin reads as `None` — never as an
    /// invented link. This is the state of every invoice issued before
    /// ADR-0123 and every one issued through the ordinary form.
    #[test]
    fn an_unknown_invoice_has_no_provenance_rather_than_a_guess() {
        let c = conn();
        assert_eq!(get_for_invoice(&c, T, "inv_missing").unwrap(), None);
    }

    /// A quote-origin draft has no dispatch, and that is recorded faithfully
    /// rather than as an absence of provenance.
    #[test]
    fn a_draft_with_no_dispatch_records_its_draft_id_and_a_null_dispatch() {
        let mut c = conn();
        let p = sample("inv_q", None);
        let tx = c.transaction().unwrap();
        record_in_tx(&tx, T, &p).unwrap();
        tx.commit().unwrap();
        let got = get_for_invoice(&c, T, "inv_q").unwrap().unwrap();
        assert_eq!(got.source_dispatch_id, None);
        assert_eq!(got.source_draft_id, "drf_1");
    }

    /// **The ADR-0123 §D5 shape.** One dispatch, two invoices — the storno +
    /// re-issue case. A schema that made `source_dispatch_id` unique would
    /// refuse the second, which is an ordinary correction, not an error.
    #[test]
    fn one_dispatch_may_carry_more_than_one_invoice() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        record_in_tx(&tx, T, &sample("inv_first", Some("dsp_1"))).unwrap();
        let mut second = sample("inv_second", Some("dsp_1"));
        second.recorded_at_utc = "2026-09-12T00:00:00Z".to_string();
        record_in_tx(&tx, T, &second).unwrap();
        tx.commit().unwrap();

        let all = list_for_dispatch(&c, T, "dsp_1").unwrap();
        assert_eq!(all.len(), 2, "storno + re-issue keeps BOTH links");
        assert_eq!(
            all[0].invoice_id, "inv_second",
            "newest first, so the current invoice leads"
        );
    }

    /// Tenancy is part of the key, not an afterthought: another tenant's
    /// invoice id must not resolve here.
    #[test]
    fn provenance_is_tenant_scoped() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        record_in_tx(&tx, T, &sample("inv_1", Some("dsp_1"))).unwrap();
        tx.commit().unwrap();
        assert_eq!(get_for_invoice(&c, "other-tenant", "inv_1").unwrap(), None);
        assert!(list_for_dispatch(&c, "other-tenant", "dsp_1")
            .unwrap()
            .is_empty());
    }

    /// `ensure_schema` is idempotent — it runs on every serve boot.
    #[test]
    fn ensure_schema_is_idempotent() {
        let c = conn();
        ensure_schema(&c).unwrap();
        ensure_schema(&c).unwrap();
    }
}
