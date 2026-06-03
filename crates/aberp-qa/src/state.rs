//! QA-inspection state machine per ADR-0063 §1 + §"Adversarial review" #3.
//!
//! Pure function: given the current `QaState` and an operator/adapter
//! `QaDecision`, returns the destination `QaState` or a typed error
//! naming the refused edge. The route layer surfaces refusal as 400;
//! [[trust-code-not-operator]] — the SPA hides the disallowed buttons
//! but a curl bypassing the SPA still gets refused loud.
//!
//! ## Why `Reworking → Passed` is allowed
//!
//! ADR-0063 §1's storage-string table omits Reworking from the "Passed
//! allowed FROM" column, but §"Adversarial review" #3 names
//! `Failed → Reworking → Passed` as the canonical rework-succeeds path.
//! The table is internally inconsistent; we implement the prose. The
//! PR-229 body calls this out for the next ADR revision.

use thiserror::Error;

use crate::types::{QaDecision, QaState};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QaStateError {
    /// The operator / adapter asked for an edge the lifecycle does
    /// not allow.
    #[error("illegal QA transition: {from:?} cannot {decision:?}")]
    IllegalTransition { from: QaState, decision: QaDecision },
}

/// Pure transition function per ADR-0063 §1 + §"Adversarial review" #3.
///
/// Allowed edges:
///
/// ```text
/// Pending   → Pass    → Passed
/// Pending   → Fail    → Failed
/// Passed    → Fail    → Failed     (operator catches a defect after-the-fact)
/// Failed    → Rework  → Reworking
/// Failed    → Dispose → Disposed
/// Reworking → Pass    → Passed     (rework succeeded — ADR-0063 §"Adversarial review" #3)
/// Reworking → Dispose → Disposed   (rework failed too)
/// ```
///
/// Disposed is terminal — every decision against it is refused.
/// Passed → Pass / Fail → Rework-from-Pending / Pending → Rework etc.
/// are all illegal per the §1 table.
pub fn next_qa_state(current: QaState, decision: QaDecision) -> Result<QaState, QaStateError> {
    use QaDecision as D;
    use QaState as S;
    match (current, decision) {
        // Pending → Passed | Failed
        (S::Pending, D::Pass) => Ok(S::Passed),
        (S::Pending, D::Fail) => Ok(S::Failed),
        // Passed → Failed (operator after-the-fact catch)
        (S::Passed, D::Fail) => Ok(S::Failed),
        // Failed → Reworking | Disposed
        (S::Failed, D::Rework) => Ok(S::Reworking),
        (S::Failed, D::Dispose) => Ok(S::Disposed),
        // Reworking → Passed (rework succeeded) | Disposed (rework failed too)
        (S::Reworking, D::Pass) => Ok(S::Passed),
        (S::Reworking, D::Dispose) => Ok(S::Disposed),
        // Every other (state, decision) pair is illegal.
        (from, decision) => Err(QaStateError::IllegalTransition { from, decision }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_valid_edge_per_adr_0063_section_1_yields_expected_next() {
        use QaDecision as D;
        use QaState as S;
        let valid = [
            (S::Pending, D::Pass, S::Passed),
            (S::Pending, D::Fail, S::Failed),
            (S::Passed, D::Fail, S::Failed),
            (S::Failed, D::Rework, S::Reworking),
            (S::Failed, D::Dispose, S::Disposed),
            (S::Reworking, D::Pass, S::Passed),
            (S::Reworking, D::Dispose, S::Disposed),
        ];
        for (from, decision, expected_to) in valid {
            let got = next_qa_state(from, decision)
                .unwrap_or_else(|e| panic!("expected {from:?}+{decision:?} ok, got {e:?}"));
            assert_eq!(got, expected_to, "{from:?} + {decision:?}");
        }
    }

    /// Pin every illegal edge per the §1 table + the
    /// `decide_qa_refuses_illegal_state_pair` invariant.
    #[test]
    fn every_illegal_edge_is_refused() {
        use QaDecision as D;
        use QaState as S;
        let all_states = [S::Pending, S::Passed, S::Failed, S::Reworking, S::Disposed];
        let all_decisions = [D::Pass, D::Fail, D::Rework, D::Dispose];
        let valid_set: &[(S, D)] = &[
            (S::Pending, D::Pass),
            (S::Pending, D::Fail),
            (S::Passed, D::Fail),
            (S::Failed, D::Rework),
            (S::Failed, D::Dispose),
            (S::Reworking, D::Pass),
            (S::Reworking, D::Dispose),
        ];
        for from in all_states {
            for decision in all_decisions {
                let is_valid = valid_set.iter().any(|(s, d)| *s == from && *d == decision);
                let result = next_qa_state(from, decision);
                if is_valid {
                    assert!(result.is_ok(), "{from:?}+{decision:?} should be ok");
                } else {
                    assert!(
                        matches!(result, Err(QaStateError::IllegalTransition { .. })),
                        "{from:?}+{decision:?} should be refused, got {result:?}"
                    );
                }
            }
        }
    }

    /// Disposed is terminal — every decision against it is refused.
    #[test]
    fn disposed_refuses_every_decision() {
        for d in [
            QaDecision::Pass,
            QaDecision::Fail,
            QaDecision::Rework,
            QaDecision::Dispose,
        ] {
            let r = next_qa_state(QaState::Disposed, d);
            assert!(matches!(r, Err(QaStateError::IllegalTransition { .. })));
        }
    }

    /// Direct `Failed → Pass` is refused — operator must go through
    /// Reworking first per ADR-0063 §"Adversarial review" #3.
    #[test]
    fn failed_to_pass_directly_is_refused() {
        let r = next_qa_state(QaState::Failed, QaDecision::Pass);
        assert!(matches!(r, Err(QaStateError::IllegalTransition { .. })));
    }
}
