//! Sequence reservation per ADR-0009 §3 "Data model" — the
//! `invoice_sequence_reservation` table.
//!
//! A reservation is the legal binding of a specific number in a series
//! to a specific invoice ULID. Reservations move only between
//! `Reserved → Used` (happy path) or `Reserved → Voided` (operator
//! cancels before submission). Numbers are never re-allocated — the
//! sequence stays gap-free in the legal sense even when entries are
//! voided.

use time::OffsetDateTime;

use super::ids::{InvoiceId, ReservationId, SeriesId};

/// Status of a reservation. Per ADR-0009 §3 "Void path".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationStatus {
    /// Sequence number is allocated but the invoice is still `Draft` or
    /// `Ready`; submission has not been attempted.
    Reserved,
    /// Invoice was submitted to NAV and reached a terminal state. The
    /// number is permanently bound.
    Used,
    /// Operator cancelled before submission. The number is not reused;
    /// `void_reason` carries the cancellation justification. Whether
    /// Hungarian practice requires a corrective placeholder invoice is
    /// `[OPEN, accountant]` per ADR-0009 §3.
    Voided,
}

/// A row of the `invoice_sequence_reservation` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceReservation {
    pub id: ReservationId,
    pub series_id: SeriesId,
    pub fiscal_year: i32,
    pub number: u64,
    pub invoice_id: InvoiceId,
    pub status: ReservationStatus,
    pub void_reason: Option<String>,
    pub reserved_at: OffsetDateTime,
    pub used_at: Option<OffsetDateTime>,
    pub voided_at: Option<OffsetDateTime>,
}
