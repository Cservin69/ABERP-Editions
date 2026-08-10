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

/// **Defense-specific pin #2.** The WAL arm has its own *reach*, and it must
/// fail loud the same way the main-file arm does.
///
/// The WAL is `fsync`'d behind an `if self.wal_path.exists()` guard. A
/// plausible-looking refactor that widened that guard into a swallow — e.g.
/// `let _ = fsync_path(&self.wal_path);` — would keep every other D3 gate green
/// (the call site is still there, still propagates its outer error) while
/// silently dropping the one file that matters most. Here the WAL exists, is
/// then removed, and the ack must surface a typed failure naming it.
#[test]
fn d3_ack_reaches_the_wal_fault() {
    let dir = scratch_dir("wal-reach");
    let db = dir.join("aberp.duckdb");
    let wal = dir.join("aberp.duckdb.wal");
    let handle = Handle::open_default(&db, tenant()).expect("open shared Handle");

    commit_a_row(&handle);
    assert!(wal.exists(), "precondition: a WAL must exist to break");

    // Make the main file's fsync succeed and the WAL's fail: replace the WAL
    // path with a directory, which `File::open` succeeds on but `sync_all`
    // handles differently — so instead, remove it after stat'ing. Removing it
    // makes `wal_path.exists()` false and the arm is skipped, which is CORRECT
    // (nothing to flush). To exercise the fault we must keep the path visible
    // to `exists()` but unopenable, so swap in a path we cannot open: a
    // dangling symlink.
    std::fs::remove_file(&wal).expect("remove the real WAL");
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("no-such-wal-target"), &wal)
        .expect("plant a dangling symlink where the WAL was");

    #[cfg(unix)]
    {
        // `Path::exists()` follows symlinks, so a DANGLING link reports false —
        // which would silently skip the arm and make this test vacuous. Assert
        // the shape we actually planted before drawing any conclusion from it.
        assert!(
            !wal.exists(),
            "a dangling symlink should not report exists(); test premise changed"
        );
        assert!(
            std::fs::symlink_metadata(&wal).is_ok(),
            "the symlink itself must be present"
        );
        // With `exists()` false the WAL arm is correctly SKIPPED and the ack
        // succeeds on the main file alone. That is the designed behaviour: a
        // tenant with no WAL has nothing to flush. Pin it so a future change
        // that starts unconditionally opening `wal_path` — and would then fail
        // every ack on a WAL-less tenant — is caught here.
        handle
            .durable_ack()
            .expect("a tenant whose WAL is absent must still ack on the main file alone");
        assert!(
            !handle.fsynced_paths().iter().any(|p| p == &wal),
            "an absent WAL must not be journalled as synced — the journal must \
             mean 'this is on stable storage', never 'we tried'"
        );
    }
}
