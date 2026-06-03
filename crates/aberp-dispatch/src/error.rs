//! Typed errors for the Dispatch crate. Mirrors the
//! `aberp_qa::QaError` / `aberp_work_orders::WorkOrderError` shapes.

use thiserror::Error;

use crate::state::DispatchStateError;

#[derive(Debug, Error)]
pub enum DispatchError {
    /// Illegal dispatch-state transition per ADR-0064 §1 + the
    /// `state::next_dispatch_state` table. Route layer → 400.
    #[error("illegal dispatch transition: {0}")]
    IllegalTransition(String),

    /// The `dsp_id` does not exist in the tenant. Route layer → 404.
    #[error("dispatch {0} not found")]
    DispatchNotFound(String),

    /// `create_dispatch` refused because the referenced WO is not in
    /// the Completed state. ADR-0064 §2 #1 — eligibility gate.
    /// Route layer → 400.
    #[error("work order {wo_id} is not eligible for dispatch (state={state:?})")]
    WorkOrderNotEligible { wo_id: String, state: String },

    /// `create_dispatch` refused because the referenced WO already has
    /// a dispatch row (Drafted, Shipped, or Cancelled). ADR-0064 §2 #2
    /// — one dispatch per WO in v1. Route layer → 400.
    #[error("work order {wo_id} already has a dispatch (dsp_id={dsp_id})")]
    WorkOrderAlreadyDispatched { wo_id: String, dsp_id: String },

    /// The referenced WO does not exist in the tenant. Route layer → 404.
    #[error("work order {0} not found")]
    WorkOrderNotFound(String),

    /// The referenced partner does not exist in the tenant. Route
    /// layer → 404. Pinned by the `mark_shipped` validations before
    /// any stock-movement write — partner-lookup failure must roll
    /// back the whole tx per ADR-0064 §5 ("Failure handling") and
    /// invariant #6.
    #[error("partner {0} not found")]
    PartnerNotFound(String),

    /// Duplicate F8 idempotency key. Route layer → 409.
    #[error("duplicate idempotency_key {0}")]
    DuplicateIdempotencyKey(String),

    /// Validation error at the create / mark_shipped boundary. Route
    /// layer → 400.
    #[error("validation error: {0}")]
    Validation(String),

    /// The injected `InvoiceSpawner` returned an error during
    /// `mark_shipped`. Per ADR-0064 §5 + invariant #6 this rolls back
    /// the ENTIRE `mark_shipped` transaction — no dispatch state flip,
    /// no stock movement, no audit entry. Route layer → 500 (or 400
    /// if the spawner classifies as caller-error; the variant carries
    /// the underlying message so the route can render it inline).
    #[error("invoice spawn failed: {0}")]
    InvoiceSpawnFailed(String),

    /// DB-layer error from DuckDB or the audit-ledger / inventory
    /// crate write. Route layer → 500.
    #[error("storage error: {0}")]
    Storage(#[from] anyhow::Error),
}

impl From<DispatchStateError> for DispatchError {
    fn from(e: DispatchStateError) -> Self {
        DispatchError::IllegalTransition(format!("{e}"))
    }
}
