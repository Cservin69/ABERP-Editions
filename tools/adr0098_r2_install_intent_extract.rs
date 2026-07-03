//! ADR-0098 R2 — faithful `rustc --test` extraction of the PURE decision cores
//! (Fable-5 finding B: the in-place WAL fold hiding inside durable_checkpoint).
//! aberp-snapshot is DuckDB-linked (bundled libduckdb amalgamation) → it cannot
//! `cargo build` in the saw-off sandbox, so the load-bearing PURE logic is copied
//! VERBATIM here and proven with `rustc --test`. The serde_json journal I/O and
//! the real DuckDB EXPORT/IMPORT are exercised by `cargo test` + the 5-point
//! crash-injection matrix on the Mac/CI gate.
//
// Provenance (copied verbatim from branch adr0098-remediation):
//   decide_resume / ResumeDecision   <- crates/aberp-snapshot/src/crash_safe.rs
//   wal_fence_violated / WalFence     <- crates/aberp-snapshot/src/crash_safe.rs
// The `*_sim` helpers model durable_checkpoint's forward protocol + the
// resume_pending_install boot path over PLAIN FILES, with a std-only FNV-1a
// content hash standing in for SHA-256 (identity semantics only) and a 3-line
// journal standing in for the serde_json one — the DECISION the harness proves
// is byte-format-independent. Run: `rustc --test --edition 2021 <file> && ./<bin>`.

use std::path::{Path, PathBuf};

// ===== VERBATIM CORE 1: boot-resume decision (crash_safe.rs, finding B) =====
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeDecision {
    NoPending,
    Complete,
    ClearStaleWal,
    Refuse,
}

fn decide_resume(
    intent_present: bool,
    staging_present: bool,
    staging_sha_matches: bool,
    live_matches_target: bool,
) -> ResumeDecision {
    if !intent_present {
        return ResumeDecision::NoPending;
    }
    if staging_present {
        // (a) staging still here => rename had not happened; finish iff SHA matches.
        if staging_sha_matches {
            ResumeDecision::Complete
        } else {
            ResumeDecision::Refuse
        }
    } else if live_matches_target {
        // (b) staging gone + live already equals journaled identity => rename done.
        ResumeDecision::ClearStaleWal
    } else {
        // (c) neither reconciles => refuse.
        ResumeDecision::Refuse
    }
}

// ===== VERBATIM CORE 2: live-WAL growth fence (crash_safe.rs, Bug-2 belt) =====
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalFence {
    present: bool,
    size: u64,
}

fn wal_fence_violated(before: WalFence, now: WalFence) -> bool {
    if now.size > before.size {
        return true;
    }
    if before.present && !now.present {
        return true;
    }
    if before.present && now.present && now.size < before.size {
        return true;
    }
    false
}

// ===== std-only file-op analogues of the crate's crash-safe I/O =====
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
fn hash_file(p: &Path) -> Option<u64> {
    std::fs::read(p).ok().map(|b| fnv1a(&b))
}
fn wal_path(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_owned();
    os.push(".wal");
    PathBuf::from(os)
}
fn intent_path(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_owned();
    os.push(".install-intent");
    PathBuf::from(os)
}

struct Intent {
    staging: PathBuf,
    hash: u64,
    target: PathBuf,
}
fn write_intent(db: &Path, staging: &Path, staging_hash: u64) {
    let body = format!(
        "{}\n{}\n{}\n",
        staging.display(),
        staging_hash,
        db.display()
    );
    std::fs::write(intent_path(db), body).unwrap();
}
fn read_intent(db: &Path) -> Option<Intent> {
    let s = std::fs::read_to_string(intent_path(db)).ok()?;
    let mut it = s.lines();
    let staging = PathBuf::from(it.next()?);
    let hash: u64 = it.next()?.parse().ok()?;
    let target = PathBuf::from(it.next()?);
    Some(Intent {
        staging,
        hash,
        target,
    })
}

// atomic_install analogue: rename staging->target, delete the now-stale target WAL.
fn atomic_install_sim(staging: &Path, target: &Path) {
    std::fs::rename(staging, target).unwrap();
    let w = wal_path(target);
    if w.exists() {
        std::fs::remove_file(&w).unwrap();
    }
}

// resume_pending_install analogue (returns the decision it acted on).
fn resume_sim(db: &Path) -> Result<ResumeDecision, String> {
    let intent = match read_intent(db) {
        None => return Ok(ResumeDecision::NoPending),
        Some(i) => i,
    };
    let staging_present = intent.staging.exists();
    let staging_sha_matches = staging_present && hash_file(&intent.staging) == Some(intent.hash);
    let live_matches_target =
        intent.target.exists() && hash_file(&intent.target) == Some(intent.hash);
    let d = decide_resume(
        true,
        staging_present,
        staging_sha_matches,
        live_matches_target,
    );
    match d {
        ResumeDecision::Complete => {
            atomic_install_sim(&intent.staging, &intent.target);
            std::fs::remove_file(intent_path(db)).unwrap();
            Ok(d)
        }
        ResumeDecision::ClearStaleWal => {
            let w = wal_path(&intent.target);
            if w.exists() {
                std::fs::remove_file(&w).unwrap();
            }
            std::fs::remove_file(intent_path(db)).unwrap();
            Ok(d)
        }
        ResumeDecision::Refuse => {
            let mut os = db.as_os_str().to_owned();
            os.push(".install-intent.unreconciled");
            let _ = std::fs::copy(intent_path(db), PathBuf::from(os)); // preserve evidence
            Err("unreconcilable install-intent (preserved + refused)".to_string())
        }
        ResumeDecision::NoPending => Ok(d),
    }
}

// durable_checkpoint forward protocol, stopped at a crash point.
#[derive(Clone, Copy)]
enum CrashPoint {
    AfterStagingBeforeJournal,
    AfterJournalBeforeRename,
    AfterRenameBeforeWalClear,
    Complete,
}
fn run_forward(dir: &Path, crash: CrashPoint) -> PathBuf {
    let db = dir.join("live.duckdb");
    std::fs::write(&db, b"OLD-GOOD").unwrap();
    std::fs::write(wal_path(&db), b"live-wal-old").unwrap(); // the DB's own live WAL
    let staging = dir.join("live.duckdb.ckpt-staging.duckdb");
    std::fs::write(&staging, b"NEW-SELF-CONTAINED").unwrap(); // fresh, self-contained
    let staging_hash = hash_file(&staging).unwrap();
    if let CrashPoint::AfterStagingBeforeJournal = crash {
        return db;
    }
    write_intent(&db, &staging, staging_hash);
    if let CrashPoint::AfterJournalBeforeRename = crash {
        return db;
    }
    std::fs::rename(&staging, &db).unwrap(); // rename in; WAL not yet deleted
    if let CrashPoint::AfterRenameBeforeWalClear = crash {
        return db;
    }
    let w = wal_path(&db);
    if w.exists() {
        std::fs::remove_file(&w).unwrap();
    }
    std::fs::remove_file(intent_path(&db)).unwrap();
    db
}

fn scratch(label: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("adr0098-r2-{label}-{n}-{}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_decision_table() {
        assert_eq!(
            decide_resume(false, true, true, true),
            ResumeDecision::NoPending
        );
        assert_eq!(
            decide_resume(true, true, true, false),
            ResumeDecision::Complete
        );
        assert_eq!(
            decide_resume(true, true, false, false),
            ResumeDecision::Refuse
        );
        assert_eq!(
            decide_resume(true, false, false, true),
            ResumeDecision::ClearStaleWal
        );
        assert_eq!(
            decide_resume(true, false, false, false),
            ResumeDecision::Refuse
        );
    }

    #[test]
    fn fence_truth_table() {
        let none = WalFence {
            present: false,
            size: 0,
        };
        let empty = WalFence {
            present: true,
            size: 0,
        };
        let w100 = WalFence {
            present: true,
            size: 100,
        };
        let w200 = WalFence {
            present: true,
            size: 200,
        };
        assert!(!wal_fence_violated(w100, w100));
        assert!(!wal_fence_violated(none, none));
        assert!(!wal_fence_violated(none, empty)); // empty WAL appearing is benign
        assert!(wal_fence_violated(w100, w200)); // grew (uncaptured commits)
        assert!(wal_fence_violated(none, w100)); // appeared with data
        assert!(wal_fence_violated(w100, none)); // vanished (concurrent fold)
        assert!(wal_fence_violated(w200, w100)); // shrank (partial fold)
    }

    #[test]
    fn crash_after_staging_before_journal_leaves_old_db_intact() {
        let d = scratch("c1");
        let db = run_forward(&d, CrashPoint::AfterStagingBeforeJournal);
        assert_eq!(resume_sim(&db).unwrap(), ResumeDecision::NoPending);
        assert_eq!(std::fs::read(&db).unwrap(), b"OLD-GOOD");
        assert!(
            wal_path(&db).exists(),
            "the DB's own consistent live WAL is untouched"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn crash_after_journal_before_rename_resumes_and_completes() {
        let d = scratch("c2");
        let db = run_forward(&d, CrashPoint::AfterJournalBeforeRename);
        assert_eq!(resume_sim(&db).unwrap(), ResumeDecision::Complete);
        assert_eq!(std::fs::read(&db).unwrap(), b"NEW-SELF-CONTAINED");
        assert!(
            !wal_path(&db).exists(),
            "stale live WAL deleted by completed install"
        );
        assert!(!intent_path(&db).exists(), "journal cleared");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn crash_after_rename_before_wal_clear_deletes_foreign_wal_no_double_replay() {
        let d = scratch("c3");
        let db = run_forward(&d, CrashPoint::AfterRenameBeforeWalClear);
        // pre-resume: live is already the fresh file, but a FOREIGN WAL sits beside it.
        assert_eq!(std::fs::read(&db).unwrap(), b"NEW-SELF-CONTAINED");
        assert!(wal_path(&db).exists(), "foreign WAL present pre-resume");
        assert_eq!(resume_sim(&db).unwrap(), ResumeDecision::ClearStaleWal);
        assert!(
            !wal_path(&db).exists(),
            "foreign WAL deleted — double-replay prevented"
        );
        assert!(!intent_path(&db).exists(), "journal cleared");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn clean_completion_has_nothing_to_resume() {
        let d = scratch("c4");
        let db = run_forward(&d, CrashPoint::Complete);
        assert_eq!(resume_sim(&db).unwrap(), ResumeDecision::NoPending);
        assert_eq!(std::fs::read(&db).unwrap(), b"NEW-SELF-CONTAINED");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn unreconcilable_intent_refuses_and_preserves() {
        let d = scratch("c5");
        let db = d.join("live.duckdb");
        std::fs::write(&db, b"SOMETHING-ELSE").unwrap();
        let staging = d.join("live.duckdb.ckpt-staging.duckdb"); // absent
        write_intent(&db, &staging, 0xdead_beef); // hash matches neither file
        assert!(resume_sim(&db).is_err());
        assert!(
            intent_path(&db).exists(),
            "original journal kept — boot stays refused"
        );
        let mut os = db.as_os_str().to_owned();
        os.push(".install-intent.unreconciled");
        assert!(PathBuf::from(os).exists(), "evidence preserved aside");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn wal_fence_violation_aborts_forward_swap() {
        // step 3b: a WAL that grew during EXPORT => fence violated => abort (no swap).
        let before = WalFence {
            present: true,
            size: 10,
        };
        let now = WalFence {
            present: true,
            size: 42,
        };
        assert!(wal_fence_violated(before, now));
        assert!(!wal_fence_violated(before, before)); // stable WAL => proceed
    }
}
