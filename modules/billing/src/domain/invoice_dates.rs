//! PR-84 — pure date-classification helpers for the issue-invoice
//! three-date UX. Mirror of the SPA's `invoice-dates.ts` helpers; lives
//! in the domain layer because the audit stamp records the same
//! classification verbatim (regulatory: out-of-range delivery dates are
//! a tamper-evident operator override).
//!
//! Three concepts (recap):
//!   1. Invoice date — server-stamped today, immutable.
//!   2. Payment deadline — operator picks, fixed or offset.
//!   3. Delivery date — REGULATORY (NAV `invoiceDeliveryDate`); drives
//!      the VAT-period assignment. The "comfort zone" is the closed
//!      interval `[invoice_date, payment_deadline]`; out-of-range
//!      choices flow through but are flagged in the audit ledger.
//!
//! Pure, no IO. Pinned in this module's `#[cfg(test)]` block.

use time::Date;

/// Classification of a candidate delivery date against the comfort
/// zone. The form's "Are you sure?" confirm fires for the two out-of-
/// range arms; the audit payload's `delivery_date_override` field
/// records the same discriminant verbatim (regulatory trail).
///
/// Bounds are INCLUSIVE — a delivery date equal to either endpoint is
/// in range (no confirm, no audit flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryDateZone {
    /// The delivery date falls in `[invoice_date, payment_deadline]`.
    /// The expected operator path; no audit override flag.
    InRange,
    /// The delivery date is before the invoice date. Legitimate
    /// (delivery happened before invoicing) but it shifts the VAT
    /// period earlier than the invoice itself — operator confirms via
    /// the SPA's inline "Are you sure?" affordance and the audit
    /// payload stamps `BeforeInvoiceDate`.
    BeforeInvoiceDate,
    /// The delivery date is after the payment deadline. Legitimate
    /// (advance billing of a future delivery) but it pushes the VAT
    /// period later — operator confirms via the same inline
    /// affordance and the audit payload stamps `AfterPaymentDeadline`.
    AfterPaymentDeadline,
}

impl DeliveryDateZone {
    /// Wire-form discriminant for the audit-ledger payload. The
    /// `InRange` case maps to `None` (default-path; no override flag on
    /// the audit row); the two out-of-range arms map to themselves as
    /// PascalCase strings matching the SPA's `DeliveryDateOverride`
    /// union exactly.
    pub fn audit_discriminant(self) -> Option<&'static str> {
        match self {
            Self::InRange => None,
            Self::BeforeInvoiceDate => Some("BeforeInvoiceDate"),
            Self::AfterPaymentDeadline => Some("AfterPaymentDeadline"),
        }
    }
}

/// Classify a candidate delivery date against the closed-interval
/// comfort zone `[invoice_date, payment_deadline]`.
///
/// Returns `None` when `payment_deadline < invoice_date` — a malformed
/// range the caller's input validator should already have rejected.
/// Refuse to classify rather than producing a wrong answer.
pub fn classify_delivery_date(
    invoice_date: Date,
    payment_deadline: Date,
    delivery_date: Date,
) -> Option<DeliveryDateZone> {
    if payment_deadline < invoice_date {
        return None;
    }
    if delivery_date < invoice_date {
        return Some(DeliveryDateZone::BeforeInvoiceDate);
    }
    if delivery_date > payment_deadline {
        return Some(DeliveryDateZone::AfterPaymentDeadline);
    }
    Some(DeliveryDateZone::InRange)
}

#[cfg(test)]
mod tests {
    //! Heavy boundary coverage — the comfort-zone classifier is the
    //! load-bearing seam between operator UX and the regulatory audit
    //! trail. A regression that flips an inclusive bound to exclusive
    //! would silently turn an in-range choice into an audit-flagged
    //! override (or vice versa); pin both endpoints + the single-day
    //! edge case explicitly.

    use super::*;
    use time::macros::date;

    #[test]
    fn in_range_when_delivery_equals_invoice_date() {
        // Left endpoint inclusive.
        let zone = classify_delivery_date(
            date!(2026 - 05 - 27),
            date!(2026 - 06 - 04),
            date!(2026 - 05 - 27),
        )
        .unwrap();
        assert_eq!(zone, DeliveryDateZone::InRange);
        assert_eq!(zone.audit_discriminant(), None);
    }

    #[test]
    fn in_range_when_delivery_equals_payment_deadline() {
        // Right endpoint inclusive.
        let zone = classify_delivery_date(
            date!(2026 - 05 - 27),
            date!(2026 - 06 - 04),
            date!(2026 - 06 - 04),
        )
        .unwrap();
        assert_eq!(zone, DeliveryDateZone::InRange);
        assert_eq!(zone.audit_discriminant(), None);
    }

    #[test]
    fn in_range_when_delivery_is_strictly_inside() {
        let zone = classify_delivery_date(
            date!(2026 - 05 - 27),
            date!(2026 - 06 - 04),
            date!(2026 - 05 - 30),
        )
        .unwrap();
        assert_eq!(zone, DeliveryDateZone::InRange);
    }

    #[test]
    fn before_invoice_date_when_delivery_is_one_day_earlier() {
        let zone = classify_delivery_date(
            date!(2026 - 05 - 27),
            date!(2026 - 06 - 04),
            date!(2026 - 05 - 26),
        )
        .unwrap();
        assert_eq!(zone, DeliveryDateZone::BeforeInvoiceDate);
        assert_eq!(zone.audit_discriminant(), Some("BeforeInvoiceDate"));
    }

    #[test]
    fn before_invoice_date_when_delivery_is_months_earlier() {
        let zone = classify_delivery_date(
            date!(2026 - 05 - 27),
            date!(2026 - 06 - 04),
            date!(2026 - 01 - 10),
        )
        .unwrap();
        assert_eq!(zone, DeliveryDateZone::BeforeInvoiceDate);
    }

    #[test]
    fn after_payment_deadline_when_delivery_is_one_day_later() {
        let zone = classify_delivery_date(
            date!(2026 - 05 - 27),
            date!(2026 - 06 - 04),
            date!(2026 - 06 - 05),
        )
        .unwrap();
        assert_eq!(zone, DeliveryDateZone::AfterPaymentDeadline);
        assert_eq!(zone.audit_discriminant(), Some("AfterPaymentDeadline"));
    }

    #[test]
    fn after_payment_deadline_when_delivery_is_far_in_the_future() {
        let zone = classify_delivery_date(
            date!(2026 - 05 - 27),
            date!(2026 - 06 - 04),
            date!(2027 - 01 - 01),
        )
        .unwrap();
        assert_eq!(zone, DeliveryDateZone::AfterPaymentDeadline);
    }

    #[test]
    fn zero_day_comfort_zone_is_single_day_inclusive() {
        // Cash sale: invoice issued + due same day. The point-interval
        // [d, d] is still inclusive, so delivery == that day is in range
        // and the day before / after both trigger the override.
        let same = date!(2026 - 05 - 27);
        assert_eq!(
            classify_delivery_date(same, same, same).unwrap(),
            DeliveryDateZone::InRange
        );
        assert_eq!(
            classify_delivery_date(same, same, date!(2026 - 05 - 26)).unwrap(),
            DeliveryDateZone::BeforeInvoiceDate
        );
        assert_eq!(
            classify_delivery_date(same, same, date!(2026 - 05 - 28)).unwrap(),
            DeliveryDateZone::AfterPaymentDeadline
        );
    }

    #[test]
    fn malformed_range_returns_none() {
        // payment_deadline < invoice_date — the form-level validator
        // should already have caught this; the classifier refuses to
        // produce a misleading answer rather than papering over the bug.
        assert_eq!(
            classify_delivery_date(
                date!(2026 - 06 - 04),
                date!(2026 - 05 - 27),
                date!(2026 - 05 - 30),
            ),
            None,
        );
    }
}
