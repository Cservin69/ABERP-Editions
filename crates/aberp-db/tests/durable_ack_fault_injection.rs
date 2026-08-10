//! ADR-0110 D3 — fault injection + coverage pins for [`Handle::durable_ack`].
//!
//! Ported from Portable PROD_v2.33.6, plus two Defense-specific pins the
//! Portable suite does not carry (see `d3_ack_journals_the_wal_not_just_the_main_db`
//! and `d3_ack_reaches_the_wal_fault` below).
//!
//! # What these are for
//!
//! `durable_ack` is a promise about bytes on stable storage. The one thing a
//! unit test cannot do is pull the power, so the risk is a `durable_ack` that
//! *looks* like it works — returns `Ok`, journals paths — while never actually
//! reaching the filesystem. Every other D3 gate reads that journal (the
//! cut-gate reads the call sites; a durability spec reads `fsynced_paths`), so
//! a hollow ack would leave the whole apparatus green through a total revert to
//! the un-fsynced posture.
//!
//! These tests break the *reach* — remove the file the ack must open — and
//! require the ack to fail loud and typed. An ack that never opened the path
//! cannot notice it is gone, so `Ok` here is proof the fsync is not happening.
//!
//! # Mutation verification
//!
//! A pin that cannot go red is not a pin. Verified in both directions before
//! landing: delete the `fsync_path(path)?` line from `fsync_and_record` and
//! this file goes RED while every other D3 gate stays green — which is the
//! whole point of it existing.
//!
//! # Scope
//!
//! `$TMPDIR` only. Nothing here touches `~/.aberp/**` or any tenant database.

use std::path::PathBuf;

use aberp_audit_ledger::TenantId;
use aberp_db::{DbError, Handle};

/// A scratch tenant directory under `$TMPDIR`, unique per process + call.
fn scratch_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "aberp-adr0110-d3-fault-{tag}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir scratch tenant dir");
    dir
}

fn tenant() -> TenantId {
    TenantId::new("tenant-adr0110-d3-fault").expect("test tenant id is valid")
}

/// Force DuckDB to actually produce a `<db>.wal` by committing a real write on
/// the shared handle. The runtime pragmas disable checkpoint-on-close and push
/// `wal_autocheckpoint` to 1TB, so the WAL persists rather than being folded —
/// which is precisely the condition that makes D3 necessary.
fn commit_a_row(handle: &Handle) {
    let mut guard = handle.write().expect("shared writer");
    let conn = guard.conn();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS d3_probe (id INTEGER); INSERT INTO d3_probe VALUES (1);",
    )
    .expect("commit a row so DuckDB writes a WAL");
}

/// **The reach test.** With the main DB file removed from under the handle,
/// `durable_ack` must return `Err` — it cannot `fsync` a path that is not
/// there, and it must say so rather than claim success.
///
/// Deleting the *path* (not the inode) is what makes this hermetic: the
/// `Handle`'s already-open `Connection` keeps its file descriptor and stays
/// perfectly usable, so nothing else in the process is disturbed and the test
/// has no cleanup hazard. Only a fresh `File::open` of the path — which is
/// exactly what the durable-ack reach does — sees the `ENOENT`.
#[test]
fn durable_ack_fails_loud_when_the_filesystem_reach_is_broken() {
    let dir = scratch_dir("reach");
    let db = dir.join("aberp.duckdb");
    let handle = Handle::open_default(&db, tenant()).expect("open shared Handle");

    // Sanity: on an intact tenant the ack succeeds and journals the main file.
    // Without this the assertion below could pass on a `durable_ack` that is
    // broken for some entirely different reason.
    handle
        .durable_ack()
        .expect("durable_ack must succeed on an intact tenant");
    assert!(
        handle.fsynced_paths().iter().any(|p| p == &db),
        "precondition: an intact durable_ack must journal the main DB file; \
         journal was {:?}",
        handle.fsynced_paths(),
    );

    // ── Break the reach ────────────────────────────────────────────────────
    std::fs::remove_file(&db).expect("remove the main DB file out from under the handle");

    let err = handle.durable_ack().expect_err(
        "ADR-0110 D3 REGRESSION: durable_ack returned Ok with the main DB file \
         DELETED. It therefore never opened or fsync'd it, so the durability \
         journal is recording syncs that are not happening — every other D3 gate \
         reads that journal and would stay green through a total revert to the \
         un-fsynced posture.",
    );

    // The error must be the typed durability failure naming the path, not some
    // incidental DuckDB error: a money path is going to turn this into an
    // operator-facing 5xx, so it has to say WHICH file could not be made
    // durable (never silently, and never uselessly).
    match err {
        DbError::DurableAck { ref path, .. } => {
            assert_eq!(path, &db, "DurableAck must name the file it could not sync")
        }
        other => panic!(
            "expected DbError::DurableAck naming {}, got {other:?}",
            db.display()
        ),
    }
    assert!(
        err.to_string().contains("durable-ack"),
        "the error text must be greppable as a durability fault; got: {err}"
    );
}

/// **Defense-specific pin #1.** The WAL — not just the main file — must reach
/// the durability journal.
///
/// This matters more on Defense than on Portable. Defense already `fsync`s the
/// audit MIRROR on every commit (the `WriteGuard` drop hook) and eventually
/// installs a fully-`fsync`'d replacement of the main file via the debounced D2
/// checkpoint. What no pre-D3 path ever flushed is the **WAL**, and the WAL is
/// where a just-committed money-path row actually lives for up to a checkpoint
/// interval. An ack that flushed only the main file would therefore look
/// plausible, pass the reach test above, and still lose the invoice.
#[test]
fn d3_ack_journals_the_wal_not_just_the_main_db() {
    let dir = scratch_dir("wal-journal");
    let db = dir.join("aberp.duckdb");
    let wal = dir.join("aberp.duckdb.wal");
    let handle = Handle::open_default(&db, tenant()).expect("open shared Handle");

    commit_a_row(&handle);
    assert!(
        wal.exists(),
        "precondition: a committed write with checkpoint-on-close disabled must \
         leave a WAL at {} — without one this test proves nothing",
        wal.display()
    );

    handle
        .durable_ack()
        .expect("durable_ack on an intact tenant");

    let journal = handle.fsynced_paths();
    assert!(
        journal.iter().any(|p| p == &db),
        "the main DB file is missing from the durability journal: {journal:?}"
    );
    assert!(
        journal.iter().any(|p| p == &wal),
        "ADR-0110 D3 REGRESSION: the WAL is NOT in the durability journal \
         ({journal:?}). The committed rows live in the WAL until the debounced \
         D2 checkpoint folds them (up to 60s later), so an ack that skips it \
         promises a durability it did not achieve — exactly the gap D3 exists \
         to close on Defense."
    );
}

/// **Defense-specific pin #2.** A tenant with no WAL must still ack, and must
/// not journal a WAL it never synced.
///
/// The WAL is flushed behind an `if self.wal_path.exists()` guard. Two ways
/// that can go wrong in opposite directions: unconditionally opening
/// `wal_path` would fail every ack on a WAL-less tenant, and journalling the
/// WAL without syncing it would make the durability journal lie — which the
/// power-loss spec builds its copy manifest from, so it would silently widen
/// the "durable" set to include a file that is not.
///
/// Uses a handle with NO committed write, so `last_ack` is empty and
/// `durable_ack` takes its direct-flush fallback. That exercises the fallback
/// path as well.
#[test]
fn d3_ack_on_a_wal_less_tenant_succeeds_and_journals_no_wal() {
    let dir = scratch_dir("wal-absent");
    let db = dir.join("aberp.duckdb");
    let wal = dir.join("aberp.duckdb.wal");
    let handle = Handle::open_default(&db, tenant()).expect("open shared Handle");

    assert!(
        !wal.exists(),
        "premise: a handle with no committed write must have no WAL"
    );

    handle
        .durable_ack()
        .expect("a tenant whose WAL is absent must still ack on the main file alone");

    assert!(
        handle.fsynced_paths().iter().any(|p| p == &db),
        "the main DB file must still be journalled: {:?}",
        handle.fsynced_paths()
    );
    assert!(
        !handle.fsynced_paths().iter().any(|p| p == &wal),
        "an absent WAL must not be journalled as synced — the journal must mean \
         'this is on stable storage', never 'we tried': {:?}",
        handle.fsynced_paths()
    );
}

/// **B3 pin (PR #37 adversarial).** The parent-directory `fsync` must be
/// recorded, and therefore assertable.
///
/// Before this, dropping just the directory `fsync` was invisible to
/// EVERYTHING: both test files, all four gate checks and all nine probes —
/// because `fsync_path(parent)` bypassed the journal entirely. The directory
/// flush is what makes the rename/create of the DB and WAL entries themselves
/// durable on POSIX, so an unpinned one is a silent hole.
///
/// Recorded in its own journal (`fsynced_dirs`) rather than `fsynced_paths`, so
/// the power-loss spec's copy manifest keeps meaning "files carrying rows".
#[test]
fn b3_the_tenant_directory_fsync_is_recorded_and_therefore_pinned() {
    let dir = scratch_dir("dir-journal");
    let db = dir.join("aberp.duckdb");
    let handle = Handle::open_default(&db, tenant()).expect("open shared Handle");

    handle
        .durable_ack()
        .expect("durable_ack on an intact tenant");

    let dirs = handle.fsynced_dirs();
    assert!(
        dirs.iter().any(|p| p == &dir),
        "ADR-0110 D3 B3 REGRESSION: the tenant directory {} is not in the \
         directory durability journal ({dirs:?}). Either the parent-directory \
         fsync was dropped, or it stopped being recorded — and an unrecorded \
         fsync is one no test and no gate can see.",
        dir.display()
    );

    // Precision: the directory must NOT leak into the file journal, or the
    // power-loss spec would try to treat it as a file to copy.
    assert!(
        !handle.fsynced_paths().iter().any(|p| p == &dir),
        "the tenant directory must stay OUT of fsynced_paths (files only): {:?}",
        handle.fsynced_paths()
    );
}

/// **Fail-closed park.** An unclaimed flush FAILURE must not be erased by a
/// later successful write.
///
/// The B2 reorder moved the flush into the guard drop and parks its outcome for
/// `durable_ack` to claim. A money path holds no lock between dropping its
/// guard and claiming, so another writer (Defense runs many daemons) can drop a
/// guard in that window. If a later `Ok` overwrote an unclaimed `Err`, the
/// money path would claim someone else's success and ack a write whose own
/// flush failed — the exact lie the whole D3 apparatus exists to prevent.
///
/// The reverse mis-attribution (claiming someone else's `Err`) is the safe
/// direction: it fails an ack that may have been durable, which rule 11 already
/// prefers over acking one that was not.
#[test]
fn an_unclaimed_flush_failure_is_not_erased_by_a_later_successful_write() {
    let dir = scratch_dir("fail-closed");
    let db = dir.join("aberp.duckdb");
    let handle = Handle::open_default(&db, tenant()).expect("open shared Handle");

    // Write #1 with the flush's reach broken -> parks an Err that nobody claims.
    commit_a_row(&handle);
    let restore = std::fs::read(&db).expect("snapshot the db bytes");
    std::fs::remove_file(&db).expect("break the reach");
    commit_a_row(&handle); // guard drop flushes -> Err parked

    // Repair the reach and let a LATER write succeed, without claiming.
    std::fs::write(&db, &restore).expect("restore the db path");
    commit_a_row(&handle); // guard drop flushes -> Ok, must NOT erase the Err

    let claimed = handle.durable_ack();
    assert!(
        claimed.is_err(),
        "ADR-0110 D3 REGRESSION: an unclaimed flush FAILURE was erased by a \
         later successful write. A money path claiming here would be told its \
         write is durable on the strength of somebody else's fsync."
    );

    // …and once claimed, the failure is consumed: the next ack reflects the
    // current state rather than latching forever.
    handle
        .durable_ack()
        .expect("a claimed failure must not latch — the next ack flushes afresh");
}
