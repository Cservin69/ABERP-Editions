//! S390/E — cross-process per-invoice NAV-submission lock.
//!
//! S378 added an in-process `tokio::Mutex` (`serve::submission_gate`)
//! that serialises concurrent submissions of one invoice WITHIN the
//! `aberp serve` process (the auto-submit-on-issue background task vs. a
//! manual Submit click — the invoice-0047 double-submit). That gate is
//! process-local: it cannot see a SEPARATE process. But ABERP's NAV
//! re-submission paths are separate CLI binaries the operator can run
//! while `aberp serve` (the desktop app) is live:
//!
//!   - `aberp submit-invoice`        (submit_invoice::submit_from_inputs)
//!   - `aberp drain-submission-queue`(drain_submission_queue)
//!   - `aberp drain-pending-retries` (drain_pending_retries)
//!   - `aberp retry-submission`      (retry_submission)
//!
//! A row a drain picks up could collide with a concurrent operator click
//! in serve — two processes both POST `manageInvoice` for the same
//! invoice (NAV `INVOICE_NUMBER_NOT_UNIQUE`). The in-process gate is
//! blind to that.
//!
//! This module is the cross-process counterpart: an `fs2` advisory file
//! lock (flock on Linux/macOS, LockFileEx on Windows — the SAME
//! primitive the audit-ledger mirror writer uses) keyed per
//! `(tenant, invoice_id)`. Every NAV-submit entry point acquires it
//! before constructing the wire request and holds it across the POST.
//! `try_acquire` is NON-blocking: a held lock returns `Ok(None)` so the
//! caller can refuse (serve → 409) or skip (a drain → next row) instead
//! of stalling.
//!
//! It does NOT replace the in-process gate: that gate stays the fast
//! same-process path (no fs syscalls), and this lock layers cross-process
//! exclusion on top. The two together close both the in-process race
//! (S378) and the cross-process race (this).
//!
//! Scope note (conservative): this is the per-invoice submission lock
//! only — NOT the broader single-`serve`-instance flock/pidfile guard
//! (S386, tracked separately in `docs/findings/s382-...`). A second
//! `aberp serve` on the same tenant is still out of scope; what this
//! closes is the CLI-vs-serve (and CLI-vs-CLI) per-invoice submit
//! collision.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;

/// RAII guard holding the exclusive advisory lock for one
/// `(tenant, invoice_id)`. The lock is released when this is dropped
/// (the underlying file handle closes, and flock releases on close).
/// The lock FILE itself is intentionally NOT deleted on drop — unlinking
/// a lock file races with another process opening it; an empty leftover
/// file per submitted invoice is negligible (same posture as the
/// in-process gate's per-invoice map entry).
#[must_use = "dropping the guard immediately releases the submission lock"]
pub struct SubmissionLockGuard {
    _file: std::fs::File,
    path: PathBuf,
}

impl SubmissionLockGuard {
    /// The on-disk lock-file path (for diagnostics/logging).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Derive the lock-file path for a `(tenant, invoice_id)` next to the
/// tenant DB. Two processes operating on the same tenant share `db_path`
/// (one DuckDB file per tenant), so a lock file in its parent dir keyed
/// by tenant + invoice is the cross-process rendezvous point. Both
/// components are sanitised (non `[A-Za-z0-9._-]` → `_`) so an exotic
/// tenant string can never escape the directory or collide via path
/// separators.
fn lock_path_for(db_path: &Path, tenant: &str, invoice_id: &str) -> Result<PathBuf> {
    let parent = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "tenant db path `{}` has no parent dir for the submission lock",
                db_path.display()
            )
        })?;
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    Ok(parent.join(format!(
        ".aberp-submission-lock.{}.{}",
        sanitize(tenant),
        sanitize(invoice_id)
    )))
}

/// Try to acquire the exclusive cross-process submission lock for
/// `(tenant, invoice_id)`.
///
/// - `Ok(Some(guard))` — acquired; hold the guard across the NAV POST.
/// - `Ok(None)` — another process holds it (a submission for this exact
///   invoice is in progress elsewhere). Non-blocking: returns
///   immediately.
/// - `Err(_)` — the lock file could not be opened (fs error). Loud-fail
///   rather than silently skipping the lock (CLAUDE.md #12) — a missing
///   lock would silently re-open the cross-process double-submit window.
pub fn try_acquire(
    db_path: &Path,
    tenant: &str,
    invoice_id: &str,
) -> Result<Option<SubmissionLockGuard>> {
    let path = lock_path_for(db_path, tenant, invoice_id)?;
    let file = OpenOptions::new()
        .create(true)
        // The lock file is a pure flock handle — its CONTENTS are never
        // read or written, so do not truncate an existing one (and never
        // race-clobber a peer's handle). `truncate(false)` is the
        // intent: open-or-create, leave bytes alone.
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open submission lock file {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(SubmissionLockGuard { _file: file, path })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(anyhow!(
            "acquire exclusive submission lock {}: {e}",
            path.display()
        )),
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
            "aberp-s390-lock-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("tenant.duckdb")
    }

    /// The headline E invariant: two concurrent submitters of the SAME
    /// invoice — only ONE acquires the lock; the other is told it is in
    /// progress (`None`). Models "parallel drain + manual submit on the
    /// same invoice_id → only one POSTs". The guards use the same lock
    /// path scheme every NAV-submit entry point uses, so this exclusion
    /// holds across processes too (flock is process-spanning).
    #[test]
    fn only_one_holder_per_invoice_at_a_time() {
        let db = scratch_db("excl");
        let first = try_acquire(&db, "tenant-a", "inv_0047")
            .expect("first acquire ok")
            .expect("first acquire must get the lock");
        // Second attempt while the first guard is alive — contended.
        let second = try_acquire(&db, "tenant-a", "inv_0047").expect("second acquire ok");
        assert!(
            second.is_none(),
            "a second concurrent submit of the same invoice must NOT acquire the lock"
        );
        // Releasing the first lets the next acquire succeed.
        drop(first);
        let third = try_acquire(&db, "tenant-a", "inv_0047")
            .expect("third acquire ok")
            .expect("after release the lock is free again");
        drop(third);
    }

    /// Different invoices never contend (the lock is per-invoice, like
    /// the S378 in-process gate's tuple key).
    #[test]
    fn distinct_invoices_do_not_contend() {
        let db = scratch_db("distinct");
        let a = try_acquire(&db, "tenant-a", "inv_0047")
            .unwrap()
            .expect("inv_0047 acquires");
        let b = try_acquire(&db, "tenant-a", "inv_0048")
            .unwrap()
            .expect("inv_0048 acquires independently");
        drop(a);
        drop(b);
    }

    /// Different tenants never contend even on the same invoice-id
    /// string (tuple key, no aliasing).
    #[test]
    fn distinct_tenants_do_not_contend() {
        let db_a = scratch_db("tenant-split-a");
        let db_b = scratch_db("tenant-split-b");
        let a = try_acquire(&db_a, "tenant-a", "inv_0047")
            .unwrap()
            .expect("tenant-a acquires");
        let b = try_acquire(&db_b, "tenant-b", "inv_0047")
            .unwrap()
            .expect("tenant-b acquires independently");
        drop(a);
        drop(b);
    }

    /// Sanitisation keeps an exotic tenant string inside the parent dir
    /// (no path-separator escape).
    #[test]
    fn lock_path_sanitises_tenant_and_invoice() {
        let db = Path::new("/tmp/aberp/x/tenant.duckdb");
        let p = lock_path_for(db, "ten/../ant", "inv_/etc/passwd").unwrap();
        assert_eq!(p.parent().unwrap(), Path::new("/tmp/aberp/x"));
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains('/'),
            "sanitised name must not contain '/': {name}"
        );
    }
}
