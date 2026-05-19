//! Injectable wall clock for the billing module.
//!
//! Per ADR-0007 §"Operator-as-threat-actor controls": "invoice timestamps
//! are server-clock-only; the operator cannot set them." The allocator
//! captures `issue_date` from a [`Clock`], not from the command payload.
//!
//! Tests inject a fixed-time clock so the year-roll logic, idempotency
//! checks, and reservation timestamps are deterministic. Production uses
//! [`SystemClock`] which calls [`time::OffsetDateTime::now_utc`].

use std::fmt;

use time::OffsetDateTime;

pub trait Clock: fmt::Debug + Send + Sync {
    /// Current wall-clock UTC time. The allocator stores this verbatim as
    /// the invoice's `issue_date` and as the reservation's `reserved_at`.
    fn now_utc(&self) -> OffsetDateTime;
}

/// Production clock — wraps the OS wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}
