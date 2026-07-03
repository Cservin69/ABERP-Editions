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

/// Suffix of the journaled install-intent written beside a DB while a
/// [`durable_checkpoint`] is mid-swap (ADR-0098 R2, finding B). Its presence at
/// boot means the atomic swap was interrupted and must be RESUMED before any
/// DuckDB open — see [`resume_pending_install`].
pub const INSTALL_INTENT_SUFFIX: &str = ".install-intent";

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

/// Journaled install-intent (`<db>.install-intent`). Written + fsync'd AFTER the
/// staging file is built, fsync'd and validated, and BEFORE the rename, so a
/// crash anywhere in the swap is deterministically recoverable at boot by
/// [`resume_pending_install`] — the WAL handling becomes an explicit journaled
/// protocol instead of relying on either an in-place WAL fold (the `duckdb#23046`
/// locus) or luck (the naive-pragma double-replay window).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallIntent {
    /// The finished, fsync'd staging file to be renamed onto the live path.
    pub staging_path: PathBuf,
    /// Hex SHA-256 of that staging file — the identity checked on resume.
    pub staging_sha256: String,
    /// The live DB path the staging file is being renamed onto.
    pub target_path: PathBuf,
    /// Unix seconds when the intent was journaled.
    pub created_at_unix: i64,
}

/// What [`resume_pending_install`] did for a pending install-intent at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAction {
    /// No install-intent journal beside the DB — nothing to resume.
    NoPendingInstall,
    /// Case (a): staging was still present + matched the journaled SHA, so the
    /// interrupted rename (and stale-WAL delete) was completed.
    CompletedInstall,
    /// Case (b): the rename had already happened (live matched the journaled
    /// identity); the now-stale foreign WAL was deleted and the journal cleared.
    ClearedStaleWal,
}

/// `<db>.ckpt-ok` marker path.
pub fn marker_path(db_path: &Path) -> PathBuf {
    let mut os = db_path.as_os_str().to_owned();
    os.push(CKPT_MARKER_SUFFIX);
    PathBuf::from(os)
}

/// DuckDB names the WAL by appending `.wal` to the FULL filename
/// (`x.duckdb` → `x.duckdb.wal`) — NOT `Path::with_extension`.
pub(crate) fn wal_sibling(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_owned();
    os.push(".wal");
    PathBuf::from(os)
}

/// A cheap fence over the live WAL (presence + byte size), used by
/// [`durable_checkpoint`] to detect a concurrent writer during the EXPORT
/// (ADR-0098 R2 Bug-2 belt; the primary swap-orphan fix is R3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalFence {
    pub(crate) present: bool,
    pub(crate) size: u64,
}

/// Snapshot the live WAL's presence + size. A missing WAL is `{present:false}`.
pub(crate) fn wal_stat(wal: &Path) -> WalFence {
    match std::fs::metadata(wal) {
        Ok(m) => WalFence {
            present: true,
            size: m.len(),
        },
        Err(_) => WalFence {
            present: false,
            size: 0,
        },
    }
}

/// `true` iff the live WAL changed between the EXPORT baseline and the
/// pre-rename re-check in a way that means a concurrent writer's commits would
/// be lost by the swap: it GREW (new uncaptured commits), VANISHED, or SHRANK
/// (a concurrent checkpoint folded it). An empty WAL that our own read-only
/// EXPORT open may have created (size 0 where there was none) is deliberately
/// NOT a violation, and mtime is NOT compared — only a real size/presence delta
/// counts, keeping the fence free of self-perturbation false positives.
pub(crate) fn wal_fence_violated(before: WalFence, now: WalFence) -> bool {
    if now.size > before.size {
        return true;
    }
    if before.present && !now.present {
        return true;
    }
    if before.present && now.present && now.size < before.size {
        return true;
    }
    false
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

/// `<db>.install-intent` journal path.
pub fn install_intent_path(db_path: &Path) -> PathBuf {
    sibling(db_path, INSTALL_INTENT_SUFFIX)
}

/// Journal (write + fsync file + fsync dir) the install-intent for a swap that
/// is about to rename `staging` onto `db_path`. Made durable BEFORE the rename
/// so a crash anywhere in the swap is resumable at boot.
pub fn write_install_intent(
    db_path: &Path,
    staging: &Path,
    staging_sha256: &str,
) -> Result<InstallIntent> {
    let created_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let intent = InstallIntent {
        staging_path: staging.to_path_buf(),
        staging_sha256: staging_sha256.to_string(),
        target_path: db_path.to_path_buf(),
        created_at_unix,
    };
    let path = install_intent_path(db_path);
    let bytes = serde_json::to_vec_pretty(&intent).map_err(|e| SnapshotError::BadMeta {
        path: path.clone(),
        detail: format!("serialize install-intent: {e}"),
    })?;
    std::fs::write(&path, bytes).map_err(|e| SnapshotError::io(&path, e))?;
    fsync_file(&path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fsync_dir(parent)?;
    }
    Ok(intent)
}

/// Read + parse the install-intent journal beside `db_path`, if present.
pub fn read_install_intent(db_path: &Path) -> Option<InstallIntent> {
    let path = install_intent_path(db_path);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Durably remove the install-intent journal (dir fsync so the clear persists).
fn clear_install_intent(db_path: &Path) -> Result<()> {
    let path = install_intent_path(db_path);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| SnapshotError::io(&path, e))?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fsync_dir(parent)?;
        }
    }
    Ok(())
}

/// Copy an unreconcilable journal aside as boot evidence (never deletes the
/// original — boot stays refused until an operator resolves it).
fn preserve_install_intent(db_path: &Path) -> Result<PathBuf> {
    let src = install_intent_path(db_path);
    let dest = sibling(
        db_path,
        &format!("{INSTALL_INTENT_SUFFIX}.unreconciled-{}", unique_tag()),
    );
    std::fs::copy(&src, &dest).map_err(|e| SnapshotError::io(&dest, e))?;
    Ok(dest)
}

/// PURE boot-resume decision (no I/O) — the load-bearing core proven by the
/// `rustc --test` extraction. Given whether the journal is present, whether the
/// staging file is still there and matches the journaled SHA, and whether the
/// live target already matches the journaled SHA, decide how to reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeDecision {
    NoPending,
    Complete,
    ClearStaleWal,
    Refuse,
}

pub(crate) fn decide_resume(
    intent_present: bool,
    staging_present: bool,
    staging_sha_matches: bool,
    live_matches_target: bool,
) -> ResumeDecision {
    if !intent_present {
        return ResumeDecision::NoPending;
    }
    if staging_present {
        // (a) staging still here => the rename had not happened. Finish it iff
        // the staging bytes match the journaled SHA; a mismatch means a
        // corrupt/partial staging file — do NOT guess.
        if staging_sha_matches {
            ResumeDecision::Complete
        } else {
            ResumeDecision::Refuse
        }
    } else if live_matches_target {
        // (b) staging gone + live already equals the journaled identity => the
        // rename happened; only the stale-WAL delete + journal clear remain.
        ResumeDecision::ClearStaleWal
    } else {
        // (c) neither reconciles => preserve evidence + refuse.
        ResumeDecision::Refuse
    }
}

/// Resume a durable-checkpoint install that a crash interrupted. Call this at
/// the boot chokepoint BEFORE any DuckDB open. It reconciles the journaled swap
/// deterministically:
///
///   (a) staging present + matches the journaled SHA => complete the install
///       (atomic rename + stale-WAL delete) and clear the journal;
///   (b) staging gone + live already matches the journaled identity => the
///       rename happened, so just delete the now-stale WAL and clear the journal
///       (this is what closes the naive-pragma double-replay window);
///   (c) neither reconciles => preserve evidence and REFUSE (boot-fatal).
///
/// Pure file operations (no DuckDB), so it is exhaustively unit-tested and safe
/// to run before the storage engine is opened.
pub fn resume_pending_install(db_path: &Path) -> Result<ResumeAction> {
    let Some(intent) = read_install_intent(db_path) else {
        return Ok(ResumeAction::NoPendingInstall);
    };
    let staging = intent.staging_path.clone();
    let target = intent.target_path.clone();
    let staging_present = staging.exists();
    let staging_sha_matches = staging_present
        && sha256_file(&staging)
            .map(|s| s == intent.staging_sha256)
            .unwrap_or(false);
    let live_matches_target = target.exists()
        && sha256_file(&target)
            .map(|s| s == intent.staging_sha256)
            .unwrap_or(false);

    match decide_resume(
        true,
        staging_present,
        staging_sha_matches,
        live_matches_target,
    ) {
        // Unreachable (the journal is present), mapped for totality.
        ResumeDecision::NoPending => Ok(ResumeAction::NoPendingInstall),
        ResumeDecision::Complete => {
            // atomic_install: rename staging->target, delete the now-stale target
            // WAL, fsync the parent dir. Then clear the journal. The verified-
            // good marker re-establishes on the next live_durable_checkpoint.
            atomic_install(&staging, &target)?;
            clear_install_intent(db_path)?;
            Ok(ResumeAction::CompletedInstall)
        }
        ResumeDecision::ClearStaleWal => {
            let target_wal = wal_sibling(&target);
            if target_wal.exists() {
                std::fs::remove_file(&target_wal).map_err(|e| SnapshotError::io(&target_wal, e))?;
            }
            if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
                fsync_dir(parent)?;
            }
            clear_install_intent(db_path)?;
            Ok(ResumeAction::ClearedStaleWal)
        }
        ResumeDecision::Refuse => {
            let preserved = preserve_install_intent(db_path)?;
            Err(SnapshotError::RestoreRefused(format!(
                "unreconcilable install-intent journal at {}: staging {} present={}, \
                 live target {} present={} matched={}; evidence preserved to {} — \
                 refusing to open the DB (investigate; do NOT hand-edit)",
                install_intent_path(db_path).display(),
                staging.display(),
                staging_present,
                target.display(),
                target.exists(),
                live_matches_target,
                preserved.display()
            )))
        }
    }
}

/// Take ONE crash-safe durable checkpoint of `db_path`, leaving the live
/// file pristine + a fresh verified-good marker. Reuses ADR-0082's
/// corruption-free logical export, then commits with [`atomic_install`] under a
/// journaled install-intent so the WAL handling is an explicit crash-recoverable
/// protocol — never an in-place WAL fold and never a replayable adjacency.
///
/// Ordering (ADR-0098 R2, finding B):
///   0. capture a live-WAL fence baseline (Bug-2 belt);
///   i. `EXPORT` the live DB → a private staging export dir on a connection that
///      sets `disable_checkpoint_on_shutdown`, so its drop never folds the live
///      WAL in place; validate the export (import + smoke + hash-chain) and
///      **abort without touching the live file** if it does not validate;
///      `IMPORT` + `CHECKPOINT` into a fresh, private staging `*.duckdb`;
///  ii. fsync the staging file, then journal an fsync'd `<db>.install-intent`
///      (staging path + SHA-256 + target) BEFORE the rename;
/// iii. [`atomic_install`] renames the staging file over the live path;
///  iv. …and deletes the now-stale target WAL (both inside `atomic_install`);
///   v. clear the journal, then write the verified-good marker.
///
/// A crash at any instant is reconciled at boot by [`resume_pending_install`],
/// which closes BOTH the in-place-fold window and the naive-pragma double-replay
/// window. A live-WAL that grew during the EXPORT (a concurrent unmigrated
/// writer) ABORTS the checkpoint, live file untouched.
///
/// DuckDB-backed → its full crash-injection integration test is Mac-gated.
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

    // 0. Bug-2 belt (ADR-0098 R2, defense-in-depth; the primary swap-orphan fix
    //    is R3). Fence the LIVE WAL: capture (presence, size) BEFORE the EXPORT
    //    begins so that, just before the swap, we can prove no concurrent writer
    //    appended commits our logical snapshot did not capture. Meaningful under
    //    the shared Handle's single-writer lock the runtime callers hold.
    let live_wal = wal_sibling(db_path);
    let wal_fence_before = wal_stat(&live_wal);

    // 1. Logical EXPORT of the live DB (a table scan, never the ART).
    //    ADR-0098 R2 (finding B) — set `disable_checkpoint_on_shutdown` on THIS
    //    connection (the F6 paired-site miss: take.rs:208 had it, crash_safe.rs
    //    did not). Without it, dropping this plain read-write connection triggers
    //    DuckDB's implicit close-checkpoint, folding the live WAL IN PLACE — the
    //    exact `duckdb#23046` locus this primitive exists to avoid. The WAL is
    //    instead handled explicitly by the journaled swap below. (Exact pragma
    //    string shared with take.rs + aberp-db's Handle; an unknown pragma errors
    //    HARD, so a future rename surfaces loudly — never silently.)
    {
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")?;
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

    // 3a. fsync the finished staging file so its bytes are durable BEFORE we
    //     journal its identity or swap it in, then hash it for the journal.
    fsync_file(&staging)?;
    let staging_sha256 = sha256_file(&staging)?;

    // 3b. Bug-2 fence re-check (see step 0). A grown / vanished / shrunk live
    //     WAL since the EXPORT began is evidence of a concurrent unmigrated
    //     writer: ABORT and leave the live DB + its WAL untouched rather than
    //     swap a stale staging file over it (which would lose those commits).
    let wal_fence_now = wal_stat(&live_wal);
    if wal_fence_violated(wal_fence_before, wal_fence_now) {
        let _ = std::fs::remove_dir_all(&export_dir);
        let _ = std::fs::remove_file(&staging);
        let _ = std::fs::remove_file(&staging_wal);
        return Err(SnapshotError::RestoreRefused(format!(
            "durable_checkpoint aborted: live WAL {} changed during the EXPORT \
             (before present={}/{} bytes, now present={}/{} bytes) — a concurrent \
             writer's commits would be lost by the swap; refusing (live DB untouched)",
            live_wal.display(),
            wal_fence_before.present,
            wal_fence_before.size,
            wal_fence_now.present,
            wal_fence_now.size
        )));
    }

    // 4. (ADR-0098 R2, step ii) Journal an fsync'd install-intent (staging path
    //    + SHA-256 + target) BEFORE the rename. A crash from here until the
    //    journal is cleared is RESUMED at boot by `resume_pending_install`,
    //    closing BOTH the in-place-fold and the naive-pragma double-replay
    //    windows.
    let _intent = write_install_intent(db_path, &staging, &staging_sha256)?;

    // 5. (steps iii+iv) Atomic, fsync'd swap over the live file, which renames
    //    the staging file in AND deletes the now-stale target WAL AND fsyncs the
    //    directory (all-or-nothing).
    atomic_install(&staging, db_path)?;

    // 6. (step v) Clear the journal, then record the verified-good marker for
    //    the freshly installed file.
    clear_install_intent(db_path)?;
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
pub(crate) fn sibling(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os = db_path.as_os_str().to_owned();
    os.push(suffix);
    PathBuf::from(os)
}

/// Process + nanosecond tag so concurrent/again runs never collide.
pub(crate) fn unique_tag() -> String {
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

    // ===== ADR-0098 R2 (finding B): journaled install-intent + boot-resume =====

    #[test]
    fn decide_resume_maps_the_four_cases() {
        // no journal => nothing to do (crash-after-staging-before-journal path)
        assert_eq!(
            decide_resume(false, true, true, true),
            ResumeDecision::NoPending
        );
        // (a) staging present + SHA matches => complete the interrupted rename
        assert_eq!(
            decide_resume(true, true, true, false),
            ResumeDecision::Complete
        );
        // staging present but SHA mismatch => refuse (never guess)
        assert_eq!(
            decide_resume(true, true, false, false),
            ResumeDecision::Refuse
        );
        // (b) staging gone + live matches target => delete stale WAL + clear
        assert_eq!(
            decide_resume(true, false, false, true),
            ResumeDecision::ClearStaleWal
        );
        // (c) staging gone + live does NOT match => refuse
        assert_eq!(
            decide_resume(true, false, false, false),
            ResumeDecision::Refuse
        );
    }

    #[test]
    fn wal_fence_flags_growth_vanish_shrink_but_not_empty_appearance() {
        let none = WalFence {
            present: false,
            size: 0,
        };
        let empty = WalFence {
            present: true,
            size: 0,
        };
        let w100 = WalFence {
            present: true,
            size: 100,
        };
        let w200 = WalFence {
            present: true,
            size: 200,
        };
        // unchanged => ok
        assert!(!wal_fence_violated(w100, w100));
        assert!(!wal_fence_violated(none, none));
        // our own read-only open may create an empty WAL => NOT a violation
        assert!(!wal_fence_violated(none, empty));
        // grew (new commits) / a WAL appeared with data => violation
        assert!(wal_fence_violated(w100, w200));
        assert!(wal_fence_violated(none, w100));
        // vanished (concurrent fold) / shrank (partial fold) => violation
        assert!(wal_fence_violated(w100, none));
        assert!(wal_fence_violated(w200, w100));
    }

    #[test]
    fn install_intent_journal_roundtrips() {
        let t = Tmp::new("intent-rt");
        let db = t.join("live.duckdb");
        std::fs::write(&db, b"live").unwrap();
        let staging = t.join("live.duckdb.ckpt-staging.duckdb");
        std::fs::write(&staging, b"fresh").unwrap();
        let sha = crate::take::sha256_file(&staging).unwrap();
        let written = write_install_intent(&db, &staging, &sha).unwrap();
        let read = read_install_intent(&db).expect("intent present");
        assert_eq!(written, read);
        assert_eq!(read.target_path, db);
        assert_eq!(read.staging_path, staging);
        assert_eq!(read.staging_sha256, sha);
        assert!(install_intent_path(&db).exists());
    }

    #[test]
    fn resume_no_intent_is_a_noop() {
        // crash-after-staging-before-journal: no journal was ever written, so
        // boot sees no intent and the old DB is left exactly intact.
        let t = Tmp::new("resume-none");
        let db = t.join("live.duckdb");
        std::fs::write(&db, b"OLD-GOOD").unwrap();
        let orphan_staging = t.join("live.duckdb.ckpt-staging.duckdb");
        std::fs::write(&orphan_staging, b"NEVER-JOURNALED").unwrap();
        assert_eq!(
            resume_pending_install(&db).unwrap(),
            ResumeAction::NoPendingInstall
        );
        assert_eq!(std::fs::read(&db).unwrap(), b"OLD-GOOD");
    }

    #[test]
    fn resume_completes_interrupted_rename_and_clears_stale_wal() {
        // crash-after-journal-before-rename: staging is built + journaled but the
        // rename had not run. Boot completes it and drops the now-stale live WAL.
        let t = Tmp::new("resume-a");
        let db = t.join("live.duckdb");
        std::fs::write(&db, b"OLD-GOOD").unwrap();
        let stale_wal = wal_sibling(&db);
        std::fs::write(&stale_wal, b"stale-live-wal").unwrap();
        let staging = t.join("live.duckdb.ckpt-staging.duckdb");
        std::fs::write(&staging, b"NEW-SELF-CONTAINED").unwrap();
        let sha = crate::take::sha256_file(&staging).unwrap();
        write_install_intent(&db, &staging, &sha).unwrap();

        assert_eq!(
            resume_pending_install(&db).unwrap(),
            ResumeAction::CompletedInstall
        );
        assert_eq!(std::fs::read(&db).unwrap(), b"NEW-SELF-CONTAINED");
        assert!(
            !staging.exists(),
            "staging consumed by the completing rename"
        );
        assert!(
            !stale_wal.exists(),
            "stale live WAL deleted — no foreign replay"
        );
        assert!(!install_intent_path(&db).exists(), "journal cleared");
    }

    #[test]
    fn resume_clears_stale_wal_when_rename_already_happened() {
        // crash-after-rename-before-WAL-clear (the naive-pragma double-replay
        // window): the live file already IS the fresh bytes; a foreign WAL still
        // sits beside it. Boot must delete that WAL and clear the journal.
        let t = Tmp::new("resume-b");
        let db = t.join("live.duckdb");
        std::fs::write(&db, b"NEW-SELF-CONTAINED").unwrap();
        let sha = crate::take::sha256_file(&db).unwrap();
        let stale_wal = wal_sibling(&db);
        std::fs::write(&stale_wal, b"foreign-wal-from-old-file").unwrap();
        // staging path recorded but already gone (renamed away).
        let staging = t.join("live.duckdb.ckpt-staging.duckdb");
        write_install_intent(&db, &staging, &sha).unwrap();

        assert_eq!(
            resume_pending_install(&db).unwrap(),
            ResumeAction::ClearedStaleWal
        );
        assert_eq!(
            std::fs::read(&db).unwrap(),
            b"NEW-SELF-CONTAINED",
            "live file untouched"
        );
        assert!(
            !stale_wal.exists(),
            "foreign WAL deleted — double-replay prevented"
        );
        assert!(!install_intent_path(&db).exists(), "journal cleared");
    }

    #[test]
    fn resume_refuses_and_preserves_when_unreconcilable() {
        // intent present but neither a valid staging file nor a matching live
        // file exists => refuse + preserve evidence, never guess.
        let t = Tmp::new("resume-c");
        let db = t.join("live.duckdb");
        std::fs::write(&db, b"SOMETHING-ELSE").unwrap();
        let staging = t.join("live.duckdb.ckpt-staging.duckdb"); // absent
        write_install_intent(&db, &staging, "deadbeef_matches_no_file").unwrap();

        let err = resume_pending_install(&db).unwrap_err();
        assert!(matches!(err, SnapshotError::RestoreRefused(_)));
        // original journal still present (boot stays refused until operator acts)
        assert!(install_intent_path(&db).exists());
        // an evidence copy was written aside
        let preserved = std::fs::read_dir(&t.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".unreconciled-"));
        assert!(preserved, "unreconcilable evidence preserved aside");
    }
}
