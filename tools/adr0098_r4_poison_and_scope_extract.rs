//! ADR-0098 R4 (Fable-5 findings F + H) — toolchain-checkable extraction of the
//! two R4 decisions, as self-contained `rustc --test` logic (std-only; no crate
//! deps, same spirit as the R1/R2/R3 extracts). Proves:
//!   (F) the POISON-RECOVER-AND-REVERIFY decision (aberp-db `Handle`): a poisoned
//!       writer mutex is RECOVERED (clear_poison) then integrity-re-verified; a
//!       benign prior panic that left the DB consistent RESUMES + is AUDITED (it
//!       must NOT permanently brick the process), while a FAILED re-verify (or a
//!       reopen failure) surfaces a HARD error. Plus the daemon tick-guard: a
//!       caught tick panic becomes an `Err` and the loop proceeds to the next
//!       tick (behaviour preserved).
//!   (H) the SCANNER's new SCOPE + ALIAS rules (tools/adr0098_opener_scan.awk):
//!       the opener scan now covers crates/ (minus the sanctioned aberp-db /
//!       aberp-snapshot seams), catches `use ... as X; X::open` alias evasion and
//!       `Database::open`, and still excludes open_in_memory / from_connection /
//!       #[cfg(test)].
//!
//! Build+run:  rustc --test adr0098_r4_poison_and_scope_extract.rs -o /tmp/r4t && /tmp/r4t

#![allow(dead_code)]

// ── Finding F: the poison-recovery decision ─────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum PoisonOutcome {
    /// Recovered in place: clear_poison + reopen + re-verify all passed. The
    /// writer resumes and the recovery is audited (db.auto_recovered).
    Recovered { audited: bool },
    /// A hard error is surfaced (never served from a bad DB, never bricked-silent).
    HardError(HardKind),
}

#[derive(Debug, PartialEq, Eq)]
enum HardKind {
    /// The freshly-reopened DB would not open at all.
    ReopenFailed,
    /// The DB reopened but the audit hash-chain did NOT verify genesis→head.
    ReverifyFailed,
}

/// Faithful port of `Handle::recover_from_poison`'s decision: reopen fresh, then
/// re-verify the chain. Only a reopen failure or a failed re-verify is hard; a
/// benign prior panic (consistent DB) recovers + audits.
fn poison_recovery_outcome(reopen_ok: bool, chain_verifies: bool) -> PoisonOutcome {
    if !reopen_ok {
        return PoisonOutcome::HardError(HardKind::ReopenFailed);
    }
    if !chain_verifies {
        return PoisonOutcome::HardError(HardKind::ReverifyFailed);
    }
    PoisonOutcome::Recovered { audited: true }
}

/// The key non-brick invariant: `lock_recovering` on a poisoned mutex must NOT
/// return the perpetual `Poisoned` error when the DB is consistent — it recovers.
fn acquire_is_bricked(poisoned: bool, reopen_ok: bool, chain_verifies: bool) -> bool {
    if !poisoned {
        return false; // clean acquire
    }
    matches!(
        poison_recovery_outcome(reopen_ok, chain_verifies),
        PoisonOutcome::HardError(_)
    )
}

#[derive(Debug, PartialEq, Eq)]
struct TickResult {
    is_err: bool,
    loop_continues: bool,
}

/// Faithful port of `guard_write_tick`: a caught panic becomes an `Err` and the
/// daemon loop proceeds to the next tick (skip exactly this one). No panic →
/// the body's own result flows through unchanged.
fn tick_outcome(panicked: bool, body_ok: bool) -> TickResult {
    if panicked {
        TickResult { is_err: true, loop_continues: true }
    } else {
        TickResult { is_err: !body_ok, loop_continues: true }
    }
}

// ── Finding H: the scanner scope + alias rules ──────────────────────────────

/// Learn an opener-type alias from a `use ... <Type> as <Alias>;` line, mirroring
/// the awk's ALIAS learning. Returns the alias identifier if present.
fn learn_alias(code: &str) -> Option<String> {
    if !code.contains("use ") {
        return None;
    }
    for ty in ["Connection", "Ledger", "DuckDbBillingStore", "Database"] {
        if let Some(i) = code.find(ty) {
            let rest = &code[i + ty.len()..];
            let rest_t = rest.trim_start();
            if let Some(a) = rest_t.strip_prefix("as ") {
                let alias: String = a
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !alias.is_empty() {
                    return Some(alias);
                }
            }
        }
    }
    None
}

/// Faithful port of the awk opener match on a comment/string-stripped `code`
/// line, given the set of learned aliases. Excludes the sanctioned seams.
fn is_opener(code: &str, aliases: &[String]) -> bool {
    if code.contains("open_in_memory") || code.contains("from_connection") {
        return false;
    }
    let literal = code.contains("Connection::open(")
        || code.contains("Connection::open_with_flags(")
        || code.contains("Ledger::open(")
        || code.contains("DuckDbBillingStore::open(")
        || code.contains("Database::open(")
        || code.contains("append_reopen(");
    if literal {
        return true;
    }
    for a in aliases {
        if code.contains(&format!("{a}::open(")) || code.contains(&format!("{a}::open_with_flags("))
        {
            return true;
        }
    }
    false
}

/// Faithful port of the R4-extended scan SCOPE + skip: apps/aberp + modules +
/// crates are scanned; crates/aberp-db/* and crates/aberp-snapshot/* are the
/// sanctioned seams and are excluded.
fn in_scan_scope(path: &str) -> bool {
    if path.starts_with("crates/aberp-db/") || path.starts_with("crates/aberp-snapshot/") {
        return false;
    }
    path.starts_with("apps/aberp/src/")
        || path.starts_with("modules/")
        || path.starts_with("crates/")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Finding F ----

    #[test]
    fn benign_prior_panic_recovers_and_audits_not_bricked() {
        assert_eq!(
            poison_recovery_outcome(true, true),
            PoisonOutcome::Recovered { audited: true }
        );
        // The whole point of finding F: a consistent prior panic does NOT brick.
        assert!(!acquire_is_bricked(true, true, true));
    }

    #[test]
    fn failed_reverify_is_a_hard_error() {
        assert_eq!(
            poison_recovery_outcome(true, false),
            PoisonOutcome::HardError(HardKind::ReverifyFailed)
        );
        assert!(acquire_is_bricked(true, true, false));
    }

    #[test]
    fn reopen_failure_is_a_hard_error() {
        assert_eq!(
            poison_recovery_outcome(false, true),
            PoisonOutcome::HardError(HardKind::ReopenFailed)
        );
    }

    #[test]
    fn clean_acquire_is_never_bricked() {
        assert!(!acquire_is_bricked(false, true, true));
    }

    #[test]
    fn caught_tick_panic_errs_but_loop_continues() {
        let t = tick_outcome(true, false);
        assert!(t.is_err && t.loop_continues);
        // no-panic path is unchanged
        assert_eq!(tick_outcome(false, true), TickResult { is_err: false, loop_continues: true });
    }

    // ---- Finding H ----

    #[test]
    fn literal_openers_are_caught() {
        let a: Vec<String> = vec![];
        assert!(is_opener("let c = Connection::open(p)?;", &a));
        assert!(is_opener("let c = Ledger::open(p, t, h)?;", &a));
        assert!(is_opener("let c = DuckDbBillingStore::open(p)?;", &a));
        assert!(is_opener("let c = Database::open(p)?;", &a)); // R4 addition
        assert!(is_opener("let c = Connection::open_with_flags(p, cfg)?;", &a));
    }

    #[test]
    fn alias_evasion_is_caught() {
        let alias = learn_alias("use duckdb::Connection as C;").unwrap();
        assert_eq!(alias, "C");
        let aliases = vec![alias];
        assert!(is_opener("let c = C::open(p)?;", &aliases));
        // Database alias too
        let da = learn_alias("use duckdb::Database as DbX;").unwrap();
        assert!(is_opener("let d = DbX::open(p)?;", &[da]));
    }

    #[test]
    fn sanctioned_seams_are_excluded_from_opener_match() {
        let a: Vec<String> = vec![];
        assert!(!is_opener("let c = Connection::open_in_memory()?;", &a));
        assert!(!is_opener("Ledger::from_connection(conn, t, h)", &a));
        // an alias whose call is open_in_memory must not trip
        assert!(!is_opener("let c = C::open_in_memory()?;", &["C".to_string()]));
    }

    #[test]
    fn scope_covers_crates_but_excludes_the_two_seams() {
        assert!(in_scan_scope("crates/aberp-qa/src/foo.rs")); // R4: now in scope
        assert!(in_scan_scope("crates/aberp-mes/src/ledger_writer.rs"));
        assert!(in_scan_scope("apps/aberp/src/serve.rs"));
        assert!(in_scan_scope("modules/billing/src/adapters/duckdb_store.rs"));
        assert!(!in_scan_scope("crates/aberp-db/src/lib.rs")); // the Handle seam
        assert!(!in_scan_scope("crates/aberp-snapshot/src/take.rs")); // boot/snapshot seam
    }
}
