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
use crate::domain::money::{Currency, RateMetadata};
use crate::domain::reservation::SequenceReservation;
use crate::domain::series::{InvoiceSeries, SeriesCode};

/// Arguments to the atomic `allocate_and_insert` operation. Grouped here
/// so the trait signature stays readable and so adapters do not develop
/// drifting parameter orders.
///
/// # Currency + rate metadata fields (PR-44γ / ADR-0037)
///
/// `currency` carries the typed `Currency` per ADR-0037 §3's closed
/// vocab + the §4 invariant C8 refusal posture. Pre-PR-44γ call sites
/// continue to pass `Currency::Huf` (the existing HUF-only default
/// preserved by the back-compat constructors on the binary side).
///
/// `rate_metadata` is `Some(_)` iff `currency` is a non-HUF variant —
/// the C10 byte-identical invariant for HUF rows holds at PR-44γ
/// because HUF rows persist `currency = "HUF"` + `rate_metadata = None`
/// + the four exchange-rate columns NULL. EUR rows persist the MNB
/// rate, the rate-publication date (which may be D-1 per ADR-0037
/// §2.b's walk-back), the literal source identifier `"MNB"`, and the
/// round-half-even HUF-equivalent total (C11 per A137).
#[derive(Debug, Clone)]
pub struct AllocateArgs {
    pub series_id: SeriesId,
    pub draft: crate::domain::invoice::DraftInvoice,
    /// Command ULID, used as the idempotency key per ADR-0009 §5 Layer 1.
    /// If a reservation already exists with this key, the allocator
    /// returns the prior outcome without burning a new number.
    pub idempotency_key: crate::app::issue_invoice::IdempotencyKey,
    /// Invoice currency per ADR-0037 §3. `Currency::Huf` for HUF
    /// invoices (the pre-PR-44γ shape); `Currency::Eur` lights up the
    /// EUR path at PR-44γ. Persisted to the DuckDB `invoice.currency`
    /// column (TEXT, NOT NULL, default `'HUF'` for the migration
    /// backfill of pre-PR-44γ rows).
    pub currency: Currency,
    /// MNB-rate metadata. `Some(_)` iff `currency` is non-HUF; persisted
    /// to the four nullable DuckDB columns (`exchange_rate`,
    /// `exchange_rate_source`, `exchange_rate_date`,
    /// `huf_equivalent_total`). The CLI boundary surfaces a typed
    /// loud-fail error if `currency == Eur` and this field is `None`
    /// (per ADR-0037 §4 invariant C1).
    pub rate_metadata: Option<RateMetadata>,
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
