//! ADR-0111 — **the checkpoint swap must never orphan the shared connection.**
//!
//! # The defect these tests pin
//!
//! `aberp_snapshot::durable_checkpoint` ends in `atomic_install`
//! (`crates/aberp-snapshot/src/crash_safe.rs:221`): it `rename`s the finished
//! staging file over the live path and then **deletes `<db>.wal`**. Both steps
//! are correct — and both are only sound while the shared DuckDB connection is
//! quiesced, which is what `Handle::run_durable_checkpoint_locked` arranges
//! (drop the connection → checkpoint → reopen on the freshly-installed inode).
//!
//! Two production callers ran that primitive on a **path**, from
//! `spawn_blocking`, holding no lock:
//!
//! * `apps/aberp/src/snapshot.rs` — the periodic snapshot daemon's tick, and
//! * `apps/aberp/src/live_checkpoint.rs` — the ADR-0095 §3 post-write debouncer.
//!
//! After their `rename` the shared connection still held an fd on the OLD,
//! now-unlinked inode. Every subsequent commit landed **in that orphan**, which
//! the kernel frees at process exit — and yet those commits were perfectly
//! visible to the post-commit `sync_mirror` running on that same connection, so
//! they were durably appended to the JSONL mirror. The mirror ends up **AHEAD
//! of the DB**: the one direction `preserve_ahead_mirror` calls unrecoverable,
//! the direction Defense's boot auto-heal RESURRECTS from, and the root behind
//! the recurring audit-chain fork incidents. Not a shutdown-only effect — a
//! long session silently loses every row written after the first daemon tick.
//!
//! # Why these tests are shaped the way they are
//!
//! Deterministic, milliseconds, no threads, no waiting on the 60 s D2 heal
//! window: `checkpoint_enabled = false` switches the handle's AUTOMATIC cadence
//! off so the ONLY checkpoint in the test is the one the test asks for, and
//! `Handle::checkpoint_now` is deliberately unconditional so that explicit ask
//! still happens. Read that pairing before "simplifying" either half — flipping
//! `checkpoint_now` to honour `checkpoint_enabled` makes every assertion below
//! vacuously green.
//!
//! [`checkpoint_actually_swaps_the_live_inode`] is the anti-vacuity control and
//! must be read as part of the crux test: `durable_checkpoint` ABORTS (leaving
//! the live file untouched) if its WAL fence trips, and a no-op checkpoint
//! cannot orphan anything, so "no rows lost" would pass for the wrong reason.
//!
//! These open **real DuckDB** files. There is no `cfg` gate on them — the whole
//! `aberp-db` crate depends on the bundled libduckdb amalgamation, so if that
//! builds these run, and if it does not the crate does not compile at all.
//! (The sibling suites' "Mac/CI only" headers describe the SAW-OFF sandbox,
//! where the amalgamation cannot build; that is a property of the environment,
//! not a `#[cfg]`. Said plainly here because the PR #41 adversarial went
//! looking for the cfg those headers imply and found none.) The unit-testable
//! D2 debounce logic lives in `src/debounce.rs` and runs anywhere.

use std::path::{Path, PathBuf};

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
        let p = std::env::temp_dir().join(format!(
            "aberp-db-ckpt-orphan-{label}-{}-{n}",
            std::process::id()
        ));
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

/// Append one audit row on a connection (the shape every daemon write takes).
fn append_one(conn: &mut Connection, label: &str) {
    let meta = LedgerMeta::new(tenant(), BinaryHash::from_bytes([7u8; 32]));
    let tx = conn.transaction().unwrap();
    let actor = Actor::from_local_cli(format!("ulid-{label}"), "tester");
    append_in_tx(
        &tx,
        &meta,
        EventKind::DbAutoRecovered,
        format!("{{\"probe\":\"{label}\"}}").into_bytes(),
        actor,
        None,
    )
    .unwrap();
    tx.commit().unwrap();
}

/// The live file's identity. `atomic_install`'s `rename` installs a DIFFERENT
/// inode at the same path, so a change here IS the swap — and an fd opened
/// before it now points at an unlinked file nobody will ever read again.
///
/// Unix-only by construction (the tree's whole `cfg(windows)` surface in
/// `aberp-db` is the directory-`fsync` no-op), which is why the tests that use
/// it carry `#[cfg(unix)]` individually — see the module header on why there is
/// no build gate beyond that.
#[cfg(unix)]
fn file_identity(p: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).expect("live DB must exist");
    (m.dev(), m.ino())
}

/// The audit payloads a FRESH external open can see — i.e. what actually
/// survives on disk, which is the only definition of "committed" that matters
/// once the process is gone.
fn payloads_on_disk(db: &Path) -> Vec<String> {
    let conn = Connection::open(db).expect("fresh open of the live DB");
    recent_entries(&conn, u32::MAX)
        .expect("read audit rows")
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).to_string())
        .collect()
}

/// **Anti-vacuity control — read this before trusting the crux test below.**
///
/// `durable_checkpoint` refuses (live file untouched) whenever its WAL fence
/// trips, and `live_durable_checkpoint` is a no-op when a verified-good marker
/// already covers the file. Either outcome would make "no rows were lost" pass
/// for a reason that has nothing to do with the fix. So pin that the checkpoint
/// really does install a new inode over the live path: if this ever goes red,
/// every other assertion in this file is meaningless until it is green again.
#[cfg(unix)]
#[test]
fn checkpoint_actually_swaps_the_live_inode() {
    let tmp = Tmp::new("swap-happens");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "dirty");
    }

    let before = file_identity(&db);
    handle.checkpoint_now();
    let after = file_identity(&db);

    assert_ne!(
        before, after,
        "checkpoint_now did NOT swap the live file (same dev/inode {before:?}) — \
         the checkpoint was skipped or aborted, so the orphan test below proves \
         NOTHING. Check the WAL fence / checkpoint_is_current marker before \
         reading any other result in this file as a pass."
    );
}

/// **THE crux.** A daemon-cadence checkpoint lands mid-session, between two
/// commits. Every committed row must survive on disk and the mirror must not
/// run ahead of the DB.
///
/// Red before ADR-0111 (with the checkpoint taken on a PATH outside the writer
/// lock, as `snapshot.rs` and `live_checkpoint.rs` did): row B commits into the
/// unlinked inode, so a fresh open sees only A while `sync_mirror` — reading
/// the very same orphaned connection — has already written B into the mirror.
/// Green after: `checkpoint_now` holds the writer mutex, quiesces the shared
/// connection so the checkpoint is the sole opener, and reopens on the newly
/// installed inode, so B lands in the file that survives.
#[test]
fn checkpoint_between_writes_loses_no_rows_and_never_puts_the_mirror_ahead() {
    let tmp = Tmp::new("orphan");
    let db = tmp.db();
    seed(&db);
    // The handle's OWN debounced checkpoint off: the only checkpoint in this
    // test is the explicit one below, standing in for the daemon tick.
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    // Row A — committed BEFORE the checkpoint. Survives either way: the
    // checkpoint's logical EXPORT captures it.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "A-before-checkpoint");
    }

    // The daemon's move, mid-session. Pre-ADR-0111 this ran as
    // `aberp_snapshot::live_durable_checkpoint(&db_path, &tenant)` on a
    // blocking task with no lock held; now it routes through the handle.
    handle.checkpoint_now();

    // Row B — committed AFTER the swap. This is the row the defect eats.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "B-after-checkpoint");
    }

    // Close the handle the way a clean process exit does. The runtime
    // connection has checkpoint-on-close disabled, so this folds nothing in
    // place — B is durable in `<db>.wal` and replays on the next open.
    drop(handle);

    // 1. Both rows must be on disk. This is the assertion the orphan breaks.
    let on_disk = payloads_on_disk(&db);
    assert!(
        on_disk.iter().any(|p| p.contains("A-before-checkpoint")),
        "row A (pre-checkpoint) missing from the live DB — the checkpoint did \
         not even preserve what it exported: {on_disk:?}"
    );
    assert!(
        on_disk.iter().any(|p| p.contains("B-after-checkpoint")),
        "ROW B LOST. It committed after the checkpoint's atomic_install and \
         went into the ORPHANED inode the shared connection still held open; \
         the kernel freed it at exit. A fresh open sees only {on_disk:?}. This \
         is the mirror-ahead root cause — the checkpoint must run under the \
         writer lock with a reopen (Handle::checkpoint_now)."
    );
    assert_eq!(
        on_disk.len(),
        2,
        "expected exactly the two committed rows on disk, got {on_disk:?}"
    );

    // 2. And the mirror must not be AHEAD of the DB. `sync_mirror` runs on the
    //    shared connection when each guard drops, so an orphaned connection
    //    durably mirrors rows the live file no longer contains — the exact
    //    `MirrorAheadOfDb` state boot refuses (and Defense's auto-heal replays).
    let mirror = read_mirror_entries(&mirror_path_for(&db)).expect("mirror readable");
    let mirror_head = mirror.last().map(|e| e.seq).unwrap_or(0);
    assert_eq!(
        mirror.len(),
        on_disk.len(),
        "MIRROR AHEAD OF DB: mirror has {} entries (head seq {mirror_head}) but \
         the live DB has {}. The post-commit sync_mirror durably recorded rows \
         that only ever existed in the orphaned inode.",
        mirror.len(),
        on_disk.len()
    );
    assert_eq!(
        mirror_head, 2,
        "mirror head should be the second committed row (seq 2), got {mirror_head}"
    );
}

/// **The belt: the ADR-0111 inode fence.**
///
/// `Handle::checkpoint_now` fixes every checkpoint this process takes. It
/// cannot fix a swap taken by someone else — a second `aberp` invocation, an
/// operator restore, a backup tool — and it cannot, on its own, prove the
/// census gate was never defeated. The fence is what catches those: the guard
/// compares the live file's `(dev, ino)` against the one its connection was
/// opened on, and on a mismatch refuses to certify anything.
///
/// Both hooks it skips would actively make the damage worse:
/// `fsync_data_paths` opens BY PATH, so it would flush and journal the NEW
/// inode — certifying bytes unrelated to this commit; `sync_mirror` reads the
/// ORPHANED connection, so it would durably mirror rows the live DB does not
/// have. This asserts the mirror stays level (behind is benign) and that the
/// money path's ack FAILS instead of certifying a lost write.
#[cfg(unix)]
#[test]
fn an_out_of_process_swap_fails_the_ack_and_never_advances_the_mirror() {
    let tmp = Tmp::new("fence");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "A-before-swap");
    }
    let mirror_after_a = read_mirror_entries(&mirror_path_for(&db)).unwrap().len();
    assert_eq!(mirror_after_a, 1, "row A should be mirrored normally");

    // The hostile swap: the path-based primitive, exactly as an out-of-process
    // writer (or a re-introduced un-censused caller) would run it. It renames a
    // fresh inode over the live path and unlinks `<db>.wal`.
    let swapped = file_id_changed_by(&db, || {
        aberp_snapshot::durable_checkpoint(&db, TENANT).expect("out-of-process checkpoint");
    });
    assert!(
        swapped,
        "the simulated out-of-process checkpoint did not change the live inode — \
         nothing was orphaned, so this test proves nothing"
    );

    // A write on the now-stranded connection. It "succeeds" into the orphan;
    // the guard drop must detect the swap.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "B-into-the-orphan");
    }

    // 1. The money path must be told, not quietly acked.
    let err = handle
        .durable_ack()
        .expect_err("durable_ack must FAIL after the live file was swapped away");
    assert!(
        matches!(err, aberp_db::DbError::LiveFileSwapped { .. }),
        "expected DbError::LiveFileSwapped, got {err:?}"
    );

    // 2. And the mirror must NOT have grown: row B exists only in the orphan,
    //    so mirroring it is precisely the mirror-ahead fork.
    let mirror_now = read_mirror_entries(&mirror_path_for(&db)).unwrap().len();
    assert_eq!(
        mirror_now, mirror_after_a,
        "MIRROR ADVANCED past the DB on a swapped file — the fence did not skip sync_mirror"
    );

    // 3. The handle heals: the next write reopens on the live inode.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "C-after-heal");
    }
    handle
        .durable_ack()
        .expect("the ack must succeed again once the handle has reopened on the live file");
    drop(handle);

    let on_disk = payloads_on_disk(&db);
    assert!(
        on_disk.iter().any(|p| p.contains("A-before-swap")),
        "row A missing after the swap: {on_disk:?}"
    );
    assert!(
        on_disk.iter().any(|p| p.contains("C-after-heal")),
        "the handle did not reopen on the live file after the fence tripped: {on_disk:?}"
    );
}

/// Run `f` and report whether the file at `p` came back with a different
/// identity — i.e. whether a `rename` really swapped it.
#[cfg(unix)]
fn file_id_changed_by(p: &Path, f: impl FnOnce()) -> bool {
    let before = file_identity(p);
    f();
    file_identity(p) != before
}

/// **The `durable_ack` by-path hole** (PR #41 adversarial, finding 3).
///
/// The guard-drop fence only covers acks with a PARKED outcome. `durable_ack`'s
/// other arm — no write has completed since the last claim, so `last_ack` is
/// `None` — falls through to `fsync_data_paths()`, which opens the DB, the WAL
/// and the parent dir **by path**. After a swap those paths name the NEW inode,
/// so the fsync succeeds, the durability journal records it, and the ack returns
/// `Ok(())`: the operator is told a write is power-loss durable on the strength
/// of flushing a file that has nothing to do with it.
///
/// This is the arm a money path hits when its write was a no-op — exactly the
/// case where "nothing to flush" must not be confused with "flushed something".
#[cfg(unix)]
#[test]
fn an_unparked_ack_over_a_swapped_file_fails_instead_of_fsyncing_the_new_inode() {
    let tmp = Tmp::new("ack-unparked");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    // A write, then CLAIM its parked outcome — so `last_ack` is back to `None`
    // and the next ack takes the fall-through arm under test.
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "A-before-swap");
    }
    handle
        .durable_ack()
        .expect("the first ack claims a healthy parked outcome");

    // Sanity: with no swap, the unparked arm still succeeds. Without this the
    // test could pass simply because the arm always fails.
    handle
        .durable_ack()
        .expect("an unparked ack on an UNSWAPPED file must still succeed");

    // The out-of-process swap. No write follows it, so nothing parks an outcome
    // and the guard-drop fence never runs — this arm is the only guard left.
    let swapped = file_id_changed_by(&db, || {
        aberp_snapshot::durable_checkpoint(&db, TENANT).expect("out-of-process checkpoint");
    });
    assert!(swapped, "the simulated swap did not change the live inode");

    let err = handle
        .durable_ack()
        .expect_err("an unparked ack over a SWAPPED file must FAIL, not fsync the new inode");
    assert!(
        matches!(err, aberp_db::DbError::LiveFileSwapped { .. }),
        "expected DbError::LiveFileSwapped, got {err:?}"
    );
}

/// Reads must not keep serving the pre-swap file.
///
/// `Handle::read()` hands out a `try_clone` of the shared connection. If that
/// connection is stranded on a swapped-away inode the clone reads the OLD file
/// — so after an operator restore the UI would show pre-restore rows
/// indefinitely, because nothing on the read path ever reopens. Cheaper to fix
/// than the write path (no commit is at stake): drop and reopen.
#[cfg(unix)]
#[test]
fn reads_do_not_keep_serving_the_pre_swap_file() {
    let tmp = Tmp::new("read-fence");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "A-before-swap");
    }
    assert_eq!(
        recent_entries(&handle.read().unwrap(), u32::MAX)
            .unwrap()
            .len(),
        1
    );

    // Swap in a file with DIFFERENT contents — a second row the stranded
    // connection has never seen. Built by checkpointing a scratch DB and
    // renaming it over the live path, i.e. what an operator restore does.
    let scratch = tmp.0.join("restored.duckdb");
    seed(&scratch);
    {
        let mut c = Connection::open(&scratch).unwrap();
        append_one(&mut c, "A-before-swap");
        append_one(&mut c, "B-only-in-the-restored-file");
        c.execute_batch("CHECKPOINT;").unwrap();
    }
    let before = file_identity(&db);
    std::fs::rename(&scratch, &db).unwrap();
    let _ = std::fs::remove_file(format!("{}.wal", db.display()));
    assert_ne!(
        before,
        file_identity(&db),
        "the rename did not swap the file"
    );

    // The read must observe the RESTORED file, not the orphan.
    let seen = recent_entries(&handle.read().unwrap(), u32::MAX)
        .unwrap()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).to_string())
        .collect::<Vec<_>>();
    assert!(
        seen.iter()
            .any(|p| p.contains("B-only-in-the-restored-file")),
        "read() served the PRE-SWAP file — the shared connection is still on the \
         orphaned inode and nothing reopened it: {seen:?}"
    );
}

/// The clean-shutdown checkpoint must not be able to block process exit.
///
/// The drain that runs before it does NOT guarantee the writer mutex is free:
/// `ShutdownCoordinator::shutdown` races each daemon's `JoinHandle` against a
/// shared budget and, on expiry, DROPS it — which detaches the tokio task
/// rather than aborting it, so a straggler can still be inside a `WriteGuard`.
/// `checkpoint_now_within` must give up on that rather than park (S213).
#[test]
fn a_bounded_checkpoint_gives_up_when_a_writer_still_holds_the_lock() {
    let tmp = Tmp::new("bounded");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    // Stand in for the detached straggler: hold a guard across the call.
    let guard = handle.write().unwrap();
    let started = std::time::Instant::now();
    let outcome = handle.checkpoint_now_within(std::time::Duration::from_millis(200));
    let waited = started.elapsed();
    drop(guard);

    assert!(
        outcome.is_none(),
        "checkpoint_now_within returned {outcome:?} while a WriteGuard was held — it must \
         report the lock miss, not somehow run"
    );
    assert!(
        waited < std::time::Duration::from_secs(2),
        "checkpoint_now_within waited {waited:?} on a 200ms budget — process exit would hang \
         behind a detached straggler (S213)"
    );

    // And once the writer is gone it runs normally — the bound must not have
    // left the handle in a state where checkpoints stop happening.
    assert_eq!(
        handle.checkpoint_now_within(std::time::Duration::from_secs(5)),
        Some(true),
        "the bounded checkpoint failed on a FREE lock"
    );
}

/// The checkpoint must leave the shared handle USABLE, not merely intact on
/// disk: `run_durable_checkpoint_locked` reopens the connection itself, and a
/// caller that keeps writing after a daemon tick must not be silently writing
/// nowhere. Pins the reopen half of the fix independently of the row-loss
/// assertion above (which a lucky WAL replay could otherwise mask).
#[test]
fn handle_keeps_serving_reads_and_writes_across_a_checkpoint() {
    let tmp = Tmp::new("reopen");
    let db = tmp.db();
    seed(&db);
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&db, tenant(), cfg).unwrap();

    // Three checkpoints interleaved with writes — a repeated daemon cadence,
    // which is what a real long-running serve process does all day.
    for i in 0..3 {
        {
            let mut g = handle.write().unwrap();
            append_one(&mut g, &format!("round-{i}"));
        }
        handle.checkpoint_now();
    }
    {
        let mut g = handle.write().unwrap();
        append_one(&mut g, "final");
    }

    // The handle's own view (a try_clone of the reopened instance).
    let conn = handle.read().unwrap();
    assert_eq!(
        recent_entries(&conn, u32::MAX).unwrap().len(),
        4,
        "the handle's own connection lost rows across repeated checkpoints"
    );

    drop(handle);
    let on_disk = payloads_on_disk(&db);
    assert_eq!(
        on_disk.len(),
        4,
        "rows lost across repeated checkpoints: {on_disk:?}"
    );
    let mirror = read_mirror_entries(&mirror_path_for(&db)).expect("mirror readable");
    assert_eq!(
        mirror.len(),
        4,
        "mirror diverged from the DB across repeated checkpoints"
    );
}
