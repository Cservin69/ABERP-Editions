//! S2 / ADR-0093 — standalone BOTH-ARMS proof of the storefront-reach gate
//! DECISION logic.
//!
//! `serve.rs` / `cad_blob.rs` / `catalogue_push.rs` are DuckDB/HTTP-backed and
//! cannot be built in the 45s/4GB sandbox, so the gate's pure decision is
//! extracted here and `rustc --test`-ed for BOTH edition arms:
//!
//! ```text
//!   rustc --test --edition 2021                      <this>   # Portable arm
//!   rustc --test --edition 2021 --cfg defense_arm    <this>   # Defense  arm
//! ```
//!
//! The decision below is byte-identical to
//! `apps/aberp/src/build_profile.rs::storefront_polling_allowed_for` — the
//! cut-gate (CHECK 8) asserts both files carry the same
//! `matches!(edition, Edition::Defense)` rule so this proof cannot drift from
//! the source of truth. Run via `tools/run_storefront_gate_probe.sh`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Prod,
    Defense,
    Portable,
}

/// THE storefront-reach decision — identical to
/// `build_profile::storefront_polling_allowed_for`: ONLY Defense.
pub const fn storefront_polling_allowed_for(edition: Edition) -> bool {
    matches!(edition, Edition::Defense)
}

/// This "build"'s edition, driven by a cfg flag so a SINGLE source proves
/// both compile arms (the real binary pins this from the `production`
/// feature).
#[cfg(defense_arm)]
pub const EDITION: Edition = Edition::Defense;
#[cfg(not(defense_arm))]
pub const EDITION: Edition = Edition::Portable;

pub const fn storefront_polling_allowed() -> bool {
    storefront_polling_allowed_for(EDITION)
}

/// Pure-bool form of the `assert_storefront_reach_allowed` backstop: a
/// non-Defense edition REFUSES storefront reach.
pub fn storefront_reach_refused_for(edition: Edition) -> bool {
    !storefront_polling_allowed_for(edition)
}

fn main() {
    // Non-test build is a no-op; the proof lives in `--test`.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_total_over_edition_both_arms() {
        // ONLY Defense may reach the storefront — proven for EVERY edition
        // value in a single compile (independent of which arm we built).
        assert!(storefront_polling_allowed_for(Edition::Defense));
        assert!(!storefront_polling_allowed_for(Edition::Portable));
        assert!(!storefront_polling_allowed_for(Edition::Prod));
        // The refusal backstop is the exact negation.
        assert!(!storefront_reach_refused_for(Edition::Defense));
        assert!(storefront_reach_refused_for(Edition::Portable));
        assert!(storefront_reach_refused_for(Edition::Prod));
    }

    #[cfg(not(defense_arm))]
    #[test]
    fn portable_arm_refuses_storefront_reach() {
        assert_eq!(EDITION, Edition::Portable);
        assert!(!storefront_polling_allowed());
    }

    #[cfg(defense_arm)]
    #[test]
    fn defense_arm_allows_storefront_reach() {
        assert_eq!(EDITION, Edition::Defense);
        assert!(storefront_polling_allowed());
    }
}
