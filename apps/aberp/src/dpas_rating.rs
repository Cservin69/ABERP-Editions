//! D-10 (S361 / ADR-0078) — DPAS priority-rating assignment + audit trail.
//!
//! Records which 15 CFR § 700 / FAR 11.6 DPAS rating (`DO`/`DX` + program
//! symbol, e.g. `"DO-A1"`) a supplier is approved to service, and fires
//! [`EventKind::SupplierDpasPrioritySet`]. The rating is validated + rendered
//! through [`aberp_compliance::avl::DpasRating`] so a free-text rating can
//! never reach the ledger or the `partners.dpas_rating` column. The append
//! rides the caller's tx on the shared `aberp_db::Handle` (ADR-0099).
//!
//! No PII at rest: the payload records WHICH partner was rated and WHO rated it
//! (opaque operator id).

use anyhow::{Context, Result};
use duckdb::Transaction;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};

/// Outcome of assigning a rating, echoed back to the operator. `dpas_rating`
/// is the canonical rendered form; `previous_rating` is what the supplier
/// carried before (for the operator's confirmation UX — NOT part of the audit
/// payload, which records the new assignment only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpasRatingOutcome {
    pub partner_id: String,
    pub dpas_rating: String,
    pub previous_rating: Option<String>,
}

/// A bad rating string — operator-fixable, so the serve layer maps it to 400.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DpasRatingError {
    #[error("invalid DPAS rating {value:?}: {reason}")]
    Invalid { value: String, reason: String },
}

/// Append `supplier.dpas_priority_set` inside the caller's tx. `dpas_rating`
/// is the already-rendered `DpasRating::as_str()` form.
pub fn append_dpas_priority_set_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    partner_id: &str,
    dpas_rating: &str,
    operator: &str,
    set_at_ms: i64,
) -> Result<()> {
    let payload = serde_json::json!({
        "partner_id": partner_id,
        "dpas_rating": dpas_rating,
        "operator_user_id": operator,
        "set_at_ms": set_at_ms,
    });
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::SupplierDpasPrioritySet,
        serde_json::to_vec(&payload).expect("serialize supplier.dpas_priority_set"),
        ledger_actor,
        Some(format!("supplier_dpas:{partner_id}:{set_at_ms}")),
    )
    .context("audit append SupplierDpasPrioritySet")?;
    Ok(())
}
