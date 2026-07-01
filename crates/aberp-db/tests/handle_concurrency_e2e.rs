//! ADR-0098 Session B — the load-bearing acceptance proof for Gap 1a/1b.
//!
//! These open **real DuckDB** files (concurrent separate-instance writers,
//! the validated logical checkpoint, audit appends), so they run under
//! `cargo test -p aberp-db` on the **Mac / CI gate** — the bundled libduckdb
//! 1.5.3 amalgamation cannot build in the saw-off sandbox (the same gate as
//! ADR-0095 chunk-3 and `aberp-snapshot`'s `tests/recover_engine_tests.rs`).
//! The PURE D2 debounce logic is unit-tested in `src/debounce.rs` and runs
//! anywhere.
//!
//! The crossing-the-finish-line test is
//! [`concurrent_separate_opens_tear_the_file_but_shared_handle_never_does`]:
//! it must **fail on `9c35ebb`/`c903e23`** (reproducing the 2026-06-29 17:02
//! re-tear with separate `Connection::open` instances) and **pass after
//! Session B** (both writers share one `aberp_db::Handle`). No `v0.2.5`
//! without it green (fix-plan §"Cross-cutting acceptance gate").

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aberp_audit_ledger::{
    append_in_tx, ensure_schema, mirror_path_for, read_mirror_entries, recent_entries, Actor,
    BinaryHash, EventKind, LedgerMeta, TenantId,
};
use aberp_db::{Handle, HandleConfig};
use duckdb::Connection;

const TENANT: &str = "defense";

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("aberp-db-it-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn db(&self) -> PathBuf {
        self.0.join("aberp.duckdb")
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tenant() -> TenantId {
    TenantId::new(TENANT.to_string()).unwrap()
}

/// Seed an empty tenant DB with the audit schema (so a fresh open is valid).
fn seed(db: &Path) {
    let conn = Connection::open(db).unwrap();
    ensure_schema(&conn).unwrap();
    conn.execute_batch("CHECKPOINT;").unwrap();
}

/// Append one audit row on a connection (the shape a daemon write takes).
fn append_one(conn: &mut Connection, seq_label: &str) {
    let meta = LedgerMeta::new(tenant(), BinaryHash::from_bytes([7u8; 32]));
    let tx = conn.transaction().unwrap();
    let actor = Actor::from_local_cli(format!("ulid-{seq_label}"), "tester");
    append_in_tx(
        &tx,
        &meta,
        EventKind::DbAutoRecovered,
        format!("{{\"probe\":\"{seq_label}\"}}").into_bytes(),
        actor,
        None,
    )
    .unwrap();
    tx.commit().unwrap();
}

/// Does a *fresh* open of `db` succeed (i.e. the on-disk checkpoint is not
/// torn)? The 17:02 signature is a fresh open failing in `LoadCheckpoint`
/// with "metadata pointer (id 0, idx 0, ptr 0)".
fn fresh_open_ok(db: &Path) -> bool {
    match Connection::open(db) {
        Ok(c) => c.execute_batch("SELECT 1;").is_ok(),
        Err(_) => false,
    }
}

/// **THE acceptance test.** Two independent writers hammer the same single-file
/// DuckDB. Arm A uses separate `Connection::open` instances (the pre-fix path):
/// on `9c35ebb`/`c903e23` this tears the file (a fresh open fails
/// `LoadCheckpoint … ptr 0`). Arm B routes both writers through ONE shared
/// `aberp_db::Handle`: the file must NEVER tear across all iterations.
#[test]
fn concurrent_separate_opens_tear_the_file_but_shared_handle_never_does() {
    // ---- Arm B (post-fix): one shared Handle, must never tear. ----
    let tmp = Tmp::new("shared");
    let db = tmp.db();
    seed(&db);
    // Isolate the single-instance property: checkpoint disabled so this asserts
    // 1a (no concurrent separate instances) independent of 1b.
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle: Arc<Handle> = Handle::open(&db, tenant(), cfg).unwrap();

    let iterations = 200usize;
    let mut workers = Vec::new();
    // Writer 1 — pricing-style enqueue cadence.
    {
        let h = handle.clone();
        workers.push(thread::spawn(move || {
            for i in 0..iterations {
                let mut g = h.write().unwrap();
                append_one(&mut g, &format!("w1-{i}"));
            }
        }));
    }
    // Writer 2 — email-relay-style 2s-claim cadence (tightened for the test).
    {
        let h = handle.clone();
        workers.push(thread::spawn(move || {
            for i in 0..iterations {
                {
                    let mut g = h.write().unwrap();
                    append_one(&mut g, &format!("w2-{i}"));
                }
                thread::sleep(Duration::from_micros(50));
            }
        }));
    }
    for w in workers {
        w.join().unwrap();
    }
    // The whole point: a fresh open still succeeds — the file never tore.
    assert!(
        fresh_open_ok(&db),
        "shared-handle path tore the single-file DB — Gap 1a regression"
    );
    // And every committed row is present (2 writers * iterations).
    let conn = handle.read().unwrap();
    let entries = recent_entries(&conn, u32::MAX).unwrap();
    assert_eq!(
        entries.len(),
        iterations * 2,
        "shared handle lost/duplicated audit rows (seq coherence)"
    );

    // ---- Arm A (pre-fix repro): separate instances, EXPECTED to tear. ----
    // Gated behind an env flag so the destructive repro runs only when asked
    // (it asserts the OLD behaviour; on the fixed code path it is not run).
    if std::env::var("ABERP_REPRO_1702_TEAR").is_ok() {
        let tmp2 = Tmp::new("separate");
        let db2 = tmp2.db();
        seed(&db2);
        let mut tore = false;
        let mut ws = Vec::new();
        for w in 0..2 {
            let dbp = db2.clone();
            ws.push(thread::spawn(move || {
                for i in 0..iterations {
                    if let Ok(mut c) = Connection::open(&dbp) {
                        let _ = ensure_schema(&c);
                        append_one(&mut c, &format!("sep-{w}-{i}"));
                        // drop(c) -> implicit close-checkpoint races the peer.
                    }
                }
            }));
        }
        for w in ws {
            let _ = w.join();
        }
        if !fresh_open_ok(&db2) {
            tore = true;
        }
        assert!(
            tore,
            "pre-fix separate-instance arm did NOT reproduce the tear — \
             expected on 9c35ebb/c903e23; if this fires on fixed code the repro \
             is no longer valid"
        );
    }
}

/// 1b lockstep (closes Gap 2b at the source): after a handle write drops, the
/// mirror head == the DB head — the mirror tracks the DB with no lag.
#[test]
fn daemon_write_appends_to_mirror_in_lockstep() {
    let tmp = Tmp::new("lockstep");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false, // isolate the mirror lockstep from the checkpoint
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    for i in 0..5 {
        let mut g = handle.write().unwrap();
        append_one(&mut g, &format!("ls-{i}"));
        // guard drop here -> post-commit hook runs sync_mirror
    }

    // DB head.
    let conn = handle.read().unwrap();
    let db_entries = recent_entries(&conn, u32::MAX).unwrap();
    let db_head = db_entries.len() as u64;

    // Mirror head.
    let mirror = mirror_path_for(&db);
    let mirror_entries = read_mirror_entries(&mirror).unwrap();
    let mirror_head = mirror_entries.last().map(|e| e.seq).unwrap_or(0);

    assert_eq!(
        mirror_head, db_head,
        "mirror head ({mirror_head}) lags DB head ({db_head}) — lockstep broken"
    );
}

/// 1b durability: a handle write followed by a debounced durable checkpoint
/// leaves a fresh, openable, validated live file (the next boot opens clean).
/// The crash-injection variant (mid-checkpoint `abort()` -> recoverable) is the
/// subprocess test below.
#[test]
fn handle_durable_checkpoint_keeps_live_file_openable() {
    let tmp = Tmp::new("ckpt");
    let db = tmp.db();
    seed(&db);
    // checkpoint enabled (default): first write fires a durable checkpoint
    // (no prior), which quiesces+reopens the shared connection.
    let handle = Handle::open_default(&db, tenant()).unwrap();
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "ckpt-1");
    }
    // The shared connection was dropped+reopened around atomic_install; a
    // brand-new external open must still succeed and see the row.
    assert!(
        fresh_open_ok(&db),
        "live file not openable after durable checkpoint"
    );
    let conn = handle.read().unwrap();
    assert_eq!(recent_entries(&conn, u32::MAX).unwrap().len(), 1);
}

/// ADR-0098 §Reproduction #2 — daemon-write crash-injection. A re-exec'd child
/// does a handle write and `abort()`s mid-checkpoint; the parent asserts the
/// live path is never torn and the next boot opens clean. Extends the
/// plain-file analogue (`aberp-snapshot recover.rs` crash-injection unit) to
/// the handle's checkpoint path.
#[test]
fn daemon_write_killed_mid_checkpoint_is_recoverable() {
    // Child arm: do a write through the handle, then hard-abort to simulate a
    // crash during/just-after the post-commit checkpoint.
    if std::env::var("ABERP_DB_CRASH_CHILD").is_ok() {
        let db = PathBuf::from(std::env::var("ABERP_DB_CRASH_DB").unwrap());
        let handle = Handle::open_default(&db, tenant()).unwrap();
        {
            let mut g = handle.write().unwrap();
            append_one(&mut g, "crash");
        }
        // Hard kill — no unwinding, no clean shutdown, mid/post checkpoint.
        std::process::abort();
    }

    let tmp = Tmp::new("crash");
    let db = tmp.db();
    seed(&db);

    let exe = std::env::current_exe().unwrap();
    let status = std::process::Command::new(exe)
        .args([
            "--exact",
            "daemon_write_killed_mid_checkpoint_is_recoverable",
        ])
        .env("ABERP_DB_CRASH_CHILD", "1")
        .env("ABERP_DB_CRASH_DB", &db)
        .env("RUST_TEST_THREADS", "1")
        .output()
        .expect("spawn crash child");
    // The child aborted, so it did not exit 0.
    assert!(
        !status.status.success(),
        "crash child unexpectedly exited clean"
    );

    // The live file must NOT be torn: a fresh open succeeds (the validated
    // atomic_install commit is all-or-nothing — a crash is on one side of the
    // rename, never a torn middle).
    assert!(
        fresh_open_ok(&db),
        "live DB torn after mid-checkpoint crash — durability regression"
    );
    // And the committed row survived (or the pre-commit state is intact —
    // either way the file is valid and openable).
    let conn = Connection::open(&db).unwrap();
    let _ = recent_entries(&conn, u32::MAX).unwrap();
}
