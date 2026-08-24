//! ADR-0199 §D3(a) — `part_drawing_refs`: drawing number + revision,
//! with history.
//!
//! ## Why this table exists at all
//!
//! ADR-0199 §C2 #3 established, over four independent sweeps, that **no
//! drawing number and no drawing revision exist anywhere in the repo**.
//! `work_orders` carries `product_id` and nothing else identifying a
//! drawing. AS9102 Form 1 fields 6-7 therefore cannot be filled from
//! today's data, and neither can the header of the per-shipment
//! dimensional report. This is the smallest table that closes that gap.
//!
//! ## Why revisions are superseded, not overwritten
//!
//! A report issued in 2026 must still name the revision it was inspected
//! against when an auditor reads it in 2033. Overwriting `drawing_rev`
//! in place would rewrite the meaning of every report that ever cited it.
//! So [`supersede_and_create`] closes the current row
//! (`superseded_at = now`) and inserts a new one; nothing is ever
//! deleted or edited in place. The report itself additionally SNAPSHOTS
//! the number + rev onto `qc_reports` at issuance, so even this table
//! going wrong later cannot change an issued document.
//!
//! ## Where the uniqueness invariant lives
//!
//! "At most one current (non-superseded) revision per
//! `(tenant, product_id)`" is enforced HERE, in code, not by a SQL
//! `UNIQUE` — [[no-sql-specific]], matching `qc::plans`' own posture and
//! V002's stated migration discipline.

use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use super::error::QcError;

/// One `part_drawing_refs` row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartDrawingRef {
    /// `pdr_<ULID>`.
    pub drawing_ref_id: String,
    pub product_id: String,
    pub drawing_number: String,
    pub drawing_rev: String,
    /// RFC3339. When this revision became effective.
    pub effective_from: String,
    /// RFC3339, or `None` for the CURRENT revision.
    pub superseded_at: Option<String>,
    pub created_at: String,
    pub created_by: String,
}

impl PartDrawingRef {
    /// `true` iff this is the current (non-superseded) revision.
    pub fn is_current(&self) -> bool {
        self.superseded_at.is_none()
    }
}

/// Operator-supplied inputs. The id and the timestamps are minted here.
#[derive(Debug, Clone, Deserialize)]
pub struct NewPartDrawingRef {
    pub product_id: String,
    pub drawing_number: String,
    pub drawing_rev: String,
}

fn rfc3339(ts: OffsetDateTime) -> Result<String, QcError> {
    ts.format(&Rfc3339)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("format timestamp: {e}")))
}

fn validate(input: &NewPartDrawingRef) -> Result<(), QcError> {
    if input.product_id.trim().is_empty() {
        return Err(QcError::Validation("product_id is required".into()));
    }
    if input.drawing_number.trim().is_empty() {
        return Err(QcError::Validation("drawing_number is required".into()));
    }
    if input.drawing_rev.trim().is_empty() {
        // A blank revision on an aerospace drawing is not a small
        // omission — it makes the report's "inspected against rev X"
        // claim unfalsifiable. Refuse at entry (CLAUDE.md rule 12).
        return Err(QcError::Validation(
            "drawing_rev is required (a drawing with no revision cannot be cited on a QC report)"
                .into(),
        ));
    }
    Ok(())
}

/// Record a new current revision for a product, superseding whatever was
/// current. Idempotent in the honest sense: re-recording the SAME
/// `(drawing_number, drawing_rev)` that is already current is a no-op
/// returning the existing row, so an operator's double-click does not
/// manufacture a spurious revision event in the history.
///
/// `now` is passed in (not read from a clock here) so the whole module
/// is deterministic and testable.
///
/// **ONE transaction.** The close (`UPDATE … superseded_at`) and the open
/// (`INSERT`) used to run as two bare statements: a failure between them
/// left the product with ZERO current revisions, and
/// [`current_for_product`] then returned `None` — so every subsequent
/// report froze a blank drawing number and blank revision, silently, on a
/// compliance document whose whole job is to name the revision it was
/// inspected against. Both or neither.
pub fn supersede_and_create(
    conn: &mut Connection,
    tenant: &str,
    input: NewPartDrawingRef,
    created_by: &str,
    now: OffsetDateTime,
) -> Result<PartDrawingRef, QcError> {
    validate(&input)?;
    let product_id = input.product_id.trim();
    let drawing_number = input.drawing_number.trim();
    let drawing_rev = input.drawing_rev.trim();

    let tx = conn
        .transaction()
        .map_err(|e| QcError::Storage(anyhow::anyhow!("begin drawing-ref tx: {e}")))?;

    if let Some(current) = current_for_product(&tx, tenant, product_id)? {
        if current.drawing_number == drawing_number && current.drawing_rev == drawing_rev {
            return Ok(current);
        }
        let stamp = rfc3339(now)?;
        tx.execute(
            "UPDATE part_drawing_refs SET superseded_at = ?
             WHERE tenant_id = ? AND drawing_ref_id = ?;",
            params![&stamp, tenant, &current.drawing_ref_id],
        )
        .map_err(|e| QcError::Storage(anyhow::anyhow!("supersede part_drawing_refs: {e}")))?;
    }

    let drawing_ref_id = format!("pdr_{}", Ulid::new());
    let stamp = rfc3339(now)?;
    tx.execute(
        "INSERT INTO part_drawing_refs (
            drawing_ref_id, tenant_id, product_id, drawing_number, drawing_rev,
            effective_from, superseded_at, created_at, created_by
         ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?);",
        params![
            &drawing_ref_id,
            tenant,
            product_id,
            drawing_number,
            drawing_rev,
            &stamp,
            &stamp,
            created_by.trim(),
        ],
    )
    .map_err(|e| QcError::Storage(anyhow::anyhow!("INSERT part_drawing_refs: {e}")))?;

    let row = get(&tx, tenant, &drawing_ref_id)?
        .ok_or_else(|| QcError::Storage(anyhow::anyhow!("drawing ref vanished after insert")))?;
    tx.commit()
        .map_err(|e| QcError::Storage(anyhow::anyhow!("commit drawing-ref tx: {e}")))?;
    Ok(row)
}

/// The CURRENT revision for a product, or `None` when the product has no
/// drawing recorded. `None` is a legitimate state — the report renders a
/// blank drawing block rather than inventing a number.
///
/// If more than one non-superseded row somehow exists (a corrupted DB, or
/// a future writer that bypassed [`supersede_and_create`]), this fails
/// LOUD rather than picking one: silently choosing a revision would put a
/// wrong drawing rev on a compliance document.
pub fn current_for_product(
    conn: &Connection,
    tenant: &str,
    product_id: &str,
) -> Result<Option<PartDrawingRef>, QcError> {
    let rows = query(
        conn,
        "WHERE tenant_id = ? AND product_id = ? AND superseded_at IS NULL
         ORDER BY effective_from DESC, drawing_ref_id DESC",
        params![tenant, product_id.trim()],
    )?;
    match rows.len() {
        0 => Ok(None),
        1 => Ok(rows.into_iter().next()),
        n => Err(QcError::Validation(format!(
            "{n} current drawing revisions for product {product_id} — the \
             one-current-revision invariant is broken; supersede the extras \
             before issuing a QC report"
        ))),
    }
}

/// Full revision history for a product, newest first. Current row (if
/// any) sorts first because `superseded_at IS NULL` orders last under
/// DuckDB's NULLS LAST default on a DESC sort — so the ordering is stated
/// explicitly on `effective_from` instead.
pub fn list_for_product(
    conn: &Connection,
    tenant: &str,
    product_id: &str,
) -> Result<Vec<PartDrawingRef>, QcError> {
    query(
        conn,
        "WHERE tenant_id = ? AND product_id = ?
         ORDER BY effective_from DESC, drawing_ref_id DESC",
        params![tenant, product_id.trim()],
    )
}

/// Fetch one row by id (tenant-scoped).
pub fn get(
    conn: &Connection,
    tenant: &str,
    drawing_ref_id: &str,
) -> Result<Option<PartDrawingRef>, QcError> {
    Ok(query(
        conn,
        "WHERE tenant_id = ? AND drawing_ref_id = ?",
        params![tenant, drawing_ref_id],
    )?
    .into_iter()
    .next())
}

fn query(
    conn: &Connection,
    where_order: &str,
    p: impl duckdb::Params,
) -> Result<Vec<PartDrawingRef>, QcError> {
    let sql = format!(
        "SELECT drawing_ref_id, product_id, drawing_number, drawing_rev,
                effective_from, superseded_at, created_at, created_by
         FROM part_drawing_refs {where_order};"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("prepare drawing-ref query: {e}")))?;
    let rows = stmt
        .query_map(p, |row| {
            Ok(PartDrawingRef {
                drawing_ref_id: row.get(0)?,
                product_id: row.get(1)?,
                drawing_number: row.get(2)?,
                drawing_rev: row.get(3)?,
                effective_from: row.get(4)?,
                superseded_at: row.get(5)?,
                created_at: row.get(6)?,
                created_by: row.get(7)?,
            })
        })
        .map_err(|e| QcError::Storage(anyhow::anyhow!("query drawing refs: {e}")))?;
    let mut acc = Vec::new();
    for r in rows {
        acc.push(r.map_err(|e| QcError::Storage(anyhow::anyhow!("read drawing-ref row: {e}")))?);
    }
    Ok(acc)
}
