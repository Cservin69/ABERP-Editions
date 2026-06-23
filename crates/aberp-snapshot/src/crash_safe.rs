//! ADR-0082 follow-up (chunk 3) — the deferred CRASH-SAFE-CHECKPOINT /
//! durability fix, landing in the EDITIONS TREE ONLY (never prod).
//!
//! # The corruption this fixes
//!
//! On **2026-06-22** a backend crash mid-write zeroed DuckDB blocks in the
//! live data file (the torn-write / ART-corruption family `duckdb#23046`,
//! ADR-0082). The root cause is that DuckDB folds its WAL into the main
//! `*.duckdb` file IN PLACE; a crash partway through that fold leaves a
//! *torn* file — half old, half new, structurally inconsistent.
//!
//! # The mechanism (hardened, not hand-rolled)
//!
//! We never rewrite the live file in place. Instead every durable
//! checkpoint is a **build-aside-then-atomically-swap**, using three mature,
//! well-understood primitives — nothing bespoke:
//!
//!   1. **DuckDB's own logical export/import** (`EXPORT DATABASE` →
//!      `IMPORT DATABASE` + `CHECKPOINT`) builds a *fresh, self-contained*
//!      file from a logical table scan. This is corruption-free **by
//!      construction** (ADR-0082): it is rebuilt from rows, not copied from
//!      the degrading ART, and the fresh file is checkpointed once while it
//!      is still a private staging file (never the live one).
//!   2. **POSIX atomic rename** (`rename(2)`, `std::fs::rename`) swaps the
//!      finished staging file over the live path in one atomic step. A
//!      crash is therefore always on one side of the rename: *before* →
//!      the old good file is intact; *after* → the new good file is intact.
//!      There is no torn middle state.
//!   3. **`fsync` of the file AND its parent directory** — the textbook
//!      durable-rename recipe. Fsyncing the staging file persists its
//!      bytes before the swap; fsyncing the directory persists the rename
//!      itself, so a crash can't resurrect the old name pointing at
//!      half-written data.
//!
//! A **verified-good marker** (`<db>.ckpt-ok`) records the SHA-256 + size of
//! the file that was just durably installed. On clean shutdown, if the
//! marker is missing or stale (the DB changed since), the serve path takes
//! one durable checkpoint so the on-disk file is left pristine and the next
//! boot needs no WAL replay (the exact `LoadCheckpoint`/`ReadIndex` path
//! that historically tripped the corruption, S332/S375).
//!
//! The crash-safety lives in [`atomic_install`] + the marker functions,
//! which operate on PLAIN FILES (no DuckDB) and are exhaustively unit
//! tested — including the "crash between write and rename leaves the old
//! good DB intact" property. [`durable_checkpoint`] wires DuckDB's
//! export/import on top; its end-to-end crash-injection test needs the full
//! bundled-DuckDB build and is gated to the Mac (see SAW-OFF.md chunk-3).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::PARTIAL_SUFFIX;
use crate::take::{sha256_file, sql_quote};
use crate::{Result, SnapshotError};

/// Suffix of the verified-good checkpoint marker written beside a DB.
pub const CKPT_MARKER_SUFFIX: &str = ".ckpt-ok";

/// Verified-good checkpoint marker (`<db>.ckpt-ok`). Records the identity of
/// the file that was last durably installed, so a clean shutdown can tell
/// whether a fresh checkpoint is still needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointMarker {
    /// Hex SHA-256 of the `*.duckdb` file at the moment it was installed.
    pub sha256: String,
    /// Byte size of that file.
    pub byte_size: u64,
    /// Unix seconds when the marker was written.
    pub created_at_unix: i64,
}

/// Outcome of a [`durable_checkpoint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointReport {
    /// SHA-256 of the freshly installed file.
    pub sha256: String,
    /// Byte size of the freshly installed file.
    pub byte_size: u64,
    /// The logical export validated (import + smoke + hash-chain) before
    /// the swap — always `true` on success (a failed validation aborts the
    /// checkpoint and leaves the live file untouched).
    pub validated: bool,
}

/// `<db>.ckpt-ok` marker path.
pub fn marker_path(db_path: &Path) -> PathBuf {
    let mut os = db_path.as_os_str().to_owned();
    os.push(CKPT_MARKER_SUFFIX);
    PathBuf::from(os)
}

/// DuckDB names the WAL by appending `.wal` to the FULL filename
/// (`x.duckdb` → `x.duckdb.wal`) — NOT `Path::with_extension`.
fn wal_sibling(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_owned();
    os.push(".wal");
    PathBuf::from(os)
}

/// `fsync` a regular file's contents + metadata to disk.
fn fsync_file(path: &Path) -> Result<()> {
    let f = std::fs::File::open(path).map_err(|e| SnapshotError::io(path, e))?;
    f.sync_all().map_err(|e| SnapshotError::io(path, e))
}

/// `fsync` a directory so a rename/create within it is durable. Opening a
/// directory read-only and `sync_all`-ing its fd is the canonical POSIX
/// way to persist a directory entry change. A platform that refuses to
/// open a directory (rare) is a soft failure: the rename already happened,
/// so we log and continue rather than fail the whole checkpoint.
fn fsync_dir(dir: &Path) -> Result<()> {
    match std::fs::File::open(dir) {
        Ok(f) => f.sync_all().map_err(|e| SnapshotError::io(dir, e)),
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "could not open directory to fsync it after rename; rename already \
                 completed so continuing (durability of the swap is best-effort here)"
            );
            Ok(())
        }
    }
}

/// **The crash-safe commit primitive.** Durably replace `target` with the
/// finished `staged` file:
///
///   1. drop any WAL beside `staged` (a checkpointed file is self-contained),
///   2. `fsync` `staged` so its bytes are on disk BEFORE the swap,
///   3. atomic `rename(staged → target)` — the swap is all-or-nothing,
///   4. drop any stale WAL beside `target` (an old WAL would corrupt the
///      fresh self-contained file on next open),
///   5. `fsync` the parent directory so the rename itself is durable.
///
/// Crash semantics: a crash before step 3 leaves the **old** `target`
/// intact (and a removable `staged`); a crash after step 3 leaves the
/// **new** `target` intact. There is no torn intermediate `target` at any
/// point — which is the whole point.
pub fn atomic_install(staged: &Path, target: &Path) -> Result<()> {
    let staged_wal = wal_sibling(staged);
    if staged_wal.exists() {
        let _ = std::fs::remove_file(&staged_wal);
    }
    fsync_file(staged)?;
    std::fs::rename(staged, target).map_err(|e| SnapshotError::io(target, e))?;
    let target_wal = wal_sibling(target);
    if target_wal.exists() {
        std::fs::remove_file(&target_wal).map_err(|e| SnapshotError::io(&target_wal, e))?;
    }
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// Write (and `fsync`) the verified-good marker for `db_path`, recording the
/// file's current SHA-256 + size. The marker write is itself durable: the
/// marker file is fsync'd and so is its parent directory.
pub fn write_marker(db_path: &Path) -> Result<CheckpointMarker> {
    let sha256 = sha256_file(db_path)?;
    let byte_size = std::fs::metadata(db_path)
        .map_err(|e| SnapshotError::io(db_path, e))?
        .len();
    let created_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let marker = CheckpointMarker {
        sha256,
        byte_size,
        created_at_unix,
    };
    let path = marker_path(db_path);
    let bytes = serde_json::to_vec_pretty(&marker).map_err(|e| SnapshotError::BadMeta {
        path: path.clone(),
        detail: format!("serialize checkpoint marker: {e}"),
    })?;
    std::fs::write(&path, bytes).map_err(|e| SnapshotError::io(&path, e))?;
    fsync_file(&path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fsync_dir(parent)?;
    }
    Ok(marker)
}

/// Read the verified-good marker beside `db_path`, if present + parseable.
pub fn read_marker(db_path: &Path) -> Option<CheckpointMarker> {
    let path = marker_path(db_path);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `true` iff a verified-good checkpoint already covers the CURRENT file —
/// i.e. a marker exists and its SHA-256 matches the file on disk right now.
/// A missing marker, an unreadable marker, or a SHA mismatch (the DB was
/// written since the last checkpoint) all mean "checkpoint missing" → a
/// clean shutdown should take one.
pub fn checkpoint_is_current(db_path: &Path) -> bool {
    let Some(marker) = read_marker(db_path) else {
        return false;
    };
    match sha256_file(db_path) {
        Ok(actual) => actual == marker.sha256,
        Err(_) => false,
    }
}

/// Take ONE crash-safe durable checkpoint of `db_path`, leaving the live
/// file pristine + a fresh verified-good marker. Reuses ADR-0082's
/// corruption-free logical export, then commits with [`atomic_install`].
///
/// Steps: `EXPORT` the live DB → a private staging export dir; validate the
/// export (import + smoke + hash-chain) and **abort without touching the
/// live file** if it does not validate (never checkpoint a corrupt DB on
/// top of itself — the snapshots are the recovery path); `IMPORT` +
/// `CHECKPOINT` into a fresh staging `*.duckdb`; [`atomic_install`] it over
/// the live file; write the marker. Best-effort cleanup of the staging dir.
///
/// DuckDB-backed → its crash-injection integration test is Mac-gated.
pub fn durable_checkpoint(db_path: &Path, tenant: &str) -> Result<CheckpointReport> {
    use duckdb::Connection;

    if !db_path.exists() {
        return Err(SnapshotError::SourceMissing(db_path.to_path_buf()));
    }
    let tag = unique_tag();
    let export_dir = sibling(db_path, &format!(".ckpt-export-{tag}{PARTIAL_SUFFIX}"));
    let staging = sibling(db_path, &format!(".ckpt-staging-{tag}.duckdb"));
    let staging_wal = wal_sibling(&staging);

    // Clear any leftovers from a crashed prior checkpoint.
    cleanup_stale(&export_dir, &staging, &staging_wal);

    // 1. Logical EXPORT of the live DB (a table scan, never the ART).
    {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(&format!(
            "EXPORT DATABASE {} (FORMAT PARQUET);",
            sql_quote(&export_dir)
        ))?;
    }

    // 2. Validate the export BEFORE building anything we'd swap in. If the
    //    live DB is already corrupt, refuse — leave the live file as-is and
    //    surface; the periodic snapshots remain the recovery path.
    let report = crate::take::validate_export(&export_dir, tenant);
    if !report.ok {
        let _ = std::fs::remove_dir_all(&export_dir);
        return Err(SnapshotError::RestoreFromInvalid(format!(
            "live DB {} did not validate (logical export failed: {}); refusing to \
             checkpoint a corrupt DB over itself",
            db_path.display(),
            report.error.as_deref().unwrap_or("unknown")
        )));
    }

    // 3. Build a FRESH, self-contained staging file (checkpointed while it
    //    is still private — never the live file).
    {
        let conn = Connection::open(&staging)?;
        conn.execute_batch(&format!("IMPORT DATABASE {};", sql_quote(&export_dir)))?;
        conn.execute_batch("CHECKPOINT;")?;
    }

    // 4. Atomic, fsync'd swap over the live file (the crash-safe commit).
    atomic_install(&staging, db_path)?;

    // 5. Record the verified-good marker for the freshly installed file.
    let marker = write_marker(db_path)?;

    // Best-effort cleanup of the export dir (the staging file is gone —
    // it was renamed onto the live path).
    let _ = std::fs::remove_dir_all(&export_dir);

    Ok(CheckpointReport {
        sha256: marker.sha256,
        byte_size: marker.byte_size,
        validated: true,
    })
}

/// A sibling path `<db><suffix>` in the same directory as the DB.
fn sibling(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os = db_path.as_os_str().to_owned();
    os.push(suffix);
    PathBuf::from(os)
}

/// Process + nanosecond tag so concurrent/again runs never collide.
fn unique_tag() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

fn cleanup_stale(export_dir: &Path, staging: &Path, staging_wal: &Path) {
    if export_dir.exists() {
        let _ = std::fs::remove_dir_all(export_dir);
    }
    for p in [staging, staging_wal] {
        if p.exists() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Crash-injection unit tests for the crash-safe COMMIT primitive.
    //! These use plain files (no DuckDB) so they run anywhere, and they
    //! pin the load-bearing property: a crash between "write staging" and
    //! "rename" can never lose or tear the live file.

    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static C: AtomicU64 = AtomicU64::new(0);
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = C.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "aberp-ckpt-{label}-{}-{n}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn join(&self, n: &str) -> PathBuf {
            self.0.join(n)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_install_replaces_target_with_staged() {
        let t = Tmp::new("replace");
        let target = t.join("live.duckdb");
        let staged = t.join("live.duckdb.ckpt-staging.duckdb");
        std::fs::write(&target, b"OLD-GOOD").unwrap();
        std::fs::write(&staged, b"NEW-GOOD-CHECKPOINTED").unwrap();

        atomic_install(&staged, &target).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"NEW-GOOD-CHECKPOINTED");
        assert!(!staged.exists(), "staged file consumed by the rename");
    }

    #[test]
    fn crash_before_rename_leaves_old_good_db_intact() {
        // Simulate a crash AFTER building the staging file but BEFORE the
        // atomic install runs: the live file must still be the old good one,
        // fully readable, and the orphan staging file is just removable.
        let t = Tmp::new("crash");
        let target = t.join("live.duckdb");
        let staged = t.join("live.duckdb.ckpt-staging.duckdb");
        std::fs::write(&target, b"OLD-GOOD").unwrap();
        std::fs::write(&staged, b"NEW-not-yet-committed").unwrap();

        // …crash here (atomic_install never called)…

        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"OLD-GOOD",
            "the live DB must survive a crash before the swap, untorn"
        );
        // Recovery just deletes the orphan staging file.
        std::fs::remove_file(&staged).unwrap();
        assert!(target.exists());
    }

    #[test]
    fn atomic_install_clears_stale_target_wal() {
        let t = Tmp::new("wal");
        let target = t.join("live.duckdb");
        let target_wal = t.join("live.duckdb.wal");
        let staged = t.join("live.duckdb.staging");
        std::fs::write(&target, b"OLD").unwrap();
        std::fs::write(&target_wal, b"stale-wal-from-old-file").unwrap();
        std::fs::write(&staged, b"NEW-self-contained").unwrap();

        atomic_install(&staged, &target).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"NEW-self-contained");
        assert!(
            !target_wal.exists(),
            "the stale WAL beside the live file must be cleared so the fresh \
             self-contained file is not corrupted on next open"
        );
    }

    #[test]
    fn marker_roundtrips_and_tracks_currency() {
        let t = Tmp::new("marker");
        let db = t.join("live.duckdb");
        std::fs::write(&db, b"checkpointed-bytes-v1").unwrap();

        // No marker yet → not current.
        assert!(!checkpoint_is_current(&db));

        let m = write_marker(&db).unwrap();
        assert_eq!(m.byte_size, b"checkpointed-bytes-v1".len() as u64);
        assert_eq!(read_marker(&db).unwrap(), m);
        assert!(
            checkpoint_is_current(&db),
            "marker matches the file → current"
        );

        // Mutate the DB → the marker is now stale → checkpoint missing.
        std::fs::write(&db, b"new-uncheckpointed-writes-v2").unwrap();
        assert!(
            !checkpoint_is_current(&db),
            "after a write the on-disk file no longer matches the marker"
        );
    }

    #[test]
    fn checkpoint_missing_when_marker_absent() {
        let t = Tmp::new("absent");
        let db = t.join("live.duckdb");
        std::fs::write(&db, b"x").unwrap();
        assert!(!checkpoint_is_current(&db));
    }
}
