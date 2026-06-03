//! Work-order state machine per ADR-0062 §2.
//!
//! The transition table is the application-layer invariant — no DB
//! CHECK enforces it per [[no-sql-specific]] + ADR-0062 §"Cross-cutting
//! decisions" #2. [`next_state`] is a pure function: given the current
//! state and an operator/adapter action, returns the next state or a
//! typed error naming the refused edge.
//!
//! `transition_wo` (the repository-level write path in
//! `handlers.rs`) consults `next_state` as its first gate; illegal
//! transitions are refused with [`WoStateError::IllegalTransition`]
//! BEFORE any DB write per the [[trust-code-not-operator]] +
//! `loud-fail` posture.

use thiserror::Error;

use crate::types::{RoutingOpAction, RoutingOpState, WoAction, WorkOrderState};

/// Errors raised by [`next_state`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WoStateError {
    /// The operator/adapter asked for an edge the lifecycle does not
    /// allow. Carries both halves so the SPA can render an explicit
    /// "cannot release from in_progress" diagnostic.
    #[error("illegal transition: {from:?} cannot {action:?}")]
    IllegalTransition {
        from: WorkOrderState,
        action: WoAction,
    },
}

/// Pure transition function per ADR-0062 §2.
///
/// Lifecycle (re-stated for context):
///
/// ```text
/// Created → Released → InProgress → Completed
///                            ↘ Cancelled
///                            ↘ OnHold  → InProgress  (resume)
///                                      → Cancelled
/// ```
///
/// Per ADR-0062 §2 the `Cancel` action is allowed from every
/// non-terminal state (Created / Released / InProgress / OnHold);
/// from terminal states (Completed / Cancelled) every action is
/// refused — there is no further lifecycle for a terminal WO.
pub fn next_state(
    current: WorkOrderState,
    action: WoAction,
) -> Result<WorkOrderState, WoStateError> {
    use WoAction as A;
    use WorkOrderState as S;
    match (current, action) {
        // Release: Created → Released
        (S::Created, A::Release) => Ok(S::Released),
        // Start: Released → InProgress
        (S::Released, A::Start) => Ok(S::InProgress),
        // Complete: InProgress → Completed
        (S::InProgress, A::Complete) => Ok(S::Completed),
        // Hold: Released → OnHold | InProgress → OnHold
        (S::Released, A::Hold) | (S::InProgress, A::Hold) => Ok(S::OnHold),
        // Resume: OnHold → InProgress
        (S::OnHold, A::Resume) => Ok(S::InProgress),
        // Cancel: Created / Released / InProgress / OnHold → Cancelled
        (S::Created, A::Cancel)
        | (S::Released, A::Cancel)
        | (S::InProgress, A::Cancel)
        | (S::OnHold, A::Cancel) => Ok(S::Cancelled),
        // Every other (state, action) pair is an illegal edge.
        (from, action) => Err(WoStateError::IllegalTransition { from, action }),
    }
}

/// S233 / PR-229 — illegal per-op transition. Surface at the route
/// layer as 400, same as [`WoStateError::IllegalTransition`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RoutingOpStateError {
    #[error("illegal routing-op transition: {from:?} cannot {action:?}")]
    IllegalTransition {
        from: RoutingOpState,
        action: RoutingOpAction,
    },
}

/// S233 / PR-229 — pure per-routing-op transition function. The
/// cascade itself (next op Pending → Active when this one flips
/// to Completed) lives in
/// [`crate::repository::transition_routing_op`]; this function
/// just gates the per-op edge.
///
/// Lifecycle (re-stated per ADR-0062 §2 + S233 brief Part A):
///
/// ```text
/// Pending → Active → Completed   (Skipped is reserved; no v1 button drives it)
/// ```
///
/// `Active → Completed` is the only operator-driven edge today. The
/// `Pending → Active` step is cascade-driven (not operator-driven) by
/// the WO Release handler and by the per-op Complete cascade.
pub fn next_routing_op_state(
    current: RoutingOpState,
    action: RoutingOpAction,
) -> Result<RoutingOpState, RoutingOpStateError> {
    use RoutingOpAction as A;
    use RoutingOpState as S;
    match (current, action) {
        (S::Active, A::Complete) => Ok(S::Completed),
        (from, action) => Err(RoutingOpStateError::IllegalTransition { from, action }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0062 §2 table — pin every VALID edge. If a future
    /// contributor relaxes (or tightens) the transition table without
    /// updating this list, the regression fires loudly per CLAUDE.md
    /// rule 9 (tests verify intent, not just behaviour).
    #[test]
    fn every_valid_edge_per_adr_0062_section_2_yields_expected_next() {
        use WoAction as A;
        use WorkOrderState as S;
        let valid = [
            (S::Created, A::Release, S::Released),
            (S::Released, A::Start, S::InProgress),
            (S::InProgress, A::Complete, S::Completed),
            (S::Released, A::Hold, S::OnHold),
            (S::InProgress, A::Hold, S::OnHold),
            (S::OnHold, A::Resume, S::InProgress),
            (S::Created, A::Cancel, S::Cancelled),
            (S::Released, A::Cancel, S::Cancelled),
            (S::InProgress, A::Cancel, S::Cancelled),
            (S::OnHold, A::Cancel, S::Cancelled),
        ];
        for (from, action, expected_to) in valid {
            let got = next_state(from, action)
                .unwrap_or_else(|e| panic!("expected {from:?}+{action:?} ok, got {e:?}"));
            assert_eq!(got, expected_to, "{from:?} + {action:?}");
        }
    }

    /// Defence-in-depth: enumerate EVERY (state, action) pair and
    /// verify the ones NOT in the valid edge list all fail with
    /// `IllegalTransition`. Catches a future widening that
    /// accidentally lets `Created → Completed` through.
    #[test]
    fn every_illegal_edge_is_refused() {
        use WoAction as A;
        use WorkOrderState as S;
        let all_states = [
            S::Created,
            S::Released,
            S::InProgress,
            S::Completed,
            S::Cancelled,
            S::OnHold,
        ];
        let all_actions = [
            A::Release,
            A::Start,
            A::Complete,
            A::Cancel,
            A::Hold,
            A::Resume,
        ];
        let valid_set: &[(S, A)] = &[
            (S::Created, A::Release),
            (S::Released, A::Start),
            (S::InProgress, A::Complete),
            (S::Released, A::Hold),
            (S::InProgress, A::Hold),
            (S::OnHold, A::Resume),
            (S::Created, A::Cancel),
            (S::Released, A::Cancel),
            (S::InProgress, A::Cancel),
            (S::OnHold, A::Cancel),
        ];
        for from in all_states {
            for action in all_actions {
                let is_valid = valid_set.iter().any(|(s, a)| *s == from && *a == action);
                let result = next_state(from, action);
                if is_valid {
                    assert!(result.is_ok(), "{from:?}+{action:?} should be ok");
                } else {
                    assert!(
                        matches!(result, Err(WoStateError::IllegalTransition { .. })),
                        "{from:?}+{action:?} should be refused, got {result:?}"
                    );
                }
            }
        }
    }

    /// S233 / PR-229 — `next_routing_op_state` pure function.
    #[test]
    fn routing_op_cascade_complete_from_active_yields_completed() {
        let got = next_routing_op_state(RoutingOpState::Active, RoutingOpAction::Complete).unwrap();
        assert_eq!(got, RoutingOpState::Completed);
    }

    #[test]
    fn routing_op_cascade_complete_from_pending_is_refused() {
        let err =
            next_routing_op_state(RoutingOpState::Pending, RoutingOpAction::Complete).unwrap_err();
        assert!(matches!(err, RoutingOpStateError::IllegalTransition { .. }));
    }

    #[test]
    fn routing_op_cascade_complete_from_completed_is_refused() {
        let err = next_routing_op_state(RoutingOpState::Completed, RoutingOpAction::Complete)
            .unwrap_err();
        assert!(matches!(err, RoutingOpStateError::IllegalTransition { .. }));
    }

    /// Specifically: Created → Completed is the most obvious illegal
    /// edge (no work has happened). Pin it explicitly per the brief.
    #[test]
    fn created_cannot_complete() {
        let err = next_state(WorkOrderState::Created, WoAction::Complete).unwrap_err();
        assert!(matches!(err, WoStateError::IllegalTransition { .. }));
    }

    /// Terminal states are inert: no action moves a Completed or
    /// Cancelled WO anywhere.
    #[test]
    fn terminal_states_refuse_every_action() {
        for terminal in [WorkOrderState::Completed, WorkOrderState::Cancelled] {
            for action in [
                WoAction::Release,
                WoAction::Start,
                WoAction::Complete,
                WoAction::Cancel,
                WoAction::Hold,
                WoAction::Resume,
            ] {
                let result = next_state(terminal, action);
                assert!(
                    matches!(result, Err(WoStateError::IllegalTransition { .. })),
                    "{terminal:?}+{action:?} should be refused on terminal state"
                );
            }
        }
    }
}
