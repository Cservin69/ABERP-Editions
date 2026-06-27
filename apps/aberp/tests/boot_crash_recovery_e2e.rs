//! ADR-0095 §1 — crash-injection boot/recovery end-to-end, against the REAL
//! `aberp` binary.
//!
//! These open real DuckDB files AND spawn the built `aberp` binary
//! (`snapshot now` to lay down the recovery snapshot, then `recover` to run
//! the supported recovery path), so they build only where the bundled
//! libduckdb 1.5.3 amalgamation builds — the Mac/CI gate, exactly like the
//! crate-level `aberp-snapshot/tests/recover_engine_tests.rs` and
//! `apps/aberp/tests/snapshot_e2e.rs`. They are FILE-SEPARATED (not
//! `#[ignore]`-gated): in the saw-off sandbox the file simply does not build
//! (no libduckdb), and on the gate it runs unconditionally.
//!
//! What this proves that the crate-level suite cannot: the wiring is reachable
//! through the **real process**. We inject the exact on-disk signature a
//! process killed mid-write leaves — a torn/unopenable DuckDB header
//! (`Failed to load metadata pointer (id 0, idx 0, ptr 0)`, ADR-0095 root
//! cause #1) and the ahead-of-DB mirror (root cause #4) — then run the real
//! `aberp recover`, and assert the next open is a healthy, openable DB with
//! ZERO manual steps and NO lost committed audit entry. The `aberp serve`
//! BOOT path invokes the identical engine via `serve::run_recover`'s sibling
//! `attempt_db_auto_recovery` → `aberp_snapshot::recover_or_refuse`; the full
//! TLS/port listener spawn is intentionally not driven here for the same
//! parallel-port reasons `serve_smoke.rs` documents.

use std::path::{Path, PathBuf};
use std::process::Command;

use aberp_audit_ledger::{
    mirror_path_for, read_mirror_entries, Actor, BinaryHash, EventKind, Ledger, TenantId,
};
use aberp_snapshot::checkpoint_is_current;
use duckdb::Connection;

const TENANT: &str = "acme";

// ── scaffolding (mirrors recover_engine_tests.rs / snapshot_e2e.rs) ───────

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "aberp-boot-recover-e2e-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn dir(&self) -> &Path {
        &self.0
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
fn bh() -> BinaryHash {
    BinaryHash::from_bytes([1u8; 32])
}

/// Seed `path` with `n_invoice` invoice rows and `n_audit` chained audit
/// entries — the same scaffold shape chunk-3 + Session A use.
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

fn append_audit(path: &Path, n: usize) {
    let mut ledger = Ledger::open(path, tid(), bh()).unwrap();
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

fn sync_mirror_of(path: &Path) -> u64 {
    let mirror = mirror_path_for(path);
    let ledger = Ledger::open(path, tid(), bh()).unwrap();
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

/// Every entry_hash (raw bytes) currently in the DB ledger, in seq order.
fn entry_hashes(path: &Path) -> Vec<Vec<u8>> {
    let l = Ledger::open(path, tid(), bh()).unwrap();
    l.entries()
        .unwrap()
        .iter()
        .map(|e| e.entry_hash.as_bytes().as_slice().to_vec())
        .collect()
}

fn count_kind(path: &Path, kind: EventKind) -> usize {
    let l = Ledger::open(path, tid(), bh()).unwrap();
    l.entries()
        .unwrap()
        .iter()
        .filter(|e| e.kind == kind)
        .count()
}

fn wal_of(path: &Path) -> PathBuf {
    let mut o = path.as_os_str().to_owned();
    o.push(".wal");
    PathBuf::from(o)
}

/// Inject the exact on-disk signature a process killed mid-checkpoint leaves:
/// a torn/unopenable DuckDB file (ADR-0095 root cause #1).
fn make_torn(path: &Path) {
    let _ = std::fs::remove_file(wal_of(path));
    std::fs::write(path, b"TORN-DUCKDB-HEADER-meta_block=0x0\x00\x00").unwrap();
}

/// Replace the live DB with a FRESH EMPTY one (audit head 0) — the Defense
/// ahead-mirror trigger (boot rebuilt an empty DB; the mirror was ahead).
fn make_fresh_empty(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal_of(path));
    let _ = Ledger::open(path, tid(), bh()).unwrap();
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

/// Run the REAL built `aberp` binary with `args`; return whether it exited 0.
fn run_aberp(args: &[&str]) -> bool {
    Command::new(env!("CARGO_BIN_EXE_aberp"))
        .args(args)
        .status()
        .expect("spawn aberp binary")
        .success()
}

// ── tests ────────────────────────────────────────────────────────────────

/// Torn-DB (root cause #1): a process killed mid-checkpoint leaves an
/// unopenable file. The supported recovery command rebuilds it from the
/// snapshot + replays the mirror, with ZERO manual steps and NO lost entry.
#[test]
fn recover_cli_auto_recovers_torn_db_with_no_lost_committed_entry() {
    let t = Tmp::new("torn");
    let db = t.db();
    let store = t.store();
    let mirror = mirror_path_for(&db);
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());

    // Truth before the crash: 3 invoices + 4 audit entries, a VALID snapshot
    // taken by the REAL binary, then 2 more committed audit entries.
    seed(&db, 3, 4);
    assert!(
        run_aberp(&["snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s]),
        "aberp snapshot now must succeed"
    );
    append_audit(&db, 2);
    let committed = entry_hashes(&db);
    let head_before = committed.len() as u64;
    assert_eq!(sync_mirror_of(&db), head_before, "mirror at the DB head");

    // The crash outcome: a torn, unopenable live file.
    make_torn(&db);
    assert!(
        Connection::open(&db)
            .and_then(|c| c.execute_batch("PRAGMA database_list;"))
            .is_err(),
        "the torn file must not open cleanly"
    );

    // The ONE supported recovery step — no sidecar surgery.
    assert!(
        run_aberp(&["recover", "--db", db_s, "--tenant", TENANT, "--store", store_s]),
        "aberp recover must exit 0 on a recoverable torn DB"
    );

    // Openable again, every committed audit entry intact, invoices restored.
    let after = entry_hashes(&db);
    for h in &committed {
        assert!(after.contains(h), "a committed audit entry was lost in recovery");
    }
    assert_eq!(invoice_ids(&db), vec![0, 1, 2], "invoices restored from the snapshot");
    assert!(
        checkpoint_is_current(&db),
        "a verified-good marker covers the rebuilt DB"
    );
    assert_eq!(
        count_kind(&db, EventKind::DbAutoRecovered),
        1,
        "the recovery is audited as db.auto_recovered"
    );
    assert!(corrupt_copies(t.dir()) >= 1, "the torn DB was retained as evidence");
}

/// Ahead-mirror (root cause #4): the live DB was lost and a fresh empty one
/// took its place while the append-only mirror still led it. Recovery REPLAYS
/// the mirror (never truncates it), so no committed entry is lost and the
/// chain does not fork.
#[test]
fn recover_cli_replays_ahead_mirror_with_no_fork_or_loss() {
    let t = Tmp::new("ahead");
    let db = t.db();
    let store = t.store();
    let mirror = mirror_path_for(&db);
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());

    seed(&db, 3, 4);
    assert!(
        run_aberp(&["snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s]),
        "aberp snapshot now must succeed"
    );
    append_audit(&db, 2);
    let committed = entry_hashes(&db);
    let head_before = committed.len() as u64;
    assert_eq!(sync_mirror_of(&db), head_before, "mirror at the DB head");

    // The Defense fingerprint: live DB lost → fresh empty one (head 0); the
    // mirror still carries every committed entry.
    make_fresh_empty(&db);

    assert!(
        run_aberp(&["recover", "--db", db_s, "--tenant", TENANT, "--store", store_s]),
        "aberp recover must exit 0 on an ahead mirror with a valid snapshot"
    );

    // Replayed, not truncated: every committed entry survives + invoices back.
    let after = entry_hashes(&db);
    for h in &committed {
        assert!(after.contains(h), "an ahead-mirror committed entry was lost");
    }
    assert_eq!(invoice_ids(&db), vec![0, 1, 2], "invoices restored from the snapshot");
    assert_eq!(count_kind(&db, EventKind::DbAutoRecovered), 1, "recovery audited");
    assert!(
        read_mirror_entries(&mirror).unwrap().len() as u64 >= head_before,
        "the ahead mirror was REPLAYED, never truncated"
    );
}
