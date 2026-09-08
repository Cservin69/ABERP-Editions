//! ADR-0117 — the single access-control enforcement seam + the scope-set
//! clearance model.
//!
//! An operator's clearances ARE their identity's `scope` set (ADR-0070). A
//! resource declares a [`RequiredClearance`] — a set of scope tokens. Access is
//! **granted iff `required ⊆ subject.scope`**, else **denied**. [`authorize`] is
//! the *only* place that decision is made; call sites must not inline the
//! comparison (ADR-0117 §2). The function is pure and total: no I/O, no ledger,
//! no clock — the call site fires the audit event and enforces the withhold
//! (ADR-0117 §8b).
//!
//! The required-clearance **tokens are pinned constants here** (ADR-0117 §8f) so
//! a typo at a call site cannot silently flip a policy; operator scopes remain
//! issuer-provided strings, and a token an operator lacks simply denies
//! (fail-closed).

use std::collections::BTreeSet;

use crate::cui::CuiMarking;

/// Controlled Unclassified Information clearance.
pub const SCOPE_CUI: &str = "cui";
/// National-security classification clearances (graded, but NOT a lattice —
/// ADR-0117 Open Q2: `top-secret` does not imply `secret`; a graded operator is
/// issued each token explicitly).
pub const SCOPE_CONFIDENTIAL: &str = "clearance:confidential";
pub const SCOPE_SECRET: &str = "clearance:secret";
pub const SCOPE_TOP_SECRET: &str = "clearance:top-secret";

/// The reason string recorded on a GRANT (ADR-0117 §8g) — a fixed value so the
/// grant arm is never an empty field a forensic walker must special-case.
pub const GRANT_REASON: &str = "cleared: lawful government purpose";

/// The subject of an access decision: an operator identity and the scope set it
/// was ISSUED (ADR-0117 §8a — this MUST come from
/// `DigitalIdProvider::current_operator()`, never from request input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessSubject {
    pub operator_user_id: String,
    pub scope: BTreeSet<String>,
}

impl AccessSubject {
    pub fn new<I, S>(operator_user_id: impl Into<String>, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            operator_user_id: operator_user_id.into(),
            scope: scopes.into_iter().map(Into::into).collect(),
        }
    }
}

/// The clearance a resource requires. The empty set means "no control" — any
/// authenticated operator is granted (ADR-0117 §4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequiredClearance(pub BTreeSet<String>);

impl RequiredClearance {
    /// No control required (an `Unclassified` marking, or a non-gated action).
    pub fn none() -> Self {
        Self(BTreeSet::new())
    }

    pub fn of(tokens: &[&str]) -> Self {
        Self(tokens.iter().map(|t| t.to_string()).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Derive the required clearance from a typed CUI marking (ADR-0117 §3).
    pub fn for_cui_marking(marking: &CuiMarking) -> Self {
        match marking {
            CuiMarking::Unclassified => Self::none(),
            CuiMarking::Cui(_) => Self::of(&[SCOPE_CUI]),
            CuiMarking::Confidential => Self::of(&[SCOPE_CONFIDENTIAL]),
            CuiMarking::Secret => Self::of(&[SCOPE_SECRET]),
            CuiMarking::TopSecret => Self::of(&[SCOPE_TOP_SECRET]),
        }
    }

    /// Derive from a stored canonical band string (the form
    /// `cui_markings.band` persists). Same closed vocabulary as
    /// [`Self::for_cui_marking`]; an UNKNOWN band is fail-closed to a sentinel
    /// requirement no operator can hold, so a corrupt/future band denies rather
    /// than silently grants (ADR-0117 §8c/§8f).
    pub fn for_cui_band(band: &str) -> Self {
        match band {
            "unclassified" => Self::none(),
            "cui" => Self::of(&[SCOPE_CUI]),
            "confidential" => Self::of(&[SCOPE_CONFIDENTIAL]),
            "secret" => Self::of(&[SCOPE_SECRET]),
            "top_secret" => Self::of(&[SCOPE_TOP_SECRET]),
            _ => Self::of(&["clearance:__unknown_band__"]),
        }
    }
}

/// Why access was denied (ADR-0117 §6). Extensible — a future model adds
/// reasons (revocation, time-boxing) without breaking the pinned payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    MissingClearance,
}

impl DenyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DenyReason::MissingClearance => "missing_clearance",
        }
    }
}

/// The decision. `Granted` carries the fixed grant reason; `Denied` the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    Granted,
    Denied { reason: DenyReason },
}

impl AccessDecision {
    pub fn is_granted(self) -> bool {
        matches!(self, AccessDecision::Granted)
    }

    /// The wire string for the audit `decision` field.
    pub fn decision_str(self) -> &'static str {
        match self {
            AccessDecision::Granted => "granted",
            AccessDecision::Denied { .. } => "denied",
        }
    }

    /// The audit `reason` on either arm (ADR-0117 §8g).
    pub fn reason_str(self) -> &'static str {
        match self {
            AccessDecision::Granted => GRANT_REASON,
            AccessDecision::Denied { reason } => reason.as_str(),
        }
    }
}

/// THE enforcement seam (ADR-0117 §2). Pure, total: granted iff the required
/// clearance is a subset of the subject's issued scope.
pub fn authorize(subject: &AccessSubject, required: &RequiredClearance) -> AccessDecision {
    if required.0.is_subset(&subject.scope) {
        AccessDecision::Granted
    } else {
        AccessDecision::Denied {
            reason: DenyReason::MissingClearance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cui::CuiCategory;

    #[test]
    fn empty_required_grants_any_subject() {
        // Unclassified / non-gated → grant even a scope-less operator.
        let bare = AccessSubject::new("op", Vec::<String>::new());
        assert!(authorize(&bare, &RequiredClearance::none()).is_granted());
    }

    #[test]
    fn subset_grants_superset_or_equal() {
        let cleared = AccessSubject::new("op", ["operator", "cui"]);
        assert!(authorize(&cleared, &RequiredClearance::of(&[SCOPE_CUI])).is_granted());
        // Extra scopes don't hurt.
        let more = AccessSubject::new("op", ["operator", "cui", "signer"]);
        assert!(authorize(&more, &RequiredClearance::of(&[SCOPE_CUI])).is_granted());
    }

    #[test]
    fn missing_scope_denies_with_reason() {
        let bare = AccessSubject::new("op", ["operator"]);
        let d = authorize(&bare, &RequiredClearance::of(&[SCOPE_CUI]));
        assert_eq!(
            d,
            AccessDecision::Denied {
                reason: DenyReason::MissingClearance
            }
        );
        assert_eq!(d.decision_str(), "denied");
        assert_eq!(d.reason_str(), "missing_clearance");
    }

    #[test]
    fn top_secret_does_not_imply_secret() {
        // ADR-0117 Open Q2 — tokens are flat, not a lattice.
        let ts_only = AccessSubject::new("op", ["operator", SCOPE_TOP_SECRET]);
        assert!(!authorize(&ts_only, &RequiredClearance::of(&[SCOPE_SECRET])).is_granted());
        // The explicitly graded operator holds both.
        let graded = AccessSubject::new("op", ["operator", SCOPE_SECRET, SCOPE_TOP_SECRET]);
        assert!(authorize(&graded, &RequiredClearance::of(&[SCOPE_SECRET])).is_granted());
    }

    #[test]
    fn for_cui_marking_maps_bands_to_the_pinned_tokens() {
        assert!(RequiredClearance::for_cui_marking(&CuiMarking::Unclassified).is_empty());
        assert_eq!(
            RequiredClearance::for_cui_marking(&CuiMarking::Cui(CuiCategory::Cti)),
            RequiredClearance::of(&[SCOPE_CUI])
        );
        assert_eq!(
            RequiredClearance::for_cui_marking(&CuiMarking::Secret),
            RequiredClearance::of(&[SCOPE_SECRET])
        );
    }

    #[test]
    fn for_cui_band_matches_for_cui_marking_and_fails_closed_on_unknown() {
        assert!(RequiredClearance::for_cui_band("unclassified").is_empty());
        assert_eq!(
            RequiredClearance::for_cui_band("cui"),
            RequiredClearance::of(&[SCOPE_CUI])
        );
        // An unknown band requires a token no operator can hold → deny.
        let bare = AccessSubject::new("op", ["operator", "cui", SCOPE_SECRET]);
        assert!(!authorize(&bare, &RequiredClearance::for_cui_band("mystery")).is_granted());
    }

    #[test]
    fn grant_reason_is_the_fixed_lawful_purpose_string() {
        assert_eq!(AccessDecision::Granted.reason_str(), GRANT_REASON);
        assert_eq!(AccessDecision::Granted.decision_str(), "granted");
    }
}
