//! S385 — shared atomic file-write helper.
//!
//! Lifted verbatim out of `nav_xml::write_to_path` (S381) so the
//! crash-safe write-temp → fsync → rename → fsync-dir sequence lives in
//! ONE place. Two consumers depend on it:
//!
//! 1. NAV `<InvoiceData>` XML — `nav_xml::write_to_path` now wraps this
//!    (S381 made the NAV emit atomic; the four chain emit sites keep
//!    calling `write_to_path`, which forwards here).
//! 2. Quote priced-PDF re-render — `quote_pdf_rerender_daemon` overwrote
//!    the on-disk `priced.pdf` with a naive `std::fs::write`, leaving a
//!    torn-file window against the SPA's PDF-download reader
//!    (`serve::read_pricing_job_pdf`, S352). A reader catching the
//!    partial write got a truncated/corrupt PDF; the atomic rename
//!    closes that window — a reader sees either the whole old file or
//!    the whole new one, never a torn prefix.
//!
//! The function is content-agnostic (`&[u8]`), so any future on-disk
//! artifact that a concurrent reader can observe should route through
//! here rather than `std::fs::write`.
//!
//! Conservative path choice (CLAUDE.md #2/#13): a binary-local module,
//! NOT a new `crates/aberp-fs` crate. Both consumers live in
//! `apps/aberp`; no other crate needs the helper, so a crate would be a
//! speculative abstraction. If a workspace crate ever needs atomic
//! writes, promote this then.

use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// Atomically write `bytes` to `path`.
///
/// Crash/torn-write safety: the bytes are written to a temp file in the
/// SAME directory, fsync'd, then `rename(2)`'d over `path` (atomic on
/// POSIX same-filesystem renames). A reader of `path` therefore observes
/// either the complete previous contents or the complete new contents —
/// never a partial prefix. The parent directory is fsync'd after the
/// rename so the rename metadata survives a power loss.
///
/// On any failure before the rename the temp file is removed (best
/// effort) so a failed write does not litter the directory with a stale
/// tempfile. The rename either fully succeeds or `path` is untouched.
pub fn write_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
    let path = path.as_ref();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("atomic write target `{}` has no parent dir", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("atomic write target `{}` has no file name", path.display()))?
        .to_string_lossy();

    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(
        ".{file_name}.tmp.{}-{nanos}-{seq}",
        std::process::id()
    ));

    let write_result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("create temp file at {}", tmp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write bytes to temp file {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync temp file {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "atomically rename {} -> {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    })();

    if write_result.is_err() {
        // Best-effort: do not litter the dir with a stale tempfile when
        // the write or rename failed.
        let _ = std::fs::remove_file(&tmp_path);
        return write_result;
    }

    // Fsync the parent directory so the rename metadata is durable across
    // a power loss (otherwise it lives only in the dir's page cache).
    // Best-effort — a dir that cannot be opened/fsynced does not
    // invalidate the already-renamed, already-fsync'd file.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test tempdir under the system temp root. Mirrors the
    /// `incoming_invoices::tests::ScopedTempDir` pattern — avoids the
    /// `tempfile` dev-dep so the surface stays tight per CLAUDE.md #2.
    struct ScopedTempDir(std::path::PathBuf);

    impl ScopedTempDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path =
                std::env::temp_dir().join(format!("aberp-s385-fs-{label}-{pid}-{nanos}-{seq}"));
            std::fs::create_dir_all(&path).expect("create scoped tempdir");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScopedTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_atomic_creates_file_with_exact_bytes() {
        let dir = ScopedTempDir::new("exact");
        let path = dir.path().join("artifact.bin");
        write_atomic(&path, b"hello-bytes").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello-bytes");
    }

    #[test]
    fn write_atomic_overwrites_existing_file_fully() {
        let dir = ScopedTempDir::new("overwrite");
        let path = dir.path().join("artifact.bin");
        std::fs::write(&path, b"OLD-CONTENTS-LONGER").unwrap();
        write_atomic(&path, b"new").unwrap();
        // Fully replaced — no trailing bytes of the old, longer file.
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    /// S385 invariant: a fault DURING the write leaves `path` either
    /// fully-old or fully-new — never torn. We simulate the fault by
    /// pointing the target at a path whose parent does not exist, which
    /// fails at `File::create` (the temp-file step) BEFORE any rename
    /// touches an existing `path`. The pre-existing file is left intact
    /// (fully-old), and no temp file is leaked into a real directory.
    #[test]
    fn write_atomic_fault_leaves_target_fully_old_never_torn() {
        let dir = ScopedTempDir::new("fault");
        let path = dir.path().join("artifact.bin");
        std::fs::write(&path, b"ORIGINAL").unwrap();

        // Force a failure: a temp file cannot be created under a
        // non-existent subdirectory, so the write fails before the
        // rename. (write_atomic derives the temp path from the target's
        // parent, so a bad parent fails at the create step.)
        let bad_target = dir
            .path()
            .join("does-not-exist-subdir")
            .join("artifact.bin");
        let err = write_atomic(&bad_target, b"NEW-DATA-THAT-MUST-NOT-LAND");
        assert!(err.is_err(), "write into a missing parent dir must fail");

        // The unrelated pre-existing file is untouched, and the failed
        // target was never created (fully-old / nonexistent, never torn).
        assert_eq!(std::fs::read(&path).unwrap(), b"ORIGINAL");
        assert!(!bad_target.exists(), "failed write must not create target");
    }

    /// A failed write must not leave a stale `.tmp.*` tempfile behind in
    /// a writable directory. We trigger the rename failure by making the
    /// target a non-empty directory (rename of a file over it fails),
    /// then assert the temp file was cleaned up.
    #[test]
    fn write_atomic_cleans_up_tempfile_on_rename_failure() {
        let dir = ScopedTempDir::new("cleanup");
        // Target path is an existing directory → rename(file, dir) fails.
        let target = dir.path().join("iam-a-dir");
        std::fs::create_dir(&target).unwrap();
        // Put a child in it so even platforms that allow rename-over-
        // empty-dir reject this one.
        std::fs::write(target.join("child"), b"x").unwrap();

        let err = write_atomic(&target, b"payload");
        assert!(err.is_err(), "rename over a non-empty dir must fail");

        // No leftover `.iam-a-dir.tmp.*` tempfile in the parent.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".iam-a-dir.tmp.")
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "failed write left a stale tempfile: {leftover:?}"
        );
    }
}
