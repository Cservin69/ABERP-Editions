//! Invoice series — the unit of legally-distinct gap-free numbering
//! per ADR-0009 §3.
//!
//! Hungarian rule (Áfa törvény §169) requires gap-free sequence numbering
//! per series. Operators may run multiple series concurrently (e.g. one
//! for products, one for services). Each series has its own reset policy.

use time::OffsetDateTime;

use super::ids::SeriesId;

/// Human-facing series code, e.g. `INV-2026` or `INV-default`. Distinct
/// from [`SeriesId`] (which is the storage key per ADR-0005); the code is
/// the operator-visible label used in invoice numbers like
/// `INV-2026/0042`.
///
/// Validity is non-empty, ASCII-printable, no whitespace, no slashes.
/// The slash is reserved for the human-facing invoice number format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeriesCode(String);

impl SeriesCode {
    pub fn new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        if s.is_empty()
            || s.chars()
                .any(|c| c.is_whitespace() || c == '/' || !c.is_ascii() || c.is_ascii_control())
        {
            None
        } else {
            Some(Self(s))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Reset policy per series, per Hungarian practice. ADR-0009 §3
/// "Series + reset-policy decision per series".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetPolicy {
    /// Number monotonically increases forever. Default for `INV-default`.
    Never,
    /// Number resets to 1 at the start of each fiscal year. Operator opt-in.
    /// **Not implemented in PR-4** — see crate docs.
    AnnualOnFiscalYear,
}

/// Invoice series record stored in the tenant DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceSeries {
    pub id: SeriesId,
    pub code: SeriesCode,
    pub reset_policy: ResetPolicy,
    /// `None` when `reset_policy = Never`; the fiscal year that the
    /// state row is currently anchored to otherwise.
    pub fiscal_year: Option<i32>,
    pub created_at: OffsetDateTime,
}
