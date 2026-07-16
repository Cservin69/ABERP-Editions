//! Pure domain types for the billing module.
//!
//! No IO, no async, no DB types, no logging — per ADR-0006 §"A module is a
//! Rust workspace member with this internal shape: `domain/` ← pure types".
//!
//! Sub-files:
//!
//! - [`ids`]         ULID newtypes for entities owned by billing.
//! - [`money`]       `Huf` + `Eur` amount types, `Currency` closed-vocab
//!                   enum, and the currency-aware `Money` sum (ADR-0009 §1
//!                   extended by ADR-0037 §3 — see PR-44α).
//! - [`invoice`]     `LineItem`, `DraftInvoice`, `ReadyInvoice`.
//! - [`invoice_dates`] PR-84 — pure comfort-zone classifier for the
//!                   three-date issuance UX. Mirror of the SPA's
//!                   `invoice-dates.ts` helper.
//! - [`series`]      `InvoiceSeries`, `SeriesCode`, `ResetPolicy`.
//! - [`reservation`] `SequenceReservation`, `ReservationStatus`.
//! - [`unit_of_measure`] S159 — `NavUnitOfMeasure`, `ProductUnit` (moved
//!                   down from products so `LineItem` can carry a line's
//!                   unit to the NAV `<unitOfMeasure>` emit).
//! - [`payment_method`] S160 — `PaymentMethod`, the per-invoice NAV
//!                   `<paymentMethod>` closed-vocab snapshot (ADR-0050).
//! - [`vat_rate_kind`] ADR-0101 — `VatRateKind`, the per-line closed-vocab
//!                   discriminant selecting the NAV `<lineVatRate>` choice
//!                   element (`vatPercentage` / `vatExemption` /
//!                   `vatOutOfScope` / `vatDomesticReverseCharge`).

pub mod ids;
pub mod invoice;
pub mod invoice_dates;
pub mod money;
pub mod payment_method;
pub mod reservation;
pub mod series;
pub mod unit_of_measure;
pub mod vat_rate_kind;
