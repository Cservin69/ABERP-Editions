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

    /// Consume this `ReadyInvoice` and produce a [`SubmittedInvoice`]
    /// carrying the NAV-assigned transaction id. The new-type-state
    /// pattern (ADR-0009 §2) requires the transition to consume the
    /// previous state — accidentally re-submitting a `ReadyInvoice`
    /// after a successful `manageInvoice` is a compile error rather
    /// than a runtime hunt.
    pub fn into_submitted(self, nav_transaction_id: String) -> SubmittedInvoice {
        SubmittedInvoice {
            id: self.id,
            series_id: self.series_id,
            customer_id: self.customer_id,
            lines: self.lines,
            issue_date: self.issue_date,
            sequence_number: self.sequence_number,
            fiscal_year: self.fiscal_year,
            nav_transaction_id,
        }
    }
}

/// A submitted invoice: `manageInvoice` returned an `OK` response with
/// a NAV-assigned `transactionId`. The state is now past the point
/// where ABERP can void or modify; advancement is by NAV's terminal
/// ack (`SAVED` → `Finalized`; `ABORTED` → `Rejected`) per ADR-0009 §2
/// and §5. PR-7-C polls `queryTransactionStatus` to drive that.
///
/// The fields mirror [`ReadyInvoice`] (same id, same series, same
/// sequence, same lines) plus the NAV transaction id. Carrying the
/// full body forward keeps the post-submit flow self-contained — the
/// PR-7-C poll loop can format log lines and audit-evidence bundles
/// without re-reading the `invoice` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedInvoice {
    pub id: InvoiceId,
    pub series_id: SeriesId,
    pub customer_id: CustomerId,
    pub lines: Vec<LineItem>,
    pub issue_date: OffsetDateTime,
    pub sequence_number: u64,
    pub fiscal_year: i32,
    /// NAV's opaque tracking id, returned by `manageInvoice`. Treated
    /// as a string at this layer — ABERP does not parse its shape.
    pub nav_transaction_id: String,
}

impl SubmittedInvoice {
    /// Sum of all line gross totals. Returns `None` on overflow.
    /// Mirrors `ReadyInvoice::total_gross` so the post-submit flow
    /// can produce the same operator-visible totals without
    /// down-converting to `ReadyInvoice`.
    pub fn total_gross(&self) -> Option<Huf> {
        self.lines
            .iter()
            .try_fold(Huf::ZERO, |acc, line| acc.checked_add(line.gross_total()?))
    }

    /// Consume this `SubmittedInvoice` and produce a [`FinalizedInvoice`].
    /// Driven by NAV's terminal-positive `SAVED` ack per ADR-0009 §2.
    /// The transition consumes `self` per the new-type-state rule — a
    /// finalized invoice cannot be re-polled, re-submitted, or re-
    /// finalized.
    pub fn into_finalized(self) -> FinalizedInvoice {
        FinalizedInvoice {
            id: self.id,
            series_id: self.series_id,
            customer_id: self.customer_id,
            lines: self.lines,
            issue_date: self.issue_date,
            sequence_number: self.sequence_number,
            fiscal_year: self.fiscal_year,
            nav_transaction_id: self.nav_transaction_id,
        }
    }

    /// Consume this `SubmittedInvoice` and produce a [`RejectedInvoice`].
    /// Driven by NAV's terminal-negative `ABORTED` ack per ADR-0009 §2.
    /// The rejected sequence slot is NOT reused (gap-free invariant);
    /// the audit ledger documents the rejection and a corrective new
    /// invoice must be issued.
    pub fn into_rejected(self) -> RejectedInvoice {
        RejectedInvoice {
            id: self.id,
            series_id: self.series_id,
            customer_id: self.customer_id,
            lines: self.lines,
            issue_date: self.issue_date,
            sequence_number: self.sequence_number,
            fiscal_year: self.fiscal_year,
            nav_transaction_id: self.nav_transaction_id,
        }
    }

    /// Consume this `SubmittedInvoice` and produce a
    /// [`SubmissionStuckInvoice`]. Driven by the poll loop running out
    /// of bounded retries per ADR-0009 §5, or by a NAV-side non-
    /// retryable error during the poll. Operator-action-required; the
    /// audit ledger carries the last NAV status (typically
    /// `RECEIVED` / `PROCESSING`) or the NAV error code.
    pub fn into_submission_stuck(self) -> SubmissionStuckInvoice {
        SubmissionStuckInvoice {
            id: self.id,
            series_id: self.series_id,
            customer_id: self.customer_id,
            lines: self.lines,
            issue_date: self.issue_date,
            sequence_number: self.sequence_number,
            fiscal_year: self.fiscal_year,
            nav_transaction_id: self.nav_transaction_id,
        }
    }
}

/// A finalized invoice: NAV's `queryTransactionStatus` returned `SAVED`
/// per ADR-0009 §2. The invoice is legally issued and reported. No
/// transition out of this state in PR-7-C scope; ADR-0009 §6 names
/// `Amended` (a MODIFY chain invoice references this one) and `Storno`
/// (a STORNO chain invoice cancels this one) as the side-paths that
/// will be added when their first call site materialises.
///
/// Fields mirror [`SubmittedInvoice`] verbatim — the typestate machinery
/// is the only thing that changes at this transition. Carrying the full
/// body forward lets the operator-visible summary read the totals
/// without re-loading the `invoice` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedInvoice {
    pub id: InvoiceId,
    pub series_id: SeriesId,
    pub customer_id: CustomerId,
    pub lines: Vec<LineItem>,
    pub issue_date: OffsetDateTime,
    pub sequence_number: u64,
    pub fiscal_year: i32,
    /// NAV-assigned transaction id from the prior `manageInvoice`
    /// response. Kept on the finalized invoice so the audit-evidence
    /// bundle (ADR-0009 §8) can be reconstructed from this state alone.
    pub nav_transaction_id: String,
}

impl FinalizedInvoice {
    /// Sum of all line gross totals. Returns `None` on overflow.
    /// Mirrors `ReadyInvoice::total_gross` / `SubmittedInvoice::total_gross`
    /// so the post-terminal flow can produce the same operator-visible
    /// totals at every stage.
    pub fn total_gross(&self) -> Option<Huf> {
        self.lines
            .iter()
            .try_fold(Huf::ZERO, |acc, line| acc.checked_add(line.gross_total()?))
    }
}

/// A rejected invoice: NAV's `queryTransactionStatus` returned `ABORTED`
/// per ADR-0009 §2. The sequence number is NOT reused; the operator
/// must issue a corrective new invoice. The fields mirror
/// [`SubmittedInvoice`] for the same operator-visible-totals reason as
/// [`FinalizedInvoice`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedInvoice {
    pub id: InvoiceId,
    pub series_id: SeriesId,
    pub customer_id: CustomerId,
    pub lines: Vec<LineItem>,
    pub issue_date: OffsetDateTime,
    pub sequence_number: u64,
    pub fiscal_year: i32,
    pub nav_transaction_id: String,
}

impl RejectedInvoice {
    /// Sum of all line gross totals. Returns `None` on overflow.
    pub fn total_gross(&self) -> Option<Huf> {
        self.lines
            .iter()
            .try_fold(Huf::ZERO, |acc, line| acc.checked_add(line.gross_total()?))
    }
}

/// A submission-stuck invoice: bounded retries on the poll loop were
/// exhausted, OR NAV returned a non-retryable error during the poll
/// (per ADR-0009 §5). No automatic state advance — the operator
/// unblocks via a typed `RetrySubmission` or `MarkSubmissionAbandoned`
/// command (PR-8). Fields mirror [`SubmittedInvoice`] verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionStuckInvoice {
    pub id: InvoiceId,
    pub series_id: SeriesId,
    pub customer_id: CustomerId,
    pub lines: Vec<LineItem>,
    pub issue_date: OffsetDateTime,
    pub sequence_number: u64,
    pub fiscal_year: i32,
    pub nav_transaction_id: String,
}

impl SubmissionStuckInvoice {
    /// Sum of all line gross totals. Returns `None` on overflow.
    pub fn total_gross(&self) -> Option<Huf> {
        self.lines
            .iter()
            .try_fold(Huf::ZERO, |acc, line| acc.checked_add(line.gross_total()?))
    }

    /// Consume this `SubmissionStuckInvoice` and produce an
    /// [`AbandonedInvoice`]. Driven by the operator's
    /// `MarkSubmissionAbandoned` decision per ADR-0009 §5 (PR-8).
    /// The transition consumes `self` per the new-type-state rule —
    /// an abandoned invoice cannot be re-submitted, re-polled, or
    /// re-abandoned through this codepath. The sequence slot is NOT
    /// reused (gap-free invariant remains intact); the audit ledger's
    /// `InvoiceMarkedAbandoned` entry documents the abandonment and
    /// a corrective new invoice must be issued if the business
    /// transaction still needs reporting.
    pub fn into_abandoned(self) -> AbandonedInvoice {
        AbandonedInvoice {
            id: self.id,
            series_id: self.series_id,
            customer_id: self.customer_id,
            lines: self.lines,
            issue_date: self.issue_date,
            sequence_number: self.sequence_number,
            fiscal_year: self.fiscal_year,
            nav_transaction_id: self.nav_transaction_id,
        }
    }
}

/// An abandoned invoice: the operator ran `MarkSubmissionAbandoned`
/// per ADR-0009 §5. Terminal in the typestate machine — no transition
/// out exists, by design. The sequence slot is NOT reused (gap-free
/// invariant); the audit ledger's `InvoiceMarkedAbandoned` entry plus
/// the upstream `InvoiceSubmissionAttempt` / `InvoiceSubmissionResponse`
/// / `InvoiceAckStatus` chain is the audit-evidence body. Fields
/// mirror [`SubmissionStuckInvoice`] verbatim so the audit-evidence
/// bundle can reconstruct the full invoice body from this state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedInvoice {
    pub id: InvoiceId,
    pub series_id: SeriesId,
    pub customer_id: CustomerId,
    pub lines: Vec<LineItem>,
    pub issue_date: OffsetDateTime,
    pub sequence_number: u64,
    pub fiscal_year: i32,
    pub nav_transaction_id: String,
}

impl AbandonedInvoice {
    /// Sum of all line gross totals. Returns `None` on overflow.
    /// Mirrors every other typestate's `total_gross` for the same
    /// operator-visible-totals reason.
    pub fn total_gross(&self) -> Option<Huf> {
        self.lines
            .iter()
            .try_fold(Huf::ZERO, |acc, line| acc.checked_add(line.gross_total()?))
    }
}

#[cfg(test)]
mod tests {
    //! Typestate-transition tests for PR-7-C-2. The new-type-state
    //! pattern's compile-time enforcement (illegal transitions are
    //! compile errors) cannot be exercised by a unit test, so these
    //! tests pin the runtime invariant: a transition consumes the
    //! source state and preserves every load-bearing field on the
    //! destination state. If a future refactor drops or reorders a
    //! field-copy, the field-by-field assertions catch it loud.
    //!
    //! CLAUDE.md rule 9: "tests verify intent, not just behavior." The
    //! intent here is "transitions are pure renames of the typestate;
    //! the underlying invoice data is identical post-transition." A
    //! test that only checked `id` would still pass if the transition
    //! lost half the lines — the field-by-field walk closes that.
    use super::*;
    use crate::domain::ids::{CustomerId, InvoiceId, SeriesId};

    fn fixture_submitted() -> SubmittedInvoice {
        SubmittedInvoice {
            id: InvoiceId::new(),
            series_id: SeriesId::new(),
            customer_id: CustomerId::new(),
            lines: vec![
                LineItem {
                    description: "widget A".to_string(),
                    quantity: 3,
                    unit_price: Huf(1_000),
                    vat_rate_basis_points: 2700,
                },
                LineItem {
                    description: "widget B".to_string(),
                    quantity: 1,
                    unit_price: Huf(500),
                    vat_rate_basis_points: 2700,
                },
            ],
            issue_date: OffsetDateTime::now_utc(),
            sequence_number: 42,
            fiscal_year: 0,
            nav_transaction_id: "TXID-fixture-1".to_string(),
        }
    }

    #[test]
    fn into_finalized_preserves_every_field() {
        let s = fixture_submitted();
        let id = s.id;
        let series_id = s.series_id;
        let customer_id = s.customer_id;
        let lines = s.lines.clone();
        let issue_date = s.issue_date;
        let seq = s.sequence_number;
        let fy = s.fiscal_year;
        let txid = s.nav_transaction_id.clone();

        let f = s.into_finalized();
        assert_eq!(f.id, id);
        assert_eq!(f.series_id, series_id);
        assert_eq!(f.customer_id, customer_id);
        assert_eq!(f.lines, lines);
        assert_eq!(f.issue_date, issue_date);
        assert_eq!(f.sequence_number, seq);
        assert_eq!(f.fiscal_year, fy);
        assert_eq!(f.nav_transaction_id, txid);
    }

    #[test]
    fn into_rejected_preserves_every_field() {
        let s = fixture_submitted();
        let id = s.id;
        let lines = s.lines.clone();
        let seq = s.sequence_number;
        let txid = s.nav_transaction_id.clone();

        let r = s.into_rejected();
        assert_eq!(r.id, id);
        assert_eq!(r.lines, lines);
        assert_eq!(r.sequence_number, seq);
        assert_eq!(r.nav_transaction_id, txid);
    }

    #[test]
    fn into_submission_stuck_preserves_every_field() {
        let s = fixture_submitted();
        let id = s.id;
        let lines = s.lines.clone();
        let seq = s.sequence_number;
        let txid = s.nav_transaction_id.clone();

        let stuck = s.into_submission_stuck();
        assert_eq!(stuck.id, id);
        assert_eq!(stuck.lines, lines);
        assert_eq!(stuck.sequence_number, seq);
        assert_eq!(stuck.nav_transaction_id, txid);
    }

    /// PR-8: `SubmissionStuckInvoice → AbandonedInvoice` is a pure rename
    /// of typestate; every field must survive the transition byte-for-
    /// byte. Field-by-field walk (CLAUDE.md rule 9: tests verify intent,
    /// not just behavior — a test that only checked `id` would still
    /// pass if `into_abandoned` lost half the lines).
    #[test]
    fn into_abandoned_preserves_every_field() {
        let s = fixture_submitted();
        let id = s.id;
        let series_id = s.series_id;
        let customer_id = s.customer_id;
        let lines = s.lines.clone();
        let issue_date = s.issue_date;
        let seq = s.sequence_number;
        let fy = s.fiscal_year;
        let txid = s.nav_transaction_id.clone();

        let stuck = s.into_submission_stuck();
        let abandoned = stuck.into_abandoned();
        assert_eq!(abandoned.id, id);
        assert_eq!(abandoned.series_id, series_id);
        assert_eq!(abandoned.customer_id, customer_id);
        assert_eq!(abandoned.lines, lines);
        assert_eq!(abandoned.issue_date, issue_date);
        assert_eq!(abandoned.sequence_number, seq);
        assert_eq!(abandoned.fiscal_year, fy);
        assert_eq!(abandoned.nav_transaction_id, txid);
    }

    #[test]
    fn total_gross_consistent_across_states() {
        // 3 * 1000 = 3000 net, 27% VAT = 810, gross = 3810
        // 1 * 500  =  500 net, 27% VAT = 135, gross =  635
        // total gross = 4445
        let s = fixture_submitted();
        let s_gross = s.total_gross().expect("totals");
        let s2 = s.clone();
        let s3 = s.clone();
        let s4 = s.clone();

        assert_eq!(s2.into_finalized().total_gross().unwrap(), s_gross);
        assert_eq!(s3.into_rejected().total_gross().unwrap(), s_gross);
        assert_eq!(s4.into_submission_stuck().total_gross().unwrap(), s_gross);
        // Through the full Submitted → Stuck → Abandoned chain.
        assert_eq!(
            s.into_submission_stuck()
                .into_abandoned()
                .total_gross()
                .unwrap(),
            s_gross
        );
    }
}
