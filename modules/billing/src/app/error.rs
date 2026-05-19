//! [`BillingError`] — the module's typed error enum.
//!
//! Per ADR-0021 Part A item 2: library crates use `thiserror`; no `anyhow`
//! here. Each variant names the failure source so callers (and CI logs)
//! can locate the issue without spelunking — ADR-0007 fail-loud.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BillingError {
    /// Underlying storage backend failed (DuckDB I/O, query error,
    /// transaction commit error). Adapter-level errors flatten into here.
    #[error("storage error: {0}")]
    Storage(#[from] duckdb::Error),

    /// The caller supplied an invalid argument that the type system
    /// could not reject at compile time (e.g. an unknown `SeriesCode`
    /// at command time, an empty `lines` list).
    #[error("invalid argument: {0}")]
    Invalid(&'static str),

    /// The named series does not exist. Operator-actionable: create the
    /// series first.
    #[error("series not found: {0}")]
    SeriesNotFound(String),

    /// A row could not be decoded from storage. Indicates schema drift
    /// or DB corruption — should never happen during normal operation.
    #[error("storage row decode error at seq={seq}: {reason}")]
    CorruptRow { seq: u64, reason: &'static str },

    /// Money arithmetic overflowed (e.g. quantity * unit_price exceeds
    /// `i64::MAX` forints — implausible for real invoices but the type
    /// system surfaces it loud rather than wrapping silently).
    #[error("money overflow in line {line_index}")]
    MoneyOverflow { line_index: usize },

    /// A wall-clock value could not be formatted for storage. RFC3339
    /// formatting of a valid `OffsetDateTime` cannot fail in practice;
    /// this variant exists so the conversion is loud if it ever does.
    #[error("time format error: {0}")]
    TimeFormat(#[from] time::error::Format),

    /// Parsing a stored RFC3339 string back to `OffsetDateTime` failed.
    /// Indicates schema drift or DB corruption.
    #[error("time parse error: {0}")]
    TimeParse(#[from] time::error::Parse),

    /// The `AnnualOnFiscalYear` reset policy is not implemented in PR-4.
    /// Failing loud rather than silently treating it as `Never`.
    #[error("AnnualOnFiscalYear reset policy is not implemented yet (named for future PR per ADR-0009 §3)")]
    AnnualResetUnimplemented,
}
