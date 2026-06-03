//! Typed errors for the QA crate. Mirrors the
//! `aberp_work_orders::WorkOrderError` shape.

use thiserror::Error;

use crate::state::QaStateError;
use crate::types::QaState;

#[derive(Debug, Error)]
pub enum QaError {
    /// Illegal QA-state transition per ADR-0063 §1 + the
    /// `state::next_qa_state` table. Route layer → 400.
    #[error("illegal QA transition: {0}")]
    IllegalTransition(String),

    /// The `qa_id` does not exist in the tenant. Route layer → 404.
    #[error("QA inspection {0} not found")]
    InspectionNotFound(String),

    /// The QA inspection has already been superseded by a later
    /// cross-actor decision (per ADR-0063 §4); no further decisions
    /// can mutate it — operator must look up the live row and decide
    /// against THAT id. Route layer → 409.
    #[error("QA inspection {0} has been superseded; decide against the live row")]
    AlreadySuperseded(String),

    /// The auto-create path was called against a routing-op that
    /// already has a non-superseded inspection AND the actor differs.
    /// Should never happen in v1 (auto-create only runs from
    /// `transition_routing_op`); kept for defence-in-depth.
    #[error("QA inspection already live for routing-op {0}")]
    InspectionAlreadyLive(String),

    /// Optimistic-concurrency loss — the row's current state changed
    /// under the caller (two operators raced; or the adapter wrote
    /// between our SELECT and our UPDATE). Route layer → 409.
    #[error("QA state raced: expected from={expected:?}, found={actual:?}")]
    StateRaced { expected: QaState, actual: QaState },

    /// Duplicate F8 idempotency key. Route layer → 409.
    #[error("duplicate idempotency_key {0}")]
    DuplicateIdempotencyKey(String),

    /// Validation error at the create / decide boundary. Route → 400.
    #[error("validation error: {0}")]
    Validation(String),

    /// DB-layer error from DuckDB or the audit-ledger / inventory
    /// crate write. Route → 500.
    #[error("storage error: {0}")]
    Storage(#[from] anyhow::Error),
}

impl From<QaStateError> for QaError {
    fn from(e: QaStateError) -> Self {
        QaError::IllegalTransition(format!("{e}"))
    }
}
