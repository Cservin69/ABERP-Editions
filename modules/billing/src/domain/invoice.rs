//! Invoice domain types — `LineItem`, `DraftInvoice`, `ReadyInvoice`.
//!
//! Per ADR-0009 §2 the full state machine spans `Draft → Ready → Submitted
//! → AckPending → Finalized` plus side paths. PR-4 implements `Draft` and
//! `Ready` only; later states arrive with the NAV adapter. Each state is
//! its own type per the new-type-state pattern ADR-0009 §2 names, so
//! illegal transitions are compile errors.

use time::OffsetDateTime;

use super::ids::{CustomerId, InvoiceId, SeriesId};
use super::money::Huf;

/// A line on the invoice. Quantities are `u32` because Hungarian invoices
/// don't fractionalize quantities for typical CNC manufacturing outputs;
/// when a future product line needs decimal quantities, a separate
/// `LineItemKind` variant lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineItem {
    pub description: String,
    pub quantity: u32,
    pub unit_price: Huf,
    /// VAT rate in basis points: 2700 = 27% (Hungarian standard rate).
    /// Integer to avoid floating-point in invariant calculations.
    pub vat_rate_basis_points: u16,
}

impl LineItem {
    /// Pre-tax line total. Returns `None` on overflow.
    pub fn net_total(&self) -> Option<Huf> {
        self.unit_price.checked_mul_u32(self.quantity)
    }

    /// VAT amount for the line: `floor(net_total * rate / 10_000)`.
    /// Returns `None` on overflow.
    pub fn vat_amount(&self) -> Option<Huf> {
        let net = self.net_total()?.as_i64();
        let vat = net.checked_mul(self.vat_rate_basis_points as i64)?;
        Some(Huf(vat / 10_000))
    }

    /// Gross (net + VAT). Returns `None` on overflow.
    pub fn gross_total(&self) -> Option<Huf> {
        self.net_total()?.checked_add(self.vat_amount()?)
    }
}

/// A draft invoice: created in ABERP, not yet validated for submission.
/// `series_id` is committed; `sequence_number` is not yet reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftInvoice {
    pub id: InvoiceId,
    pub series_id: SeriesId,
    pub customer_id: CustomerId,
    pub lines: Vec<LineItem>,
    /// The invoice's issue date. Per ADR-0007 §"Operator-as-threat-actor",
    /// the operator cannot set this; the allocator captures it from the
    /// injected [`crate::ports::clock::Clock`].
    pub issue_date: OffsetDateTime,
}

/// A ready invoice: passed local validation; sequence number reserved in
/// the same transaction that created the reservation row. Promoting a
/// `DraftInvoice` to a `ReadyInvoice` is the job of
/// [`crate::app::issue_invoice::IssueInvoiceCommand`]; constructing one
/// directly outside that handler is a bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyInvoice {
    pub id: InvoiceId,
    pub series_id: SeriesId,
    pub customer_id: CustomerId,
    pub lines: Vec<LineItem>,
    pub issue_date: OffsetDateTime,
    /// The contiguous sequence number assigned in this series for this
    /// fiscal year. Stable; not reused even if the invoice is voided.
    pub sequence_number: u64,
    /// Fiscal year the reservation is anchored to. For
    /// `ResetPolicy::Never` series this is `0`.
    pub fiscal_year: i32,
}

impl ReadyInvoice {
    /// Sum of all line gross totals. Returns `None` on overflow.
    pub fn total_gross(&self) -> Option<Huf> {
        self.lines
            .iter()
            .try_fold(Huf::ZERO, |acc, line| acc.checked_add(line.gross_total()?))
    }
}
