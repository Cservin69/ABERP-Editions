//! ABERP Dispatch board v1 — S234 / PR-230 / ADR-0064.
//!
//! ## What this crate does
//!
//! ONE table — `dispatches` — one row per Completed work order. Two
//! write paths:
//!
//! - [`create_dispatch`] — operator (or future adapter) creates a
//!   Drafted dispatch against a Completed WO. Enforces the ADR-0064 §2
//!   eligibility gate (WO state = Completed AND no prior dispatch row)
//!   in the supplied transaction. Emits `DispatchCreated` audit entry.
//!
//! - [`mark_shipped`] — operator (or future adapter) flips Drafted →
//!   Shipped. Per ADR-0064 §4 + §5 + §"Invariants pinned" #1 this
//!   writes ALL of the following in the SAME caller-owned transaction:
//!   the dispatch state flip + carrier_kind + tracking_number +
//!   shipped_at; one `Dispatch` `stock_movements` row (via
//!   [`aberp_inventory::record_movement`]); one spawned invoice id
//!   (via the injected [`InvoiceSpawner`] — see §"Invoice spawner"
//!   below); one `DispatchShipped` audit entry; and — S440 — the three
//!   `export.*` export-control entries (`classification_set`,
//!   `access_check`, `shipment_logged`) produced by the injected
//!   [`ExportControlContext`]. Any failure rolls back all of them.
//!
//! ## S440 — export-control firing sites
//!
//! A shipment is the one place in ABERP where an export-controlled
//! action actually happens, so `mark_shipped` is where the `export.*`
//! family (kinds-only since S359) fires. The gate refuses the ship in
//! code when the consignee screens to a denied-party / restricted match
//! ([[trust-code-not-operator]]). **Weak without RBAC:** the events
//! record *what happened*, not *who was permitted* — there is no
//! role/citizenship check behind `operator_user_id` yet.
//!
//! - [`cancel_dispatch`] — operator cancels a Drafted dispatch. No
//!   inventory impact, no invoice spawn, no dedicated audit kind per
//!   ADR-0064 §6.
//!
//! ## What this crate does NOT do
//!
//! - **No HTTP / SPA surface.** Routes live in `apps/aberp/src/serve.rs`
//!   and SPA pieces in `apps/aberp-ui/ui/src/`. This crate is the
//!   storage + invariant author.
//! - **No DB-level CHECK on `state` / `carrier_kind`** — per
//!   [[no-sql-specific]] + ADR-0064 §8 #2 the transition table is the
//!   gate.
//! - **No carrier-API integration / label print / cost calc** — per
//!   ADR-0064 §"Out of scope".
//! - **No partial shipments** — one dispatch per WO in v1 per ADR-0064
//!   §"Out of scope".
//! - **No auto-NAV-submit on Ship** — the injected [`InvoiceSpawner`]
//!   spawns a DRAFT only; the operator's Issue click is the only NAV
//!   trigger per ADR-0064 §5 + §"Alternatives considered".
//! - **No `aberp_work_orders` runtime dep** — `mark_shipped` reads
//!   `work_orders.product_id` + `work_orders.qty_target` via direct
//!   SQL inside the supplied tx. Same posture as
//!   [`aberp_qa::decide_qa`]'s Dispose branch (it reads `work_orders`
//!   directly to size the Scrap qty without depending on
//!   aberp-work-orders). Schema lives in aberp-work-orders'
//!   migration; we treat the column shape as a contract pinned by the
//!   cross-crate round-trip tests in `tests/dispatch_round_trip.rs`.
//!
//! ## Invoice spawner — divergence from ADR-0064 §5
//!
//! ADR-0064 §5 demands the Stage 1 invoice draft be created IN the
//! same transaction as the dispatch state flip + stock movement. The
//! existing Stage 1 issuance pipeline
//! (`apps/aberp/src/issue_invoice.rs::issue_from_parsed`) is async,
//! opens its own transaction, and atomically allocates a gap-free
//! sequence number — bringing it in-tx would require a major refactor
//! that is OUT OF SCOPE for PR-230.
//!
//! Per [[pushback-as-method]] the PR-230 body flags this and proposes
//! the v1 cut: this crate defines [`InvoiceSpawner`] as a trait
//! parameter to [`mark_shipped`]. PR-230 ships [`NoopInvoiceSpawner`]
//! as the v1 production wiring (returns `Ok(None)`); the SPA's
//! dispatch detail surfaces a click-through that pre-fills the
//! existing IssueInvoice form. PR-230b will land the sync billing
//! extraction + the real spawner.
//!
//! Tests in `tests/dispatch_round_trip.rs` use a [`MockInvoiceSpawner`]
//! to pin invariants #4 (spawned invoice is Drafted-equivalent), #5
//! (spawned_invoice_id matches the spawner's return), and #6 (failed
//! spawn rolls back the entire mark_shipped tx).

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod audit;
mod error;
mod repository;
mod state;
mod types;

pub use audit::{
    DispatchCreatedPayload, DispatchShippedPayload, ExportAccessCheckPayload,
    ExportClassificationSetPayload, ExportShipmentLoggedPayload,
};
pub use error::DispatchError;
pub use repository::{
    cancel_dispatch, count_dispatches_by_state, count_dispatches_shipped_today,
    count_eligible_work_orders, create_dispatch, ensure_schema, get_dispatch, list_dispatches,
    list_eligible_work_orders, mark_shipped, null_spawned_invoice_id_in_tx, CreateDispatchInputs,
    Dispatch, DispatchStateCounts, DispatchWriteContext, EligibleWorkOrder, ExportControlContext,
    InvoiceSpawner, MarkShippedInputs, MarkShippedOutcome, NoopInvoiceSpawner,
    NoopShipmentDocumentBinder, ShipmentDocumentBinder, MAX_DISPATCH_LIST_LIMIT,
    MAX_ELIGIBLE_WO_LIMIT,
};
pub use state::{next_dispatch_state, DispatchAction, DispatchStateError};
pub use types::{CarrierKind, DispatchState};
