//! Public surface of the billing module per ADR-0006 §"A module's external
//! surface is its `api.rs`".
//!
//! Callers (the binary in PR-5, future modules) reach billing through these
//! re-exports — they should never `use crate::domain::ids` or
//! `use crate::adapters::duckdb_store` from outside this crate.

// ── Domain value types ────────────────────────────────────────────────
pub use crate::domain::ids::{CustomerId, InvoiceId, ReservationId, SeriesId};
pub use crate::domain::invoice::{
    AbandonedInvoice, DraftInvoice, FinalizedInvoice, LineItem, ReadyInvoice, RejectedInvoice,
    SubmissionStuckInvoice, SubmittedInvoice,
};
pub use crate::domain::money::{
    huf_equivalent_round_half_even, Currency, Eur, Huf, Money, RateMetadata,
};
pub use crate::domain::reservation::{ReservationStatus, SequenceReservation};
pub use crate::domain::series::{InvoiceSeries, ResetPolicy, SeriesCode};

// ── Ports (traits) ────────────────────────────────────────────────────
pub use crate::ports::clock::{Clock, SystemClock};
pub use crate::ports::storage::BillingStore;

// ── Adapters ──────────────────────────────────────────────────────────
pub use crate::adapters::duckdb_store::{
    allocate_in_tx, load_ready_invoice_by_id, DuckDbBillingStore,
};
pub use crate::adapters::in_memory_store::InMemoryBillingStore;

// ── Port arg/result types (re-exported so binary callers using
//     `allocate_in_tx` directly can build `AllocateArgs` without
//     reaching into `crate::ports::storage`) ────────────────────────────
pub use crate::ports::storage::{AllocateArgs, AllocateOutcome};

// ── App layer (commands + handler) ────────────────────────────────────
pub use crate::app::error::BillingError;
pub use crate::app::issue_invoice::{
    handle as issue_invoice, IdempotencyKey, IssueInvoiceCommand, IssueInvoiceOutcome,
};
