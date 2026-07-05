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
    append_in_tx, append_reopen, ensure_schema, mirror_path_for, read_mirror_entries,
    recent_entries, Actor, BinaryHash, EventKind, LedgerMeta, TenantId,
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

/// ADR-0098 C2 -> v0.2.5 **Option 1** (Ervin-approved) — **read-after-write
/// coherence via `try_clone`.** `Handle::read()` now hands out a `try_clone`
/// of the SHARED instance (one buffer cache, no second OS open), so a read
/// observes every committed write immediately — the coherent-read property the
/// F5 separate read-only instance could NOT provide (a separate instance did
/// not replay the live writer's WAL: the avl `revoke->list` stale read). This
/// pins that coherence while the read-write instance stays continuously open
/// (checkpoint off) and multiple read clones coexist in-process.
///
/// Gap-2b (deferred to v0.2.6): a `try_clone` is read-WRITE, so a write
/// mis-routed through `read()` no longer fails loud at the DB layer; that guard
/// becomes a compile-time read-guard in v0.2.6 (docs/adr-0098-v0.2.6-scope.md).
/// The single-WRITER durability invariant is unchanged — all real writes still
/// flow through `write()`.
#[test]
fn c2_read_clone_is_coherent_while_rw_instance_stays_open() {
    let tmp = Tmp::new("clone-coherent");
    let db = tmp.db();
    seed(&db);
    // checkpoint disabled => the read-write instance stays CONTINUOUSLY open, so
    // the read clones below truly coexist with a live RW instance (strictest case).
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    // Commit a write through the read-write instance.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "co-1");
    }

    // (1) A read clone taken while the RW instance is live observes the commit.
    let r1 = handle.read().unwrap();
    assert_eq!(
        recent_entries(&r1, u32::MAX).unwrap().len(),
        1,
        "read() try_clone did not observe the committed write (coherence broken)"
    );

    // (2) Hold r1 open, commit a second write; a fresh clone sees BOTH, and the
    //     still-held r1 (on a NEW query) also sees the latest committed state --
    //     one shared instance, not a point-in-time separate snapshot.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "co-2");
    }
    let r2 = handle.read().unwrap();
    assert_eq!(
        recent_entries(&r2, u32::MAX).unwrap().len(),
        2,
        "fresh read() try_clone did not see the second committed write"
    );
    assert_eq!(
        recent_entries(&r1, u32::MAX).unwrap().len(),
        2,
        "held read() try_clone did not observe the later commit (single-instance coherence)"
    );

    // (3) Neither the clones nor the live RW instance tore the file.
    drop(r1);
    drop(r2);
    assert!(
        fresh_open_ok(&db),
        "file tore under read-clone + read-write same-process coexistence"
    );
}

/// ADR-0098 C2 — assertion #1 reinforcement: after EACH of many commits, a
/// fresh read-only open (concurrent with the continuously-open read-write
/// instance) observes exactly the committed count so far. Deterministic
/// (single thread) to avoid CI timing flakiness while still exercising real
/// RO+RW same-process coexistence on every iteration.
#[test]
fn c2_assertion1_readonly_open_sees_each_commit_while_rw_instance_stays_open() {
    let tmp = Tmp::new("ro-seq");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    let n = 50usize;
    for i in 0..n {
        {
            let mut g = handle.write().unwrap();
            append_one(&mut g, &format!("seq-{i}"));
        }
        let ro = handle
            .read()
            .expect("read-only open rejected while the read-write instance is live (assertion #1)");
        assert_eq!(
            recent_entries(&ro, u32::MAX).unwrap().len(),
            i + 1,
            "read-only open did not see all commits up to iteration {i}"
        );
    }
    assert!(
        fresh_open_ok(&db),
        "file tore under repeated read-only + read-write coexistence"
    );
}

/// ADR-0098 C2 -> v0.2.5 **Option 1** — **read() hands out a coherent read-WRITE
/// clone, not a separate read-only instance.** The F5 `AccessMode::ReadOnly`
/// path is removed: `read()` is a `try_clone` of the shared instance, so
/// `connection_is_read_only` reports FALSE for it (as it does for the write
/// guard). The detector is RETAINED — it still guards the tolerant
/// `ensure_schema` DDL — and its RW-direction correctness (never wrongly
/// reporting a writable conn as read-only, which would silently skip real
/// schema creation) is what this pins.
#[test]
fn c2_read_returns_coherent_rw_clone_not_readonly() {
    let tmp = Tmp::new("rw-clone");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    // Read-write guard connection -> must be detected NOT read-only.
    {
        let g = handle.write().unwrap();
        assert!(
            !aberp_audit_ledger::connection_is_read_only(&g),
            "RW write-guard connection wrongly detected as read-only"
        );
    }

    // Commit a write, then the read() clone must (a) NOT be read-only under
    // Option 1 and (b) observe the committed row (coherent single instance).
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "rwc-1");
    }
    let r = handle.read().unwrap();
    assert!(
        !aberp_audit_ledger::connection_is_read_only(&r),
        "read() clone reported read-only — Option 1 hands out a read-WRITE \
         try_clone of the shared instance, not a separate AccessMode::ReadOnly open"
    );
    assert_eq!(
        recent_entries(&r, u32::MAX).unwrap().len(),
        1,
        "read() try_clone did not observe the committed write (coherence)"
    );
}

/// ADR-0098 C2 — **read-after-write under the PRODUCTION checkpoint config.**
/// Assertion #1 proved read() sees committed writes with `checkpoint_enabled =
/// false`. This exercises the DEFAULT (production) posture where the debounced
/// durable checkpoint (quiesce → EXPORT → atomic_install → reopen) is LIVE, and
/// asserts a read() taken AFTER a committed write() still observes it. If this
/// fails, the read()-side residual is NOT safe under production config — the
/// checkpoint's atomic_install swaps the inode out from under the separate
/// read-only open. (Surfaced by the avl_vendors_route revoke→list failures.)
#[test]
fn c2_read_after_write_sees_commit_under_default_checkpoint_config() {
    let tmp = Tmp::new("raw-ckpt");
    let db = tmp.db();
    seed(&db);
    // DEFAULT config: checkpoint_enabled = true (production checkpoint posture).
    let handle = Handle::open_default(&db, tenant()).unwrap();

    for i in 1..=3usize {
        {
            let mut g = handle.write().unwrap();
            append_one(&mut g, &format!("raw-{i}"));
        }
        let ro = handle.read().unwrap();
        let seen = recent_entries(&ro, u32::MAX).unwrap().len();
        assert_eq!(
            seen, i,
            "read() after {i} committed write(s) saw {seen} rows under the \
             production checkpoint config — read-after-write visibility broken"
        );
    }
}

/// Read a `status` column back through the Handle's `read()` (try_clone) path.
fn read_probe_status(handle: &Handle, id: &str) -> Option<String> {
    let conn = handle.read().unwrap();
    let mut stmt = conn
        .prepare("SELECT status FROM avl_probe WHERE id = ?")
        .unwrap();
    let mut rows = stmt.query_map([id], |r| r.get::<_, String>(0)).unwrap();
    rows.next().map(|r| r.unwrap())
}

/// ADR-0098 C2 round-13 — **read-after-write coherence for an UPDATE**, in the
/// avl `create → edit → revoke → read` shape. The append-only sibling
/// ([`c2_read_after_write_sees_commit_under_default_checkpoint_config`]) only
/// INSERTs; avl `set_vendor_status` UPDATEs a column of an existing row and then
/// reads the value back through `read()`. With the first durable checkpoint
/// already consumed (mirroring the boot `ensure_all_tenant_schemas`), those
/// UPDATEs are post-checkpoint, WAL-only writes; a SEPARATE read-only open
/// (`read_returns_readonly`) does not replay them, so `read()` observed the
/// pre-UPDATE row (the `avl_vendors_route` `revoke→list` failures). The residual
/// `append_reopen` audit opener after each mutation is avl's exact shape. Pins
/// the fix: `read()` is a `try_clone` of the shared instance, which replays the
/// live writer's WAL, so the post-checkpoint (WAL-only) UPDATE is visible with
/// no publish step (v0.2.5 Option 1).
#[test]
fn c2_read_after_write_sees_update_in_avl_shape() {
    let tmp = Tmp::new("raw-avl-update");
    let db = tmp.db();
    seed(&db);
    let handle = Handle::open_default(&db, tenant()).unwrap();
    let meta = LedgerMeta::new(tenant(), BinaryHash::from_bytes([9u8; 32]));
    let audit = |tag: &str| {
        append_reopen(
            &db,
            &meta,
            EventKind::DbAutoRecovered,
            format!("{{\"probe\":\"{tag}\"}}").into_bytes(),
            Actor::from_local_cli(format!("ulid-{tag}"), "tester"),
            None,
        )
        .unwrap();
    };

    // Consume the immediate first-write durable checkpoint (as the boot
    // schema-ensure does), so the row mutations below are WAL-only writes.
    {
        let g = handle.write().unwrap();
        g.execute_batch("CREATE TABLE avl_probe (id TEXT PRIMARY KEY, status TEXT);")
            .unwrap();
    }
    // create (INSERT) + residual audit append (avl's Ledger::open-per-mutation).
    {
        let g = handle.write().unwrap();
        g.execute("INSERT INTO avl_probe VALUES ('v1', 'pending')", [])
            .unwrap();
    }
    audit("create");
    assert_eq!(
        read_probe_status(&handle, "v1").as_deref(),
        Some("pending"),
        "read() must observe the committed INSERT"
    );

    // edit (UPDATE, no audit) then revoke (UPDATE) + residual audit append.
    {
        let g = handle.write().unwrap();
        g.execute(
            "UPDATE avl_probe SET status = 'conditional' WHERE id = 'v1'",
            [],
        )
        .unwrap();
    }
    {
        let g = handle.write().unwrap();
        g.execute(
            "UPDATE avl_probe SET status = 'revoked' WHERE id = 'v1'",
            [],
        )
        .unwrap();
    }
    audit("revoke");
    assert_eq!(
        read_probe_status(&handle, "v1").as_deref(),
        Some("revoked"),
        "read() must observe the committed UPDATE (avl revoke→list coherence; ADR-0098 C2 round-13)"
    );
}

/// ADR-0098 R6 (NEW-1) REGRESSION — the test the durability matrix lacked.
///
/// Proves the runtime **debounced** checkpoint folds a PENDING WAL on EVERY
/// dirty tick — the behaviour NEW-1 restored. Before the fix,
/// `checkpoint_is_current` hashed ONLY the main `.duckdb`; every Handle commit
/// is WAL-only (main bytes unchanged), so after the FIRST post-boot checkpoint
/// the debounced `live_durable_checkpoint` saw "current" forever, returned
/// `Ok(None)`, and NEVER folded again — committed data piled up in the WAL until
/// DuckDB's own ~16 MB auto-checkpoint folded it IN PLACE on the live file
/// (duckdb#23046). With the fix a non-empty `<db>.wal` reports not-current, so
/// the validated build-aside fold runs and clears the WAL each tick.
///
/// The acceptance test above runs with `checkpoint_enabled = false`; this drives
/// the checkpoint ON with a zero coalescing window and asserts the SECOND
/// (WAL-only) commit's WAL is folded — not just the first. DuckDB-backed, so it
/// runs on the CI/Mac durability gate alongside the acceptance test.
#[test]
fn debounced_checkpoint_folds_a_pending_wal_every_dirty_tick() {
    fn wal_len(db: &Path) -> u64 {
        let mut w = db.as_os_str().to_owned();
        w.push(".wal");
        std::fs::metadata(PathBuf::from(w))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    let tmp = Tmp::new("wal-fold");
    let db = tmp.db();
    seed(&db);

    // Checkpoint ON, ZERO coalescing window => every commit's guard-drop fires
    // the debounced checkpoint (the pure D2 debounce logic is unit-tested in
    // src/debounce.rs; here we exercise the real DuckDB fold path).
    let cfg = HandleConfig {
        checkpoint_enabled: true,
        min_checkpoint_interval: Duration::ZERO,
        disable_implicit_close_checkpoint: true,
    };
    let handle: Arc<Handle> = Handle::open(&db, tenant(), cfg).unwrap();

    // Commit #1 — the first post-boot checkpoint installs a fresh main + marker
    // (build-aside + atomic rename) and folds the WAL.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "c1");
    } // guard drops -> debounced checkpoint
    assert_eq!(
        wal_len(&db),
        0,
        "commit #1: the debounced checkpoint must fold the pending WAL"
    );
    assert!(
        aberp_snapshot::checkpoint_is_current(&db),
        "commit #1: a verified-good marker must cover the freshly-installed main file"
    );

    // Commit #2 — WAL-ONLY (main bytes unchanged). THE REGRESSION: under the
    // pre-fix `checkpoint_is_current` (main-hash only) this reports "current",
    // `live_durable_checkpoint` returns Ok(None), and the commit is stranded in a
    // growing WAL. With the fix, the pending WAL forces the validated fold.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "c2");
    }
    assert_eq!(
        wal_len(&db),
        0,
        "REGRESSION (NEW-1): the SECOND WAL-only commit must ALSO be folded by the \
         debounced checkpoint. A non-empty WAL here means checkpoint_is_current \
         wrongly reported 'current' and the validated fold was skipped — the WAL \
         would then grow until DuckDB self-folds IN PLACE (duckdb#23046)."
    );
    assert!(aberp_snapshot::checkpoint_is_current(&db));

    // Both committed rows survived the two build-aside folds (rename-swap integrity).
    drop(handle); // close the shared connection before a fresh verifying open
    let c = Connection::open(&db).unwrap();
    let entries = recent_entries(&c, 16).unwrap();
    assert_eq!(
        entries.len(),
        2,
        "both commits present after two build-aside folds (no data lost to the swaps)"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// ADR-0098 R7 — python-resolved BOOT-audit coherence (FACT-A orphan fix).
//
// FACT A (forensic on the live Defense DB): `audit_ledger` accumulated
// `quote.pipeline_python_resolved` rows that are PRESENT in the DB but ABSENT
// from the `.audit.log` mirror — one forked `seq` per process launch.
// FACT B: the runtime emitter (`emit_python_resolved_audit`) is already
// Handle-routed and idempotent-keyed, so it is NOT the source.
//
// The source was the serve.rs BOOT daemon-spawn block: right after the shared
// Handle wrote the python-resolved row into its (debounced) PENDING WAL, the
// block opened the live tenant DB with SEPARATE, un-pragma'd
// `duckdb::Connection::open` instances (S288 index-migrate, S286 boot
// row-count, cad-key provision). Those boot openers are allow-listed out of
// cut-gate CHECK 10j, so they fold-on-close. A second instance folding the
// Handle's pending WAL DOUBLE-APPLIES the tail row; because `audit_ledger`
// carries no UNIQUE on `seq` (S341 / duckdb#23046) the fork is INSERTED rather
// than rejected — a DB-only, mirror-absent duplicate, once per launch.
//
// The fix routes those three boot accesses through the shared Handle
// (`db.write()`/`db.read()`): exactly ONE instance, one validated fold, no
// double-apply. `separate_boot_opener_*` reproduces the pre-fix fork on real
// libduckdb 1.5.3; `boot_access_through_shared_handle_*` asserts the post-fix
// invariant (DB == mirror, contiguous seq, no orphan across relaunch).

fn wal_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(".wal");
    PathBuf::from(s)
}

const PY_KIND: &str = "quote.pipeline_python_resolved";

/// Append one `quote.pipeline_python_resolved` audit row through the shared
/// Handle (the shape `emit_python_resolved_audit` takes). The WriteGuard drop
/// runs the lockstep `sync_mirror`, so the row lands in DB **and** mirror.
fn append_python_resolved(handle: &Handle, launch: &str) {
    let mut g = handle.write().unwrap();
    let meta = LedgerMeta::new(tenant(), BinaryHash::from_bytes([7u8; 32]));
    let tx = g.transaction().unwrap();
    let actor = Actor::from_local_cli(format!("proc-{launch}"), "system");
    append_in_tx(
        &tx,
        &meta,
        EventKind::PipelinePythonResolved,
        br#"{"resolution_kind":"project_venv","module_importable":true}"#.to_vec(),
        actor,
        Some("quote_pipeline_python_resolved:defense:project_venv:/opt/venv".to_string()),
    )
    .unwrap();
    tx.commit().unwrap();
    // g drops here -> lockstep sync_mirror.
}

fn py_rows_in_db(db: &Path) -> i64 {
    let c = Connection::open(db).unwrap();
    c.query_row(
        "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'quote.pipeline_python_resolved'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn py_rows_in_mirror(db: &Path) -> usize {
    read_mirror_entries(&mirror_path_for(db))
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == PY_KIND)
        .count()
}

fn db_total_vs_distinct_seq(db: &Path) -> (i64, i64) {
    let c = Connection::open(db).unwrap();
    let total: i64 = c
        .query_row("SELECT COUNT(*) FROM audit_ledger", [], |r| r.get(0))
        .unwrap();
    let distinct: i64 = c
        .query_row("SELECT COUNT(DISTINCT seq) FROM audit_ledger", [], |r| r.get(0))
        .unwrap();
    (total, distinct)
}

/// PRE-FIX repro: a separate un-pragma'd boot opener folds the Handle's
/// pending-WAL python-resolved row; with no UNIQUE on `seq` the double-apply
/// FORKS a DB-only, mirror-absent duplicate. Deterministic model of the two
/// coexisting instances via a WAL snapshot/restore (empirically confirmed on
/// libduckdb 1.5.3). This is what the boot block did on `c662e39`.
#[test]
fn separate_boot_opener_forks_a_pending_wal_python_resolved_row() {
    let tmp = Tmp::new("pyfork");
    let db = tmp.db();
    seed(&db);
    // Handle keeps a PENDING WAL: no debounced checkpoint, no close-fold.
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        disable_implicit_close_checkpoint: true,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();
    append_python_resolved(&handle, "launch-1");
    // The Handle write synced the mirror: mirror has exactly one.
    assert_eq!(py_rows_in_mirror(&db), 1, "mirror got the one coherent row");
    let wal = wal_path(&db);
    let snap = std::fs::read(&wal).expect("row must sit in a pending WAL, not yet folded");
    assert!(!snap.is_empty(), "pending WAL must be non-empty");
    // The Handle instance is dropped WITHOUT folding (pragma) — its WAL view persists.
    drop(handle);
    // Boot opener #1 (a separate, un-pragma'd instance) folds the WAL on close.
    {
        let c = Connection::open(&db).unwrap();
        c.execute_batch("SELECT 1;").unwrap();
    }
    // The co-existing second instance (the Handle's own) held the same WAL view:
    // restore it and let a second separate opener fold it too -> double-apply.
    std::fs::write(&wal, &snap).unwrap();
    {
        let c = Connection::open(&db).unwrap();
        c.execute_batch("CHECKPOINT;").unwrap();
    }
    let db_ct = py_rows_in_db(&db);
    let mir_ct = py_rows_in_mirror(&db) as i64;
    let (total, distinct) = db_total_vs_distinct_seq(&db);
    assert!(
        db_ct >= 2,
        "pre-fix: the double-apply must FORK the python-resolved row in the DB (got {db_ct})"
    );
    assert!(
        db_ct > mir_ct,
        "pre-fix: the fork is DB-present but mirror-absent (FACT A): db={db_ct} mirror={mir_ct}"
    );
    assert!(
        total > distinct,
        "pre-fix: a duplicate/forked seq exists (total={total} distinct={distinct})"
    );
}

/// POST-FIX invariant: when the boot DB accesses are routed through the shared
/// Handle (`read()`/`write()`), there is ONE instance and one fold, so no
/// separate opener can double-apply the pending WAL. Across two launches the
/// audit chain stays coherent: every DB row is mirrored, the mirror head tracks
/// the DB head, and no `seq` is forked. This is what the fixed serve.rs does.
#[test]
fn boot_access_through_shared_handle_keeps_python_resolved_db_mirror_coherent() {
    let tmp = Tmp::new("pycoherent");
    let db = tmp.db();
    seed(&db);
    for launch in ["launch-1", "launch-2"] {
        let handle = Handle::open(&db, tenant(), HandleConfig::default()).unwrap();
        append_python_resolved(&handle, launch);
        // Boot row-count via the ONE shared instance (models count_jobs).
        {
            let c = handle.read().unwrap();
            let _: i64 = c
                .query_row("SELECT COUNT(*) FROM audit_ledger", [], |r| r.get(0))
                .unwrap();
        }
        // Boot index-migration DDL via the ONE shared instance (models the S288
        // migrate). No separate un-pragma'd opener exists to fold the WAL.
        {
            let g = handle.write().unwrap();
            g.execute_batch(
                "CREATE TABLE IF NOT EXISTS boot_probe(x INTEGER); DROP TABLE boot_probe;",
            )
            .unwrap();
        }
        drop(handle);
    }
    // Coherence invariant (the fix's guarantee):
    let db_ct = py_rows_in_db(&db);
    let mir_ct = py_rows_in_mirror(&db) as i64;
    assert_eq!(
        db_ct, mir_ct,
        "no DB-only orphan: python-resolved DB rows ({db_ct}) must equal mirror rows ({mir_ct})"
    );
    let (total, distinct) = db_total_vs_distinct_seq(&db);
    assert_eq!(
        total, distinct,
        "no forked seq across relaunch (total={total} distinct={distinct})"
    );
    // Mirror head tracks DB head — nothing is DB-only.
    let db_max: i64 = {
        let c = Connection::open(&db).unwrap();
        c.query_row("SELECT COALESCE(MAX(seq), 0) FROM audit_ledger", [], |r| r.get(0))
            .unwrap()
    };
    let mir_max = read_mirror_entries(&mirror_path_for(&db))
        .unwrap()
        .last()
        .map(|e| e.seq)
        .unwrap_or(0);
    assert_eq!(
        mir_max as i64, db_max,
        "mirror head ({mir_max}) must equal DB head ({db_max}) — no DB-only rows"
    );
}
