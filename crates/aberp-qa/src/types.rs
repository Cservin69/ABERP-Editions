//! Closed-vocab enums for the QA queue per ADR-0063 §1.
//!
//! Two enums:
//!
//! - [`QaState`] — the inspection lifecycle vocab. Pending → Passed,
//!   Pending → Failed, Passed → Failed (operator after-the-fact catch),
//!   Failed → Reworking, Reworking → Passed (rework succeeded — see
//!   note below), Failed → Disposed, Reworking → Disposed.
//! - [`QaDecision`] — the operator-button / adapter-trigger verb that
//!   drives a decision. Closed vocab so a POST body parses unambiguously.
//!
//! ## Why `Reworking → Passed` is in the table
//!
//! ADR-0063 §1's storage-string table omits `Reworking` from the
//! "Passed allowed FROM" column but §"Adversarial review" #3 names
//! `Failed → Reworking → Passed` as the valid rework-succeeds path.
//! The table is internally inconsistent — the ADR's §6 + §"Adversarial
//! review" prose treats `Reworking → Passed` as the canonical success
//! path. We implement the prose (allow `Reworking → Passed`) and flag
//! the ADR-table inconsistency in the PR-229 body so the next ADR
//! revision can resolve the surface; pushing back per [[pushback-as-method]].

use serde::{Deserialize, Serialize};

/// Inspection lifecycle vocab per ADR-0063 §1.
///
/// ```text
/// Pending → Passed
/// Pending → Failed
/// Passed  → Failed     (operator catches a defect after-the-fact)
/// Failed  → Reworking  (operator triggers rework — also flips upstream routing-op back to Active)
/// Failed  → Disposed   (scrap; emits Scrap stock_movement per ADR-0063 §6)
/// Reworking → Passed   (rework succeeded — ADR-0063 §"Adversarial review" #3)
/// Reworking → Disposed (rework failed too — emits Scrap)
/// ```
///
/// Pinned by [`crate::state::next_qa_state`]; no DB CHECK per
/// [[no-sql-specific]].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaState {
    Pending,
    Passed,
    Failed,
    Reworking,
    Disposed,
}

impl QaState {
    /// On-disk / wire string per ADR-0063 §1 table.
    pub fn as_str(&self) -> &'static str {
        match self {
            QaState::Pending => "pending",
            QaState::Passed => "passed",
            QaState::Failed => "failed",
            QaState::Reworking => "reworking",
            QaState::Disposed => "disposed",
        }
    }

    /// Parse from the on-disk / wire string. Errors loud per
    /// CLAUDE.md rule 12.
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "pending" => Ok(QaState::Pending),
            "passed" => Ok(QaState::Passed),
            "failed" => Ok(QaState::Failed),
            "reworking" => Ok(QaState::Reworking),
            "disposed" => Ok(QaState::Disposed),
            _ => Err("unknown QaState storage string"),
        }
    }

    /// `true` once the inspection has been decided (any state except
    /// Pending). The QA queue's `Pending` filter is the SPA default
    /// per ADR-0063 §8.
    pub fn is_decided(&self) -> bool {
        !matches!(self, QaState::Pending)
    }
}

/// The operator-button / adapter-trigger verb that drives a decision.
/// Maps 1:1 onto a destination `QaState` (Pass→Passed, Fail→Failed,
/// Rework→Reworking, Dispose→Disposed). Closed vocab so a POST body
/// like `{ "decision": "pass" }` parses unambiguously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaDecision {
    Pass,
    Fail,
    Rework,
    Dispose,
}

impl QaDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            QaDecision::Pass => "pass",
            QaDecision::Fail => "fail",
            QaDecision::Rework => "rework",
            QaDecision::Dispose => "dispose",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "pass" => Ok(QaDecision::Pass),
            "fail" => Ok(QaDecision::Fail),
            "rework" => Ok(QaDecision::Rework),
            "dispose" => Ok(QaDecision::Dispose),
            _ => Err("unknown QaDecision storage string"),
        }
    }

    /// The destination [`QaState`] this decision maps to.
    pub fn to_state(&self) -> QaState {
        match self {
            QaDecision::Pass => QaState::Passed,
            QaDecision::Fail => QaState::Failed,
            QaDecision::Rework => QaState::Reworking,
            QaDecision::Dispose => QaState::Disposed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qa_state_round_trip_for_every_variant() {
        let variants = [
            QaState::Pending,
            QaState::Passed,
            QaState::Failed,
            QaState::Reworking,
            QaState::Disposed,
        ];
        for v in variants {
            let s = v.as_str();
            let back = QaState::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn qa_state_rejects_unknown_string() {
        assert!(QaState::from_storage_str("not_a_state").is_err());
        assert!(QaState::from_storage_str("").is_err());
    }

    /// Pin the wire shape — snake_case tokens MUST match the storage
    /// strings byte-for-byte. A future contributor swapping `rename_all`
    /// would silently desync; this pin fires loudly first. Same posture
    /// as `aberp_inventory::types::tests::movement_reason_serde_matches_storage_string`.
    #[test]
    fn qa_state_serde_matches_storage_string() {
        for v in [
            QaState::Pending,
            QaState::Passed,
            QaState::Failed,
            QaState::Reworking,
            QaState::Disposed,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let inside = json.trim_matches('"');
            assert_eq!(inside, v.as_str());
        }
    }

    #[test]
    fn qa_decision_round_trip_for_every_variant() {
        let variants = [
            QaDecision::Pass,
            QaDecision::Fail,
            QaDecision::Rework,
            QaDecision::Dispose,
        ];
        for v in variants {
            let s = v.as_str();
            let back = QaDecision::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn qa_decision_to_state_matches_adr_0063_section_1() {
        assert_eq!(QaDecision::Pass.to_state(), QaState::Passed);
        assert_eq!(QaDecision::Fail.to_state(), QaState::Failed);
        assert_eq!(QaDecision::Rework.to_state(), QaState::Reworking);
        assert_eq!(QaDecision::Dispose.to_state(), QaState::Disposed);
    }

    #[test]
    fn is_decided_returns_true_for_every_state_except_pending() {
        assert!(!QaState::Pending.is_decided());
        for v in [
            QaState::Passed,
            QaState::Failed,
            QaState::Reworking,
            QaState::Disposed,
        ] {
            assert!(v.is_decided(), "{v:?} must be decided");
        }
    }
}
