//! D-21 R1 (ADR-0119) — the whole-DB cross-process advisory lock.
//!
//! ## What it closes
//!
//! The audit ledger's single-writer guarantee is in-process only: the shared
//! `aberp_db::Handle` writer mutex (and `AUDIT_APPEND_LOCK`) serialise every
//! writer INSIDE `aberp serve`. A separate `aberp <subcommand>` process —
//! `drain-submission-queue`, `retry-submission`, `drain-pending-retries`,
//! `submit-invoice`, the `*-annulment` tools — shares none of those locks, so
//! if it runs while `serve` holds the DB, the two can both read the same audit
//! chain head and append `seq = head + 1`: the fork class (seq-369/416/428/515)
//! this whole line of work exists to prevent. ADR-0099 §R2 closed the MIRROR
//! half (its `sync_mirror_lockstep` flock is genuinely cross-process); the
//! TABLE half was backstopped by hash-chain DETECTION alone.
//!
//! This module is the prevention: an `fs2` advisory file lock (flock on
//! Linux/macOS, `LockFileEx` on Windows — the same primitive
//! `submission_lock.rs` and the mirror writer use) taken **exclusive** on a
//! per-tenant lock file next to the tenant DB. `aberp serve` holds it for its
//! whole lifetime; an audit-writing CLI takes it first and REFUSES if `serve`
//! holds it, rather than racing the audit table.
//!
//! ## The held lock IS the liveness signal
//!
//! There is deliberately no pidfile. `flock`/`LockFileEx` is released by the
//! kernel when the holding process's fd closes — on clean exit, on crash, and
//! on `kill -9`/OOM alike — so a CLI that acquires the lock freely KNOWS no
//! `serve` is live on this tenant, with no stale-file reaping and no TOCTOU. A
//! *hung* (not dead) `serve` is the one residual: the CLI's refusal names the
//! lock path so an operator can `lsof` it, exactly as `MirrorLockTimeout` does.
//!
//! ## Scope
//!
//! - **Exclusive, whole-DB, per-tenant.** One `aberp serve` and the
//!   audit-writing CLIs are mutually exclusive on a tenant; two CLIs are too
//!   (the second refuses), which also closes the CLI-vs-CLI table fork. As a
//!   side effect it subsumes the tracked-but-unbuilt S386 single-`serve`
//!   guard: two serves cannot both hold the exclusive lock.
//! - **Read-only CLIs are NOT gated here** — `verify-chain`, `entries`, the
//!   list/query subcommands never append, so they may run against a live
//!   `serve`. Only the callers that WRITE the audit ledger take this lock.
//! - Independent of the per-invoice [`crate::submission_lock`] (kept as the
//!   in-process + serve-down CLI-vs-CLI backstop) and of the DB-row restore
//!   lock (a different concern).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;

/// RAII guard holding the exclusive whole-DB advisory lock for one tenant DB.
/// The lock releases when this drops (the file handle closes; flock releases on
/// close — and on process death). The lock FILE is intentionally NOT unlinked
/// on drop: unlinking races another process opening it, and an empty leftover
/// file per tenant is negligible (same posture as `submission_lock.rs`).
#[derive(Debug)]
#[must_use = "dropping the guard immediately releases the whole-DB lock"]
pub struct DbLockGuard {
    _file: std::fs::File,
    path: PathBuf,
}

impl DbLockGuard {
    /// The on-disk lock-file path (for diagnostics / the CLI refusal message).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The lock-file path for a tenant DB: `.aberp-db.lock` next to the DB file.
/// Two processes on the same tenant share `db_path` (one DuckDB file per
/// tenant, ADR-0002), so a sibling lock file is their rendezvous point. The
/// name is fixed (whole-DB, not keyed by a sub-resource), so there is nothing
/// operator-supplied to sanitise here.
///
/// A bare relative db path (`aberp.duckdb`, whose `.parent()` is `""`) resolves
/// against the current directory — the same directory two processes with that
/// same bare path would share — rather than being rejected. Rejecting it would
/// break serve boot for a legitimately-relative `--db`.
fn lock_path_for(db_path: &Path) -> PathBuf {
    let parent = db_path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(dir) => dir.join(".aberp-db.lock"),
        None => PathBuf::from(".aberp-db.lock"),
    }
}

/// Try to acquire the exclusive whole-DB lock, non-blocking.
///
/// - `Ok(Some(guard))` — acquired; hold it for the command's whole run.
/// - `Ok(None)` — another process (an `aberp serve`, or another audit-writing
///   CLI) holds it. Returns immediately; the caller REFUSES.
/// - `Err(_)` — the lock file could not be opened. Loud-fail rather than
///   silently skip the lock (CLAUDE.md #12): a missing lock silently re-opens
///   the cross-process fork window.
pub fn try_acquire(db_path: &Path) -> Result<Option<DbLockGuard>> {
    let path = lock_path_for(db_path);
    let file = OpenOptions::new()
        .create(true)
        // The lock file is a pure flock handle — its CONTENTS are never read or
        // written, so never truncate an existing one (and never race-clobber a
        // peer's handle). Open-or-create, leave bytes alone.
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open whole-DB lock file {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(DbLockGuard { _file: file, path })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(anyhow!(
            "acquire exclusive whole-DB lock {}: {e}",
            path.display()
        )),
    }
}

/// Acquire the exclusive whole-DB lock, blocking up to `timeout`, for
/// `aberp serve` boot. Serve OWNS this lock for its lifetime; a short CLI
/// one-shot clears well within the timeout, so a boot that times out means a
/// long-running CLI (or a second serve) is holding the DB — which serve must
/// NOT boot alongside. On timeout it fails loud (naming the lock path for an
/// `lsof`) rather than boot unsynchronised.
///
/// `fs2` exposes no timed acquire, so this is a `try_lock` spin at 50 ms — the
/// same bounded-wait shape ADR-0099 R3.5 uses for the mirror flock.
pub fn acquire_for_serve_boot(db_path: &Path, timeout: std::time::Duration) -> Result<DbLockGuard> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match try_acquire(db_path)? {
            Some(guard) => return Ok(guard),
            None => {
                if std::time::Instant::now() >= deadline {
                    let path = lock_path_for(db_path);
                    return Err(anyhow!(
                        "aberp serve could not acquire the whole-DB lock {} within {:?} — another \
                         process (a second `aberp serve`, or an `aberp` audit-writing subcommand \
                         such as drain-submission-queue / retry-submission) holds this tenant's \
                         database. Stop it (or `lsof {}` to find it) and retry. Serve will not \
                         boot alongside another writer, to keep the audit chain single-writer \
                         (ADR-0119 R1).",
                        path.display(),
                        timeout,
                        path.display(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// Acquire the whole-DB lock for an audit-writing CLI one-shot, or REFUSE with
/// a clear, actionable error if `aberp serve` (or another audit-writing CLI)
/// holds it. Hold the returned guard for the command's whole run — it releases
/// on drop. `command` is the subcommand name for the message (e.g.
/// `"drain-submission-queue"`).
pub fn acquire_or_refuse(db_path: &Path, command: &str) -> Result<DbLockGuard> {
    match try_acquire(db_path)? {
        Some(guard) => Ok(guard),
        None => {
            let path = lock_path_for(db_path);
            Err(anyhow!(
                "refusing `aberp {command}`: another process holds this tenant's database lock \
                 ({}) — an `aberp serve` (the desktop app) is almost certainly running. This \
                 command writes the audit ledger, and running it alongside serve could fork the \
                 tamper-evident chain (ADR-0119 R1), so it is refused. Quit the desktop app (or \
                 stop `aberp serve`) and re-run. Normal submission and NAV-ack happen inside \
                 serve automatically; use this only for manual recovery while serve is down. \
                 (`lsof {}` shows the holder.)",
                path.display(),
                path.display(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_db(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aberp-d21-dblock-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("tenant.duckdb")
    }

    /// The headline R1 invariant: while one holder (serve) has the lock, a
    /// second acquirer (a CLI) is told it is contended (`None`) rather than
    /// racing it. Releasing lets the next acquire succeed.
    #[test]
    fn only_one_holder_at_a_time() {
        let db = scratch_db("excl");
        let serve = try_acquire(&db)
            .expect("first acquire ok")
            .expect("first acquire must get the lock");
        let cli = try_acquire(&db).expect("second acquire ok");
        assert!(
            cli.is_none(),
            "a CLI must NOT acquire the whole-DB lock while serve holds it — it refuses instead"
        );
        drop(serve);
        let after = try_acquire(&db)
            .expect("third acquire ok")
            .expect("after release the lock is free again");
        drop(after);
    }

    /// Different tenants never contend (the lock is per-tenant-DB, keyed by the
    /// DB's parent dir).
    #[test]
    fn distinct_tenants_do_not_contend() {
        let db_a = scratch_db("tenant-a");
        let db_b = scratch_db("tenant-b");
        let a = try_acquire(&db_a).unwrap().expect("tenant-a acquires");
        let b = try_acquire(&db_b)
            .unwrap()
            .expect("tenant-b acquires independently");
        drop(a);
        drop(b);
    }

    /// The serve-boot blocking acquire fails loud (not hang) when a CLI already
    /// holds the lock, and its message names the lock path for an `lsof`.
    #[test]
    fn serve_boot_acquire_times_out_loud_when_held() {
        let db = scratch_db("boot-contended");
        let held = try_acquire(&db).unwrap().expect("CLI holds the lock");
        let err = acquire_for_serve_boot(&db, std::time::Duration::from_millis(150))
            .expect_err("serve boot must refuse, not hang, when the DB is held");
        let msg = err.to_string();
        assert!(
            msg.contains(".aberp-db.lock"),
            "must name the lock path: {msg}"
        );
        assert!(
            msg.contains("lsof"),
            "must tell the operator how to find the holder: {msg}"
        );
        drop(held);
        // Once free, serve boot acquires immediately.
        let g = acquire_for_serve_boot(&db, std::time::Duration::from_millis(150))
            .expect("after release serve boot acquires");
        drop(g);
    }

    /// `acquire_or_refuse` returns the actionable refusal (naming the command,
    /// the fork risk, and `lsof`) when the lock is already held, and succeeds
    /// when free.
    #[test]
    fn acquire_or_refuse_refuses_when_held_succeeds_when_free() {
        let db = scratch_db("refuse");
        let held = try_acquire(&db).unwrap().expect("serve holds the lock");
        let err =
            acquire_or_refuse(&db, "drain-submission-queue").expect_err("must refuse while held");
        let msg = err.to_string();
        assert!(
            msg.contains("drain-submission-queue"),
            "names the command: {msg}"
        );
        assert!(msg.contains("fork"), "explains the fork risk: {msg}");
        assert!(
            msg.contains("lsof"),
            "tells the operator how to find the holder: {msg}"
        );
        drop(held);
        let g =
            acquire_or_refuse(&db, "drain-submission-queue").expect("succeeds once serve releases");
        drop(g);
    }

    /// The lock frees on guard drop (the within-process proxy for
    /// process-death-release; a true kill-9 test needs a subprocess).
    #[test]
    fn dropping_the_guard_frees_the_lock() {
        let db = scratch_db("drop");
        {
            let _g = try_acquire(&db).unwrap().expect("acquire");
            assert!(
                try_acquire(&db).unwrap().is_none(),
                "held while guard alive"
            );
        }
        assert!(
            try_acquire(&db).unwrap().is_some(),
            "freed once the guard dropped"
        );
    }
}
