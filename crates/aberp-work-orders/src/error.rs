//! Typed errors for the Work Orders crate. The route layer maps these
//! to HTTP status codes per the existing partners / products /
//! incoming_invoices / inventory pattern.

use thiserror::Error;

/// Errors surfaced by the work-orders repository.
#[derive(Debug, Error)]
pub enum WorkOrderError {
    /// Illegal state transition per ADR-0062 §2 / the
    /// `state::next_state` table. Surface at the route layer as
    /// 400 — the SPA refused to render the action button, but a curl
    /// that bypasses it still gets refused loud.
    #[error("illegal transition: {0}")]
    IllegalTransition(String),

    /// Optimistic-concurrency loss per ADR-0062 §"Adversarial review"
    /// (two operators clicked Complete on the same WO simultaneously).
    /// The handler reads the current state at the start of the tx; if
    /// it does not match the expected `from`, this fires. Route layer
    /// maps to 409 — operator sees "state changed under you, refresh."
    #[error("state raced: expected from={expected:?}, found={actual:?}")]
    StateRaced {
        expected: crate::types::WorkOrderState,
        actual: crate::types::WorkOrderState,
    },

    /// `Release` was requested but the product has no active BOM
    /// rows (every row's `retired_at IS NOT NULL`). Per ADR-0062 §5
    /// this is a structured refuse — without a BOM the Release
    /// handler has nothing to consume, which is almost certainly a
    /// data-error (or the operator forgot to author the BOM before
    /// the first WO). Route layer maps to 400.
    #[error("cannot release: product {0} has no active BOM")]
    NoActiveBomForProduct(String),

    /// The `wo_id` (or routing_op_id / bom_line_id) does not exist
    /// in the tenant. Route layer maps to 404.
    #[error("work order {0} not found")]
    WorkOrderNotFound(String),

    /// The product referenced (by a WO create or by a BOM author)
    /// does not exist in the tenant's `products` table. Route layer
    /// maps to 400 (the caller has a stale or typo'd product id).
    #[error("product {0} not found")]
    ProductNotFound(String),

    /// The `idempotency_key` already exists in the audit ledger for
    /// this WO operation. Route layer maps to 409 — the client
    /// retried a request that already landed.
    #[error("duplicate idempotency_key {0}")]
    DuplicateIdempotencyKey(String),

    /// Validation error at the create / author boundary (empty
    /// op_name, zero / negative qty_target, BOM size > cap, etc.).
    /// Route layer maps to 400.
    #[error("validation error: {0}")]
    Validation(String),

    /// The routing-op id does not exist in the tenant. Route
    /// layer → 404.
    #[error("routing op {0} not found")]
    RoutingOpNotFound(String),

    /// S233 / PR-229 — the WO Complete gate (ADR-0063 §7 + invariant
    /// #6) refused: at least one routing-op has no live `qa_inspections`
    /// row in state `Passed`. Carries a human-readable list of
    /// blocking op names so the SPA can render a helpful tooltip.
    /// Route layer maps to 400 — same status as IllegalTransition;
    /// the message body distinguishes.
    #[error("WO completion blocked by QA gate: {0}")]
    WoCompletionBlockedByQa(String),

    /// DB-layer error from DuckDB or the audit-ledger write.
    /// Route layer maps to 500.
    #[error("storage error: {0}")]
    Storage(#[from] anyhow::Error),
}
