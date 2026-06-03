//! ABERP Work Orders module v1 — S232 / PR-228 / ADR-0062.
//!
//! ## What this crate does
//!
//! Three tables on the per-tenant DuckDB:
//!
//! - `work_orders` — the regulated entity (Created → Released →
//!   InProgress → Completed | Cancelled | OnHold).
//! - `boms` — 1-level bill of materials per finished good
//!   (soft-retired, never DELETEd per ADR-0062 §6).
//! - `routings` — linear per-WO operation sequence.
//!
//! One write surface per concern:
//!
//! - [`create_work_order`] — insert WO + N routings + emit
//!   `WorkOrderCreated` audit entry.
//! - [`transition_work_order`] — state transition + side effects
//!   (BOM consumption on Release, finished-good production on
//!   Complete) + audit entry. SAME function called by SPA buttons
//!   AND future adapter events per ADR-0062 §3.
//! - [`replace_bom_for_product`] — soft-retire prior active BOM
//!   rows + insert new lines (no audit kind in v1; BOM is reference
//!   data per ADR-0062 §6).
//!
//! ## What this crate does NOT do
//!
//! - **No HTTP / SPA surface.** Routes live in
//!   `apps/aberp/src/serve.rs`. This crate is the storage + invariant
//!   author.
//! - **No BOM nesting** — flat per ADR-0062 §"Out of scope".
//! - **No routing branching** — linear per ADR-0062 §"Out of scope".
//! - **No DB-level CHECK on state columns** — per
//!   [[no-sql-specific]] + ADR-0062 §"Cross-cutting decisions" #2
//!   the transition table is the gate.
//! - **No auto-reverse on Cancel** — per ADR-0062 §5 the operator
//!   posts manual `Adjustment` movements to recover scrap-allowance
//!   stock.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod audit;
mod error;
mod repository;
mod state;
mod types;

pub use audit::{
    RoutingOpStateChangedPayload, WorkOrderCreatedPayload, WorkOrderStateChangedPayload,
};
pub use error::WorkOrderError;
pub use repository::{
    create_work_order, ensure_schema, list_active_bom_for_product, list_routing_ops_for_wo,
    list_work_orders, read_routing_op, read_work_order, replace_bom_for_product,
    transition_routing_op, transition_work_order, BomLine, BomLineInput, CreateWorkOrderInputs,
    RoutingOp, RoutingOpInput, RoutingOpTransitionInputs, RoutingOpTransitionOutcome,
    TransitionInputs, WoWriteContext, WorkOrder, WorkOrderTransitionOutcome,
    MAX_BOM_LINES_PER_REQUEST, MAX_ROUTING_OPS_PER_WO,
};
pub use state::{next_routing_op_state, next_state, RoutingOpStateError, WoStateError};
pub use types::{RoutingOpAction, RoutingOpState, WoAction, WorkOrderState};
