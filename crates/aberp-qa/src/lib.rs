//! ABERP QA queue v1 — S233 / PR-229 / ADR-0063.
//!
//! ## What this crate does
//!
//! ONE table — `qa_inspections` — auto-created when a routing-op flips
//! to Completed. Two write paths:
//!
//! - [`auto_create_inspection_for_op_completion`] — called from
//!   `aberp_work_orders::transition_routing_op` when an op completes.
//!   Inserts a Pending row + emits `QaInspectionCreated`. If a prior
//!   non-superseded inspection exists for the same (wo, op) — the
//!   post-Rework path — the prior row's `superseded_by` is set to the
//!   new qa_id so the live-state filter returns exactly one row.
//!
//! - [`decide_qa`] — operator (or adapter) decides on a Pending row:
//!   Pass / Fail / Rework / Dispose. Per ADR-0063 §4 cross-actor
//!   decisions INSERT a new row + supersede the prior; same-actor
//!   decisions UPDATE in place. Dispose emits a `Scrap` stock_movement
//!   via [`aberp_inventory::record_movement`] (ADR-0063 §6 +
//!   invariant #5). Rework flips the upstream routing-op back to
//!   Active.
//!
//! ## What this crate does NOT do
//!
//! - **No HTTP / SPA surface.** Routes live in `apps/aberp/src/serve.rs`
//!   and SPA pieces in `apps/aberp-ui/ui/src/`. This crate is the
//!   storage + invariant author.
//! - **No DB-level CHECK on `state`** — per [[no-sql-specific]] +
//!   ADR-0063 §9 #2 the transition table is the gate.
//! - **No partial-qty inspection support** — v1 disposes the whole WO
//!   qty per ADR-0063 §"Adversarial review" #5 (partial-qty named-
//!   deferred to v2 alongside per-unit serial tracking).
//! - **No inspector roles / permissions** — anyone can `decide_qa`
//!   per ADR-0063 §"Out of scope".
//! - **No `aberp_work_orders` dep** to avoid a cycle (work-orders
//!   depends on aberp-qa for the auto-create-on-op-completion path).
//!   The Rework side-effect (flip routing-op back to Active) and the
//!   Dispose side-effect (read WO qty for the Scrap qty_delta) read
//!   `routings` / `work_orders` via SQL directly. Schema lives in
//!   aberp-work-orders' migration; we treat the column shape as a
//!   contract pinned by the cross-crate round-trip tests in
//!   `tests/qa_round_trip.rs`.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod audit;
mod error;
mod repository;
mod state;
mod types;

pub use audit::{QaInspectionCreatedPayload, QaInspectionDecidedPayload};
pub use error::QaError;
pub use repository::{
    all_live_inspections_passed_for_wo, auto_create_inspection_for_op_completion, decide_qa,
    ensure_schema, get_qa_inspection, list_live_inspections_for_wo, list_qa_inspections,
    AutoCreateInspectionInputs, DecideQaInputs, QaDecisionOutcome, QaInspection, QaWriteContext,
};
pub use state::{next_qa_state, QaStateError};
pub use types::{QaDecision, QaState};
