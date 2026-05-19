//! HUF money type per ADR-0009 §1: "Currency: HUF only for v1. The command
//! boundary rejects any currency code other than `HUF`."
//!
//! HUF amounts are stored as **whole forints** in an `i64`. HUF has no
//! sub-unit in practice — Hungarian invoices round to the forint, and NAV
//! accepts integer HUF values. Multi-currency adds a separate ADR with the
//! named trigger "first non-HUF customer signed" (ADR-0009 §1).
//!
//! Negative amounts are permitted at the type level because credit notes
//! and storno invoices carry negative line totals. The command-handler
//! layer (`app/issue_invoice.rs`) enforces per-line invariants.

use std::fmt;

/// Whole forints. Hungarian invoicing rounds to the forint at the line
/// level; sub-forint precision is not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Huf(pub i64);

impl Huf {
    pub const ZERO: Self = Self(0);

    pub fn as_i64(self) -> i64 {
        self.0
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    pub fn checked_mul_u32(self, n: u32) -> Option<Self> {
        self.0.checked_mul(n as i64).map(Self)
    }
}

impl fmt::Display for Huf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} HUF", self.0)
    }
}
