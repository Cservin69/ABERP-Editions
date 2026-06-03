//! Dispatch-state transition machine per ADR-0064 §1 table.
//!
//! Pure function: given the current [`DispatchState`] and the operator
//! / adapter action, returns the destination state or a typed error
//! naming the refused edge. The route layer surfaces refusal as 400;
//! [[trust-code-not-operator]] — the SPA hides the disallowed buttons
//! but a curl bypassing the SPA still gets refused loud.

use thiserror::Error;

use crate::types::DispatchState;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DispatchStateError {
    /// The operator / adapter asked for an edge the lifecycle does not
    /// allow.
    #[error("illegal dispatch transition: {from:?} cannot {action:?}")]
    IllegalTransition {
        from: DispatchState,
        action: DispatchAction,
    },
}

/// The operator-button / adapter-trigger verb that drives a transition.
/// Mirrors [`crate::types::DispatchState`] semantics: `Ship` flips
/// Drafted → Shipped (firing all the side-effects in the same tx);
/// `Cancel` flips Drafted → Cancelled (no side-effects).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DispatchAction {
    Ship,
    Cancel,
}

/// Pure transition function per ADR-0064 §1 table.
///
/// Allowed edges:
///
/// ```text
/// Drafted  → Ship   → Shipped
/// Drafted  → Cancel → Cancelled
/// ```
///
/// Shipped and Cancelled are terminal — every action against them is
/// refused.
pub fn next_dispatch_state(
    current: DispatchState,
    action: DispatchAction,
) -> Result<DispatchState, DispatchStateError> {
    use DispatchAction as A;
    use DispatchState as S;
    match (current, action) {
        (S::Drafted, A::Ship) => Ok(S::Shipped),
        (S::Drafted, A::Cancel) => Ok(S::Cancelled),
        (from, action) => Err(DispatchStateError::IllegalTransition { from, action }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_valid_edge_yields_expected_next() {
        assert_eq!(
            next_dispatch_state(DispatchState::Drafted, DispatchAction::Ship),
            Ok(DispatchState::Shipped)
        );
        assert_eq!(
            next_dispatch_state(DispatchState::Drafted, DispatchAction::Cancel),
            Ok(DispatchState::Cancelled)
        );
    }

    /// Shipped is terminal — every action against it is refused.
    #[test]
    fn shipped_refuses_every_action() {
        for a in [DispatchAction::Ship, DispatchAction::Cancel] {
            let r = next_dispatch_state(DispatchState::Shipped, a);
            assert!(matches!(
                r,
                Err(DispatchStateError::IllegalTransition { .. })
            ));
        }
    }

    /// Cancelled is terminal — every action against it is refused.
    #[test]
    fn cancelled_refuses_every_action() {
        for a in [DispatchAction::Ship, DispatchAction::Cancel] {
            let r = next_dispatch_state(DispatchState::Cancelled, a);
            assert!(matches!(
                r,
                Err(DispatchStateError::IllegalTransition { .. })
            ));
        }
    }
}
