//! ADR-0098 R1 — faithful `rustc --test` extraction of the PURE decision cores
//! (Fable-5 findings D + E + G). audit-ledger + aberp-snapshot are DuckDB-linked
//! (bundled libduckdb amalgamation) → they cannot `cargo build` in the saw-off
//! sandbox, so the load-bearing PURE logic is copied VERBATIM here and proven
//! with `rustc --test`. The serde/fs/DuckDB wrappers (preserve/trim I/O, IMPORT/
//! replay/verify_chain) are exercised by `cargo test` on the Mac/CI gate.
//
// Provenance (copied verbatim from the branch adr0098-remediation):
//   decide_tail / TailDecision            <- crates/audit-ledger/src/mirror.rs
//   route_guard / GuardRoute              <- crates/aberp-snapshot/src/recover.rs
//   first_overlap_disagreement            <- crates/aberp-snapshot/src/recover.rs
//   overlap_is_genesis_anchored           <- crates/aberp-snapshot/src/recover.rs
// The `*_action` / `reconcile` mappers model the match arms that call these
// cores (ensure_consistent_with_db, recover_or_refuse, build_and_validate).

// ===== VERBATIM CORE 1: unified torn-tail decision (mirror.rs, findings D+E) =====
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TailDecision { Clean, TornTail, Deep }

pub(crate) fn decide_tail(terminated: bool, prefix_ok: bool) -> TailDecision {
    match (terminated, prefix_ok) {
        (true, true) => TailDecision::Clean,
        (false, true) => TailDecision::TornTail,
        (_, false) => TailDecision::Deep,
    }
}

// ===== VERBATIM CORE 2: empty-mirror-aware guard (recover.rs, finding G) =====
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardRoute { Prefix, RecoverAheadTopUp, FallBackToPrefix, Refuse }

fn route_guard(snapshot_head: u64, mirror_head: u64, self_certifies: bool, fallback_available: bool) -> GuardRoute {
    if snapshot_head <= mirror_head {
        GuardRoute::Prefix
    } else if self_certifies && mirror_head >= 1 {
        GuardRoute::RecoverAheadTopUp
    } else if fallback_available {
        GuardRoute::FallBackToPrefix
    } else {
        GuardRoute::Refuse
    }
}

// ===== VERBATIM CORE 3+4: overlap agreement + genesis anchor (recover.rs, G) =====
fn first_overlap_disagreement(mirror: &[(u64, String)], staging: &[(u64, String)]) -> Option<u64> {
    for (seq, mirror_hash) in mirror {
        match staging.iter().find(|(s, _)| s == seq) {
            Some((_, staging_hash)) if staging_hash == mirror_hash => {}
            _ => return Some(*seq),
        }
    }
    None
}
fn overlap_is_genesis_anchored(mirror_overlap: &[(u64, String)]) -> bool {
    matches!(mirror_overlap.first(), Some((seq, _)) if *seq == 1)
}

// ===== Thin mappers modeling the match arms that consume the cores =====
#[derive(Debug, PartialEq)]
enum TailAction { ProceedOnPrefix, PreserveRefuse }
/// Models the IDENTICAL disposition routing in BOTH ensure_consistent_with_db
/// (boot) AND recover_or_refuse (recovery) — the "one policy, both sides".
fn tail_action(d: TailDecision) -> TailAction {
    match d {
        TailDecision::Clean | TailDecision::TornTail => TailAction::ProceedOnPrefix,
        TailDecision::Deep => TailAction::PreserveRefuse,
    }
}

#[derive(Debug, PartialEq)]
enum Reconcile { Extend, Unchanged, PreserveRefuseAhead, PreserveRefuseHeadMismatch }
/// Models ensure_consistent_with_db's post-read seq/hash if-chain (P0 ahead-of-DB
/// preserved verbatim; the equal-length head-hash arm is the R1 finding-D fix).
fn reconcile(mirror_head: u64, db_head: u64, head_hash_equal: bool) -> Reconcile {
    if mirror_head < db_head {
        Reconcile::Extend
    } else if mirror_head > db_head {
        Reconcile::PreserveRefuseAhead // P0 (Gap-2a) — UNCHANGED
    } else if db_head == 0 {
        Reconcile::Unchanged
    } else if head_hash_equal {
        Reconcile::Unchanged
    } else {
        Reconcile::PreserveRefuseHeadMismatch // finding D — was silent rebuild
    }
}
/// Models build_and_validate's ahead-branch self-cert gates: R1/G empty-mirror
/// + genesis anchor, then the D4 overlap-agreement check.
fn ahead_self_certifies(mirror_head: u64, mirror_overlap: &[(u64, String)], overlap_disagree: Option<u64>) -> bool {
    if mirror_head == 0 { return false; }
    if !overlap_is_genesis_anchored(mirror_overlap) { return false; }
    overlap_disagree.is_none()
}

fn main() {}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure cores ----
    #[test]
    fn decide_tail_four_cases() {
        assert_eq!(decide_tail(true, true), TailDecision::Clean);
        assert_eq!(decide_tail(false, true), TailDecision::TornTail);
        assert_eq!(decide_tail(true, false), TailDecision::Deep);
        assert_eq!(decide_tail(false, false), TailDecision::Deep);
    }
    #[test]
    fn route_guard_table_incl_empty_mirror() {
        assert_eq!(route_guard(100, 106, true, true), GuardRoute::Prefix);
        assert_eq!(route_guard(106, 106, false, false), GuardRoute::Prefix);
        assert_eq!(route_guard(109, 106, true, false), GuardRoute::RecoverAheadTopUp);
        assert_eq!(route_guard(109, 106, false, true), GuardRoute::FallBackToPrefix);
        assert_eq!(route_guard(109, 106, false, false), GuardRoute::Refuse);
        // finding G: empty mirror never RecoverAheadTopUp
        assert_eq!(route_guard(109, 0, true, false), GuardRoute::Refuse);
        assert_eq!(route_guard(109, 0, true, true), GuardRoute::FallBackToPrefix);
    }
    #[test]
    fn overlap_helpers() {
        let m = vec![(1u64, "a".into()), (2, "b".into())];
        assert_eq!(first_overlap_disagreement(&m, &[(1, "a".into()), (2, "b".into())]), None);
        assert_eq!(first_overlap_disagreement(&m, &[(1, "a".into()), (2, "X".into())]), Some(2));
        assert!(overlap_is_genesis_anchored(&m));
        assert!(!overlap_is_genesis_anchored(&[]));
        assert!(!overlap_is_genesis_anchored(&[(2u64, "b".into())]));
    }

    // ---- the six NAMED gate behaviours ----
    #[test]
    fn g1_torn_tail_boot_preserve_trim_continue() {
        assert_eq!(decide_tail(false, true), TailDecision::TornTail);
        assert_eq!(tail_action(TailDecision::TornTail), TailAction::ProceedOnPrefix);
        // trimmed head 1, DB head 2 → the reconcile re-extends (continues)
        assert_eq!(reconcile(1, 2, false), Reconcile::Extend);
    }
    #[test]
    fn g2_deep_corrupt_boot_preserve_refuse() {
        assert_eq!(decide_tail(true, false), TailDecision::Deep);
        assert_eq!(decide_tail(false, false), TailDecision::Deep);
        assert_eq!(tail_action(TailDecision::Deep), TailAction::PreserveRefuse);
    }
    #[test]
    fn g3_equal_length_mismatch_preserve_refuse() {
        assert_eq!(reconcile(5, 5, false), Reconcile::PreserveRefuseHeadMismatch);
        assert_eq!(reconcile(5, 5, true), Reconcile::Unchanged); // control
    }
    #[test]
    fn g4_torn_tail_recovery_preserve_trim_proceed() {
        // recovery routes the SAME disposition mapper as boot (coherence)
        assert_eq!(tail_action(TailDecision::TornTail), TailAction::ProceedOnPrefix);
        assert_eq!(tail_action(TailDecision::Deep), TailAction::PreserveRefuse);
        assert_eq!(tail_action(TailDecision::Clean), TailAction::ProceedOnPrefix);
    }
    #[test]
    fn g5_empty_mirror_recovery_refuses() {
        assert!(!ahead_self_certifies(0, &[], None));                       // empty mirror
        assert!(!ahead_self_certifies(2, &[(2u64, "b".into())], None));     // non-genesis overlap
        assert!(ahead_self_certifies(2, &[(1u64,"a".into()),(2,"b".into())], None)); // ok
        assert_eq!(route_guard(109, 0, true, false), GuardRoute::Refuse);
    }
    #[test]
    fn g6_regression_p0_ahead_of_db_preserves_refuses() {
        assert_eq!(reconcile(107, 106, true), Reconcile::PreserveRefuseAhead);
        assert_eq!(reconcile(107, 106, false), Reconcile::PreserveRefuseAhead);
        // coherence: a torn tail that trims to a STILL-ahead head refuses via P0
        assert_eq!(tail_action(TailDecision::TornTail), TailAction::ProceedOnPrefix);
        assert_eq!(reconcile(107, 106, false), Reconcile::PreserveRefuseAhead);
    }
}
