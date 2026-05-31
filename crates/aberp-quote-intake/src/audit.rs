//! Audit payload + write helper for `EventKind::QuoteIntakePollCompleted`.
//!
//! Emit only when the cycle saw work (`fetched > 0`), when a
//! writeback retry FAILED, or when the cycle ERRORED. Pure-zero
//! no-op cycles are silent.

use duckdb::Transaction;
use serde::{Deserialize, Serialize};

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};

use crate::error::QuoteIntakeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollTrigger {
    Daemon,
    Manual,
}

impl PollTrigger {
    pub fn as_audit_str(self) -> &'static str {
        match self {
            PollTrigger::Daemon => "daemon",
            PollTrigger::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuoteIntakePollPayload {
    pub idempotency_key: String,
    pub trigger: String,
    pub fetched_count: u32,
    pub created_count: u32,
    pub skipped_duplicate_count: u32,
    pub writeback_retried_count: u32,
    pub writeback_failed_count: u32,
    pub failed_count: u32,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

impl QuoteIntakePollPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

pub fn audit_kind_string() -> &'static str {
    EventKind::QuoteIntakePollCompleted.as_str()
}

pub fn should_emit(payload: &QuoteIntakePollPayload) -> bool {
    payload.fetched_count > 0
        || payload.writeback_failed_count > 0
        || payload.failed_count > 0
        || payload.error.is_some()
}

pub fn write_poll_audit_entry(
    tx: &Transaction<'_>,
    meta: &LedgerMeta,
    actor: Actor,
    payload: &QuoteIntakePollPayload,
) -> Result<(), QuoteIntakeError> {
    append_in_tx(
        tx,
        meta,
        EventKind::QuoteIntakePollCompleted,
        payload.to_bytes(),
        actor,
        Some(payload.idempotency_key.clone()),
    )
    .map(|_| ())
    .map_err(|e| QuoteIntakeError::Storage(format!("append QuoteIntakePollCompleted entry: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(fetched: u32, err: Option<String>) -> QuoteIntakePollPayload {
        QuoteIntakePollPayload {
            idempotency_key: "ulid-xxx".to_string(),
            trigger: "daemon".to_string(),
            fetched_count: fetched,
            created_count: 0,
            skipped_duplicate_count: 0,
            writeback_retried_count: 0,
            writeback_failed_count: 0,
            failed_count: 0,
            elapsed_ms: 12,
            error: err,
        }
    }

    #[test]
    fn audit_kind_string_matches_event_kind() {
        assert_eq!(
            audit_kind_string(),
            EventKind::QuoteIntakePollCompleted.as_str()
        );
    }

    #[test]
    fn should_emit_is_false_for_pure_noop_cycle() {
        assert!(!should_emit(&sample(0, None)));
    }

    #[test]
    fn should_emit_is_true_when_fetched_or_failure_or_error() {
        assert!(should_emit(&sample(1, None)));
        assert!(should_emit(&sample(0, Some("transport".to_string()))));
        let mut p = sample(0, None);
        p.failed_count = 1;
        assert!(should_emit(&p));
        let mut p = sample(0, None);
        p.writeback_failed_count = 1;
        assert!(should_emit(&p));
    }

    #[test]
    fn payload_round_trips_through_bytes() {
        let p = sample(2, None);
        let bytes = p.to_bytes();
        let back: QuoteIntakePollPayload = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn poll_trigger_audit_strings() {
        assert_eq!(PollTrigger::Daemon.as_audit_str(), "daemon");
        assert_eq!(PollTrigger::Manual.as_audit_str(), "manual");
    }
}
