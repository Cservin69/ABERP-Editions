//! ABERP Inventory module v1 — S231 / PR-227 / ADR-0061.
//!
//! ## What this crate does
//!
//! Append-only `stock_movements` ledger + denormalised `stock_qty`
//! cache on the existing `products` table + virtual low-stock view.
//! Single write surface: [`repository::record_movement`] — opens no
//! transactions itself, requires the caller to hold the
//! `&Transaction` so the audit-ledger append and the cache UPDATE
//! ride the same DB commit per ADR-0008 §"Storage". Same posture as
//! [`aberp_audit_ledger::append_in_tx`] and
//! [`aberp_mes::write_mes_adapter_event`].
//!
//! ## What this crate does NOT do
//!
//! - **No HTTP / SPA surface.** Routes live in `apps/aberp/src/serve.rs`
//!   and the SPA pieces in `apps/aberp-ui/src/lib/`. This crate is
//!   the storage + invariant author.
//! - **No multi-warehouse / lot / serial / costing / reservation
//!   model.** All deferred per ADR-0061 §"Out of scope".
//! - **No DB-level CHECK / triggers on derived quantities.** Per
//!   [[no-sql-specific]] + ADR-0061 §3 the invariant author is
//!   `record_movement`, not the storage engine.
//!
//! ## Recovery: `rebuild-stock-cache` binary
//!
//! Per ADR-0061 §3 the cache (`products.stock_qty`) is derived from
//! the ledger (`stock_movements`). If the cache ever drifts (operator
//! edit-by-mistake, schema migration bug, ledger restore from
//! mirror), running `cargo run -p aberp-inventory --bin rebuild-stock-cache -- --tenant <id> --db <path>`
//! re-derives every product's `stock_qty` from `SUM(qty_delta)` in
//! one transaction.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod audit;
mod error;
mod repository;
mod types;

pub use audit::StockMovementRecordedPayload;
pub use error::InventoryError;
pub use repository::{
    current_stock, ensure_schema, inventory_fields_for_product, inventory_fields_for_tenant,
    list_movements_for_product, low_stock_products, rebuild_stock_cache_for_tenant,
    record_movement, validate_reason_sign, InventoryFields, LowStockRow, RecordMovementContext,
    RecordMovementInputs, StockMovement,
};
pub use types::{ActorKind, MovementReason, MovementRefKind, RequiredSign};
