//! Closed-vocab enums + transition table for the Work Orders module
//! per ADR-0062 §2.
//!
//! Three enums:
//!
//! - [`WorkOrderState`] — the regulated lifecycle vocab pinned in
//!   ADR-0060 §1 and re-declared in ADR-0062 §2 as canonical.
//! - [`RoutingOpState`] — narrower per-operation vocab.
//! - [`WoAction`] — the operator-button (or adapter-trigger) verb that
//!   drives a state transition. Closed vocab so the route layer can
//!   round-trip from JSON without a free-form string.
//!
//! All three round-trip through the storage strings named in
//! ADR-0062 §2 + §"Cross-cutting decisions" #2 ("no DB-engine
//! specifics" — the transition table lives in Rust, not in a CHECK).
//!
//! ## Why these are not built from serde derives
//!
//! The DB layer stores state as plain VARCHAR (no DB CHECK per
//! `[[no-sql-specific]]`). The round-trip pair is exercised at every
//! read site; pairing `as_str` + `from_storage_str` mirrors the
//! `aberp_audit_ledger::EventKind` F12 ritual + the
//! `aberp_inventory::types::MovementReason` two-surface pin posture.
//! A naked `#[derive(Serialize, Deserialize)]` would collapse the
//! as_str surface into one source of truth — but then SQL
//! `WHERE state = 'released'` queries scattered across the binary
//! would silently break on a rename. The two-surface pin is the
//! load-bearing posture.

use serde::{Deserialize, Serialize};

/// The regulated Work Order lifecycle vocab per ADR-0062 §2.
///
/// ```text
/// Created → Released → InProgress → Completed
///                            ↘ Cancelled
///                            ↘ OnHold  → InProgress  (resume)
///                                      → Cancelled
/// ```
///
/// Transition validity is enforced by [`crate::state::next_state`];
/// no DB CHECK constraint per [[no-sql-specific]].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkOrderState {
    Created,
    Released,
    InProgress,
    Completed,
    Cancelled,
    OnHold,
}

impl WorkOrderState {
    /// On-disk / wire string per ADR-0062 §2 table.
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkOrderState::Created => "created",
            WorkOrderState::Released => "released",
            WorkOrderState::InProgress => "in_progress",
            WorkOrderState::Completed => "completed",
            WorkOrderState::Cancelled => "cancelled",
            WorkOrderState::OnHold => "on_hold",
        }
    }

    /// Parse from the on-disk / wire string. Errors loud on anything
    /// outside the closed vocab per CLAUDE.md rule 12.
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "created" => Ok(WorkOrderState::Created),
            "released" => Ok(WorkOrderState::Released),
            "in_progress" => Ok(WorkOrderState::InProgress),
            "completed" => Ok(WorkOrderState::Completed),
            "cancelled" => Ok(WorkOrderState::Cancelled),
            "on_hold" => Ok(WorkOrderState::OnHold),
            _ => Err("unknown WorkOrderState storage string"),
        }
    }

    /// Terminal states cannot transition further. Used by the SPA to
    /// disable action buttons and by the handler as a defence-in-depth
    /// refuse.
    pub fn is_terminal(&self) -> bool {
        matches!(self, WorkOrderState::Completed | WorkOrderState::Cancelled)
    }
}

/// Per-operation lifecycle vocab per ADR-0062 §2 (narrower than the
/// WO-level vocab). `Active` is set only when the prior op completed
/// AND the parent WO is `InProgress` — both conditions are checked at
/// the transition handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingOpState {
    Pending,
    Active,
    Completed,
    Skipped,
}

impl RoutingOpState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutingOpState::Pending => "pending",
            RoutingOpState::Active => "active",
            RoutingOpState::Completed => "completed",
            RoutingOpState::Skipped => "skipped",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "pending" => Ok(RoutingOpState::Pending),
            "active" => Ok(RoutingOpState::Active),
            "completed" => Ok(RoutingOpState::Completed),
            "skipped" => Ok(RoutingOpState::Skipped),
            _ => Err("unknown RoutingOpState storage string"),
        }
    }
}

/// The operator-button (or future adapter-trigger) verb. Closed vocab
/// so a POST body like `{ "action": "release" }` parses
/// unambiguously; unknown actions surface as a 400 at the route
/// boundary per [[trust-code-not-operator]].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WoAction {
    /// Created → Released. The Release handler snapshots the active
    /// BOM rows + emits one `BomConsumption` `stock_movement` per row
    /// (ADR-0062 §5) — all in one transaction with the state update.
    Release,
    /// Released → InProgress.
    Start,
    /// InProgress → Completed. The Complete handler emits one
    /// `WoCompletion` `stock_movement` for the finished good
    /// (ADR-0062 §5).
    Complete,
    /// {Created, Released, InProgress, OnHold} → Cancelled.
    Cancel,
    /// {Released, InProgress} → OnHold. The operator may supply a
    /// `hold_reason` (stored on the row + audit payload).
    Hold,
    /// OnHold → InProgress.
    Resume,
}

/// S233 / PR-229 — the per-routing-op verb. Closed vocab so the route
/// layer can round-trip a `{ "action": "complete" }` body
/// unambiguously. v1 has just `Complete`; future widening (per-op
/// `Skip` per ADR-0062 §2's table) extends here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingOpAction {
    /// Active → Completed. Cascades the next op (by sequence) Pending
    /// → Active per ADR-0062 §2; auto-creates a Pending qa_inspection
    /// per ADR-0063 §2.
    Complete,
}

impl RoutingOpAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutingOpAction::Complete => "complete",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "complete" => Ok(RoutingOpAction::Complete),
            _ => Err("unknown RoutingOpAction storage string"),
        }
    }
}

impl WoAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            WoAction::Release => "release",
            WoAction::Start => "start",
            WoAction::Complete => "complete",
            WoAction::Cancel => "cancel",
            WoAction::Hold => "hold",
            WoAction::Resume => "resume",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "release" => Ok(WoAction::Release),
            "start" => Ok(WoAction::Start),
            "complete" => Ok(WoAction::Complete),
            "cancel" => Ok(WoAction::Cancel),
            "hold" => Ok(WoAction::Hold),
            "resume" => Ok(WoAction::Resume),
            _ => Err("unknown WoAction storage string"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_order_state_round_trip_for_every_variant() {
        let variants = [
            WorkOrderState::Created,
            WorkOrderState::Released,
            WorkOrderState::InProgress,
            WorkOrderState::Completed,
            WorkOrderState::Cancelled,
            WorkOrderState::OnHold,
        ];
        for v in variants {
            let s = v.as_str();
            let back = WorkOrderState::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn work_order_state_rejects_unknown_string() {
        assert!(WorkOrderState::from_storage_str("not_a_state").is_err());
        assert!(WorkOrderState::from_storage_str("").is_err());
    }

    /// The serde rename_all = "snake_case" tokens MUST match the storage
    /// strings byte-for-byte — same posture as
    /// `aberp_inventory::types::tests::movement_reason_serde_matches_storage_string`.
    #[test]
    fn work_order_state_serde_matches_storage_string() {
        for v in [
            WorkOrderState::Created,
            WorkOrderState::Released,
            WorkOrderState::InProgress,
            WorkOrderState::Completed,
            WorkOrderState::Cancelled,
            WorkOrderState::OnHold,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let inside = json.trim_matches('"');
            assert_eq!(inside, v.as_str());
        }
    }

    #[test]
    fn routing_op_state_round_trip_for_every_variant() {
        let variants = [
            RoutingOpState::Pending,
            RoutingOpState::Active,
            RoutingOpState::Completed,
            RoutingOpState::Skipped,
        ];
        for v in variants {
            let s = v.as_str();
            let back = RoutingOpState::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn wo_action_round_trip_for_every_variant() {
        let variants = [
            WoAction::Release,
            WoAction::Start,
            WoAction::Complete,
            WoAction::Cancel,
            WoAction::Hold,
            WoAction::Resume,
        ];
        for v in variants {
            let s = v.as_str();
            let back = WoAction::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn terminal_states_are_completed_and_cancelled() {
        assert!(WorkOrderState::Completed.is_terminal());
        assert!(WorkOrderState::Cancelled.is_terminal());
        for s in [
            WorkOrderState::Created,
            WorkOrderState::Released,
            WorkOrderState::InProgress,
            WorkOrderState::OnHold,
        ] {
            assert!(!s.is_terminal(), "{s:?} must not be terminal");
        }
    }
}
