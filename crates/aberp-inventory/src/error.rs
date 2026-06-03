//! Typed errors for the inventory crate. The route layer maps these
//! to HTTP status codes per the existing partners / products /
//! incoming_invoices pattern.

use thiserror::Error;

/// Errors surfaced by the inventory repository.
#[derive(Debug, Error)]
pub enum InventoryError {
    /// Reason-sign matrix violation per ADR-0061 §5. The route layer
    /// maps this to HTTP 400 with a body describing which reason +
    /// which sign was required. Surfaced BEFORE any DB write — refuse
    /// at the boundary, never silently flip.
    #[error("reason {reason} requires sign {required:?}, got qty_delta={got}")]
    WrongSignForReason {
        reason: &'static str,
        required: crate::types::RequiredSign,
        got: rust_decimal::Decimal,
    },

    /// The `idempotency_key` already exists in `stock_movements`. The
    /// route layer maps to HTTP 409 — the client retried a request
    /// that already landed.
    #[error("duplicate idempotency_key {0}")]
    DuplicateIdempotencyKey(String),

    /// Caller asked to write a movement against a product that does
    /// not exist in the tenant's `products` table. Route layer maps
    /// to 404 — the client has a stale product id.
    #[error("product {0} not found")]
    ProductNotFound(String),

    /// DB-layer error from DuckDB or the audit-ledger write.
    /// The route layer maps to 500.
    #[error("storage error: {0}")]
    Storage(#[from] anyhow::Error),
}
