//! Pure domain types for the billing module.
//!
//! No IO, no async, no DB types, no logging — per ADR-0006 §"A module is a
//! Rust workspace member with this internal shape: `domain/` ← pure types".
//!
//! Sub-files:
//!
//! - [`ids`]         ULID newtypes for entities owned by billing.
//! - [`money`]       HUF-only `Huf` amount type (ADR-0009 §1 — currency).
//! - [`invoice`]     `LineItem`, `DraftInvoice`, `ReadyInvoice`.
//! - [`series`]      `InvoiceSeries`, `SeriesCode`, `ResetPolicy`.
//! - [`reservation`] `SequenceReservation`, `ReservationStatus`.

pub mod ids;
pub mod invoice;
pub mod money;
pub mod reservation;
pub mod series;
