//! Billing storage port.
//!
//! Per ADR-0006: "Each module defines its own **storage port** as a Rust
//! trait whose methods are in terms of *domain types*, not SQL." The SQL
//! string `duckdb` does not appear in domain or app layers.
//!
//! The trait is shaped around what the allocator (ADR-0009 §3) actually
//! needs: read series + state, atomically allocate a number, insert a
//! reservation + invoice, lookup by idempotency key. Concrete adapters
//! ([`crate::adapters::duckdb_store`], [`crate::adapters::in_memory_store`])
//! implement these against their backend.

use std::fmt;

use time::OffsetDateTime;

use crate::app::error::BillingError;
use crate::domain::ids::{InvoiceId, SeriesId};
use crate::domain::invoice::ReadyInvoice;
use crate::domain::reservation::SequenceReservation;
use crate::domain::series::{InvoiceSeries, SeriesCode};

/// Arguments to the atomic `allocate_and_insert` operation. Grouped here
/// so the trait signature stays readable and so adapters do not develop
/// drifting parameter orders.
#[derive(Debug, Clone)]
pub struct AllocateArgs {
    pub series_id: SeriesId,
    pub draft: crate::domain::invoice::DraftInvoice,
    /// Command ULID, used as the idempotency key per ADR-0009 §5 Layer 1.
    /// If a reservation already exists with this key, the allocator
    /// returns the prior outcome without burning a new number.
    pub idempotency_key: crate::app::issue_invoice::IdempotencyKey,
}

/// Outcome of an `allocate_and_insert` call. The fresh and replay
/// branches are distinguished loudly so callers (and tests) can verify
/// idempotency rather than infer it from byte equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllocateOutcome {
    /// A fresh number was burned; this invoice did not previously exist.
    Fresh {
        invoice: ReadyInvoice,
        reservation: SequenceReservation,
    },
    /// The idempotency key matched an existing reservation; no new number
    /// was burned. Returned outcome is byte-identical to the original.
    Replay {
        invoice: ReadyInvoice,
        reservation: SequenceReservation,
    },
}

// `Send` (not `Send + Sync`): `duckdb::Connection` is `Send` but its
// internal `RefCell<InnerConnection>` makes it `!Sync`. We never share a
// store across threads (`&mut self` everywhere), so `Sync` would be
// purely-aspirational ceremony that excludes the production adapter.
// The audit-ledger crate's `Ledger` carries no Send/Sync bound at all
// for the same reason; we keep `Send` here so a future thread-per-tenant
// model can still move stores between worker threads.
pub trait BillingStore: fmt::Debug + Send {
    /// Create the schema if it doesn't exist. Idempotent.
    fn ensure_schema(&mut self) -> Result<(), BillingError>;

    /// Insert a new invoice series. Errors if `code` already exists.
    fn create_series(&mut self, series: &InvoiceSeries) -> Result<(), BillingError>;

    /// Look up a series by its operator-visible code.
    fn find_series_by_code(&self, code: &SeriesCode)
        -> Result<Option<InvoiceSeries>, BillingError>;

    /// Look up a series by ULID.
    fn find_series_by_id(&self, id: SeriesId) -> Result<Option<InvoiceSeries>, BillingError>;

    /// Atomically allocate a sequence number, insert the reservation
    /// row, and insert the invoice row — all in one transaction per
    /// ADR-0009 §3 "Allocate (atomic)". Idempotent under retry of the
    /// same `idempotency_key`.
    fn allocate_and_insert(
        &mut self,
        args: AllocateArgs,
        now: OffsetDateTime,
    ) -> Result<AllocateOutcome, BillingError>;

    /// Mark a reservation as Voided. ADR-0009 §3 "Void path".
    /// **Not exercised by PR-4 tests but defined here so the trait
    /// surface matches the data model.** Failing loud rather than
    /// quietly skipping per ADR-0007.
    fn void_reservation(
        &mut self,
        invoice_id: InvoiceId,
        void_reason: String,
        voided_at: OffsetDateTime,
    ) -> Result<(), BillingError>;

    /// Read all reservations for a series, oldest first. Used by tests
    /// to assert gap-free numbering.
    fn list_reservations(
        &self,
        series_id: SeriesId,
    ) -> Result<Vec<SequenceReservation>, BillingError>;
}
