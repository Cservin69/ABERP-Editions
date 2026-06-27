//! ADR-0095 §1–§2 — DuckDB-backed end-to-end tests for the recovery engine.
//! These open real DuckDB files (snapshot IMPORT/EXPORT + audit replay), so
//! they run under `cargo test -p aberp-snapshot` on the Mac gate (the bundled
//! DuckDB amalgamation cannot build in the saw-off sandbox). The PURE
//! crash-safe COMMIT property (a crash mid-create never leaves a torn file at
//! the live path) is unit-tested in `recover.rs` and runs anywhere.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{
    mirror_path_for, read_mirror_entries, Actor, AppendError, BinaryHash, EventKind, Ledger,
    TenantId,
};
use aberp_snapshot::{
    checkpoint_is_current, provision_atomic, recover_or_refuse, take_snapshot, RecoveryOutcome,
};
use duckdb::Connection;
use time::OffsetDateTime;

const TENANT: &str = "acme";

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "aberp-recover-it-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn db(&self) -> PathBuf {
        self.0.join("aberp.duckdb")
    }
    fn store(&self) -> PathBuf {
        self.0.join("ABERP-snapshots-test").join(TENANT)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tid() -> TenantId {
    TenantId::new(TENANT.to_string()).unwrap()
}

/// Seed `path` with `n_invoice` invoice rows and `n_audit` chained audit
/// entries (the same scaffold shape chunk-3's checkpoint tests use).
fn seed(path: &Path, n_invoice: usize, n_audit: usize) {
    {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS invoice (id BIGINT, amount DOUBLE);")
            .unwrap();
        for i in 0..n_invoice {
            conn.execute(
                "INSERT INTO invoice VALUES (?, ?)",
                duckdb::params![i as i64, i as f64],
            )
            .unwrap();
        }
    }
    append_audit(path, n_audit);
}

/// Append `n` more chained audit entries to the DB at `path`.
fn append_audit(path: &Path, n: usize) {
    let mut ledger = Ledger::open(path, tid(), BinaryHash::from_bytes([1u8; 32])).unwrap();
    for i in 0..n {
        ledger
            .append(
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                Actor::test_only(),
                None,
            )
            .unwrap();
    }
}

/// Synchronise the on-disk JSONL mirror to the DB's current head.
fn sync_mirror_of(path: &Path) -> u64 {
    let mirror = mirror_path_for(path);
    let ledger = Ledger::open(path, tid(), BinaryHash::from_bytes([1u8; 32])).unwrap();
    ledger.sync_mirror(&mirror).unwrap()
}

fn invoice_ids(path: &Path) -> Vec<i64> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT id FROM invoice ORDER BY id").unwrap();
    let v = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    v
}

fn audit_count(path: &Path) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row("SELECT count(*) FROM audit_ledger", [], |r| r.get(0))
        .unwrap()
}

fn wal_of(path: &Path) -> PathBuf {
    let mut o = path.as_os_str().to_owned();
    o.push(".wal");
    PathBuf::from(o)
}

/// Overwrite the live DB with garbage so it is a torn/unopenable file.
fn make_torn(path: &Path) {
    let _ = std::fs::remove_file(wal_of(path));
    std::fs::write(path, b"TORN-DUCKDB-HEADER-meta_block=0x0\x00\x00").unwrap();
}

/// Replace the live DB with a FRESH EMPTY one (audit head 0) — the Defense
/// ahead-mirror trigger (boot rebuilt an empty DB; the mirror was ahead).
fn make_fresh_empty(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal_of(path));
    let _ = Ledger::open(path, tid(), BinaryHash::from_bytes([1u8; 32])).unwrap();
}

fn corrupt_copies(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.contains(".CORRUPT-"))
        })
        .count()
}

// ─────────────────────────────────────────────────────────────────────────
// §1 — auto-recover: ahead-mirror (replay > 0) and torn-DB (replay == 0)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn recover_replays_ahead_mirror_into_a_fresh_empty_db_with_zero_manual_steps() {
    let t = Tmp::new("ahead");
    let db = t.db();
    let store = t.store();
    let mirror = mirror_path_for(&db);

    // Truth at snapshot time: 3 invoices + 4 audit entries.
    seed(&db, 3, 4);
    take_snapshot(&db, &store, TENANT, OffsetDateTime::now_utc()).unwrap();

    // Two more committed audit entries (5,6); the mirror now leads the
    // snapshot by 2.
    append_audit(&db, 2);
    assert_eq!(sync_mirror_of(&db), 6, "mirror head is 6");

    // Defense scenario: the live DB is lost and boot rebuilt a fresh empty
    // one (audit head 0) while the mirror still carries 6.
    make_fresh_empty(&db);
    assert_eq!(audit_count(&db), 0, "fresh empty DB before recovery");

    let outcome = recover_or_refuse(&db, &store, &mirror, TENANT).unwrap();
    match outcome {
        RecoveryOutcome::Recovered {
            source_snapshot_seq,
            snapshot_audit_count,
            replayed_entries,
            recovered_max_seq,
            retained_corrupt_db,
        } => {
            assert_eq!(
                snapshot_audit_count, 4,
                "rebuild started from the 4-entry snapshot"
            );
            assert_eq!(
                replayed_entries, 2,
                "entries 5 and 6 were replayed from the mirror"
            );
            assert_eq!(
                recovered_max_seq, 6,
                "rebuilt head reconciles with the mirror head"
            );
            assert!(source_snapshot_seq >= 1);
            assert!(
                retained_corrupt_db.is_some(),
                "the pre-recovery DB was retained"
            );
        }
        other => panic!("expected Recovered, got {other:?}"),
    }

    // ZERO manual steps: the live DB is openable, every committed audit entry
    // is present, and the snapshot's invoices were restored.
    assert_eq!(audit_count(&db), 6, "no committed audit entry was lost");
    assert_eq!(
        invoice_ids(&db),
        vec![0, 1, 2],
        "invoices restored from the snapshot"
    );
    assert!(
        checkpoint_is_current(&db),
        "a verified-good marker covers the rebuilt file"
    );

    // The mirror was REPLAYED, never truncated.
    assert_eq!(
        read_mirror_entries(&mirror).unwrap().len(),
        6,
        "the mirror is preserved intact (never truncated)"
    );
    assert!(corrupt_copies(t.0.as_path()) >= 1, "evidence copy retained");
}

#[test]
fn recover_rebuilds_torn_db_from_a_current_snapshot_without_opening_the_torn_file() {
    let t = Tmp::new("torn");
    let db = t.db();
    let store = t.store();
    let mirror = mirror_path_for(&db);

    // Snapshot is current with the mirror (both head 5) → replay is a no-op,
    // the rebuild comes wholly from the snapshot.
    seed(&db, 2, 5);
    assert_eq!(sync_mirror_of(&db), 5);
    take_snapshot(&db, &store, TENANT, OffsetDateTime::now_utc()).unwrap();

    // The live file is torn (the duckdb#23046 signature). recover_or_refuse
    // never opens it — it rebuilds from snapshot + mirror.
    make_torn(&db);

    let outcome = recover_or_refuse(&db, &store, &mirror, TENANT).unwrap();
    match outcome {
        RecoveryOutcome::Recovered {
            replayed_entries,
            recovered_max_seq,
            retained_corrupt_db,
            ..
        } => {
            assert_eq!(
                replayed_entries, 0,
                "snapshot already current → nothing to replay"
            );
            assert_eq!(recovered_max_seq, 5);
            assert!(retained_corrupt_db.is_some());
        }
        other => panic!("expected Recovered, got {other:?}"),
    }

    assert_eq!(audit_count(&db), 5);
    assert_eq!(invoice_ids(&db), vec![0, 1]);
    assert!(checkpoint_is_current(&db));
    // The torn original was preserved byte-for-byte aside.
    let retained = std::fs::read_dir(t.0.as_path())
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.contains(".CORRUPT-"))
        })
        .unwrap()
        .path();
    assert!(std::fs::read(&retained)
        .unwrap()
        .starts_with(b"TORN-DUCKDB-HEADER"));
}

// ─────────────────────────────────────────────────────────────────────────
// §1 guard-rails — refuse (never guess), inputs untouched
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn recover_refuses_when_no_valid_snapshot_exists() {
    let t = Tmp::new("nosnap");
    let db = t.db();
    let store = t.store(); // never populated
    let mirror = mirror_path_for(&db);

    seed(&db, 1, 3);
    sync_mirror_of(&db);
    let mirror_before = std::fs::read(&mirror).unwrap();
    make_torn(&db);
    let db_before = std::fs::read(&db).unwrap();

    let outcome = recover_or_refuse(&db, &store, &mirror, TENANT).unwrap();
    assert!(
        matches!(outcome, RecoveryOutcome::RefusedNoSnapshot { .. }),
        "no snapshot → refuse, got {outcome:?}"
    );

    // Zero mutation of the live inputs (the torn DB and the mirror).
    assert_eq!(
        std::fs::read(&db).unwrap(),
        db_before,
        "live DB untouched on refuse"
    );
    assert_eq!(
        std::fs::read(&mirror).unwrap(),
        mirror_before,
        "mirror untouched on refuse"
    );
    assert!(
        !checkpoint_is_current(&db),
        "no verified-good marker written on refuse"
    );
}

#[test]
fn recover_refuses_unsafe_when_snapshot_is_ahead_of_the_mirror() {
    let t = Tmp::new("ahead-snap");
    let db = t.db();
    let store = t.store();
    let mirror = mirror_path_for(&db);

    // Mirror is synced at head 4, then two more entries are committed and a
    // snapshot is taken at head 6 — so the snapshot LEADS the mirror.
    seed(&db, 1, 4);
    assert_eq!(sync_mirror_of(&db), 4);
    append_audit(&db, 2);
    take_snapshot(&db, &store, TENANT, OffsetDateTime::now_utc()).unwrap();
    let mirror_before = std::fs::read(&mirror).unwrap();
    make_torn(&db);

    let outcome = recover_or_refuse(&db, &store, &mirror, TENANT).unwrap();
    match outcome {
        RecoveryOutcome::RefusedUnsafe { reason, .. } => {
            assert!(
                reason.contains("AHEAD"),
                "reason names the inconsistency: {reason}"
            );
        }
        other => panic!("expected RefusedUnsafe, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(&mirror).unwrap(),
        mirror_before,
        "mirror untouched on refuse"
    );
    assert!(!checkpoint_is_current(&db));
}

#[test]
fn recover_refuses_unsafe_when_the_mirror_is_corrupt() {
    let t = Tmp::new("badmirror");
    let db = t.db();
    let store = t.store();
    let mirror = mirror_path_for(&db);

    seed(&db, 1, 3);
    sync_mirror_of(&db);
    take_snapshot(&db, &store, TENANT, OffsetDateTime::now_utc()).unwrap();
    // Corrupt the mirror: a trailing partial line (the ADR-0030 §3 signal).
    std::fs::write(&mirror, b"{not-json without newline").unwrap();
    make_torn(&db);

    let outcome = recover_or_refuse(&db, &store, &mirror, TENANT).unwrap();
    assert!(
        matches!(outcome, RecoveryOutcome::RefusedUnsafe { .. }),
        "corrupt mirror → refuse, got {outcome:?}"
    );
    assert!(!checkpoint_is_current(&db));
}

// ─────────────────────────────────────────────────────────────────────────
// idempotency + §2 atomic creation
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn recover_is_idempotent_in_outcome() {
    let t = Tmp::new("idem");
    let db = t.db();
    let store = t.store();
    let mirror = mirror_path_for(&db);

    seed(&db, 2, 4);
    take_snapshot(&db, &store, TENANT, OffsetDateTime::now_utc()).unwrap();
    append_audit(&db, 2);
    sync_mirror_of(&db);
    make_fresh_empty(&db);

    let first = recover_or_refuse(&db, &store, &mirror, TENANT).unwrap();
    assert!(matches!(
        first,
        RecoveryOutcome::Recovered {
            recovered_max_seq: 6,
            ..
        }
    ));
    assert_eq!(audit_count(&db), 6);

    // Re-running recovery on the already-recovered DB yields the SAME valid,
    // openable result (no data loss, mirror still intact).
    let second = recover_or_refuse(&db, &store, &mirror, TENANT).unwrap();
    assert!(matches!(
        second,
        RecoveryOutcome::Recovered {
            recovered_max_seq: 6,
            ..
        }
    ));
    assert_eq!(
        audit_count(&db),
        6,
        "still complete after a second recovery"
    );
    assert_eq!(invoice_ids(&db), vec![0, 1]);
    assert_eq!(
        read_mirror_entries(&mirror).unwrap().len(),
        6,
        "mirror still intact"
    );
    assert!(checkpoint_is_current(&db));
}

#[test]
fn provision_atomic_creates_a_good_marked_db_at_the_live_path() {
    let t = Tmp::new("provision");
    let db = t.db();
    assert!(!db.exists());

    provision_atomic(&db, |creating: &Path| -> Result<(), AppendError> {
        // Build schema + a genesis audit row ENTIRELY on the temp path.
        {
            let conn = Connection::open(creating)?;
            conn.execute_batch("CREATE TABLE invoice (id BIGINT, amount DOUBLE);")?;
        }
        let mut ledger = Ledger::open(creating, tid(), BinaryHash::from_bytes([2u8; 32]))?;
        ledger.append(
            EventKind::Test,
            b"genesis".to_vec(),
            Actor::test_only(),
            None,
        )?;
        Ok(())
    })
    .unwrap();

    // The live path is the present, good, self-contained file with a
    // verified-good marker — boot needs no in-place LoadCheckpoint replay.
    assert!(db.exists(), "the live DB exists after provisioning");
    assert_eq!(audit_count(&db), 1, "genesis audit row present");
    assert!(
        checkpoint_is_current(&db),
        "verified-good marker written at creation"
    );
    // No leftover creating-temp.
    let leftovers = std::fs::read_dir(t.0.as_path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.contains(".creating-"))
        })
        .count();
    assert_eq!(
        leftovers, 0,
        "the creating-temp was consumed by the atomic swap"
    );
}

#[test]
fn provision_atomic_surfaces_init_failure_without_writing_the_live_path() {
    let t = Tmp::new("provision-fail");
    let db = t.db();
    let err = provision_atomic(&db, |_creating: &Path| -> Result<(), AppendError> {
        Err(AppendError::SequenceConflict { seq: 7 })
    })
    .unwrap_err();
    assert!(
        format!("{err}").contains("atomic provisioning"),
        "loud provisioning error: {err}"
    );
    assert!(
        !db.exists(),
        "a failed init never materialises the live path"
    );
}
