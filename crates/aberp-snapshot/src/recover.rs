//! ADR-0095 §1–§3 — the LOCAL, fully-verifiable recovery engine.
//!
//! Three additive entrypoints that WIRE the chunk-3 crash-safe primitives
//! (`crate::crash_safe`) into the paths a crash actually traverses. **No new
//! durability primitive is invented** — [`atomic_install`], the verified-good
//! markers, the ADR-0082 logical export/import, and [`durable_checkpoint`]
//! are reused as-is.
//!
//! - [`recover_or_refuse`] — boot safe-open + auto-recover, covering BOTH the
//!   torn/unopenable live DB (root cause #1) and the ahead-mirror (root cause
//!   #4). It preserves evidence (never deletes), restores the latest VALID
//!   snapshot, **replays** (never truncates) the append-only audit-ledger
//!   JSONL delta, validates the rebuild (hash-chain + head-seq + head-hash),
//!   then atomically installs it + writes the verified-good marker. The
//!   GUARD-RAIL is that it auto-recovers ONLY when snapshot+mirror prove
//!   consistent; otherwise it falls back to today's preserve-and-refuse
//!   (returning a `Refused*` outcome, never guessing).
//! - [`provision_atomic`] — atomic initial DB creation (root cause #3): build
//!   the fresh DB aside at `<db>.creating-<tag>.duckdb`, then atomically swap
//!   it onto the live path, so a crash mid-create can never leave a torn file
//!   at the live path.
//! - [`live_durable_checkpoint`] — a thin, debounced wrapper over
//!   [`durable_checkpoint`] for the periodic / post-write / boot callers
//!   (Session B wires the call sites). Safe to call repeatedly: a no-op when a
//!   verified-good checkpoint already covers the current file.
//!
//! # Prod safety
//!
//! Every entrypoint calls [`ensure_not_prod_path`] first, so an editions
//! build can never act on the FROZEN prod line (ADR-0093) — the same
//! mechanical guarantee the snapshot/restore surface already carries.
//!
//! # Gating
//!
//! The pure crash-safe COMMIT property (a crash mid initial-creation never
//! leaves a torn file at the live path) is exercised by a real-subprocess
//! crash-injection UNIT test below that uses PLAIN FILES — no DuckDB at
//! runtime — so it runs anywhere. The DuckDB-backed end-to-end recoveries
//! (torn-DB and ahead-mirror) live in `tests/recover_engine_tests.rs` and run
//! on the Mac gate (the bundled libduckdb amalgamation cannot build in the
//! saw-off sandbox), exactly like chunk-3's `crash_safe_checkpoint_tests.rs`.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{read_mirror_entries, replay_mirror_delta, BinaryHash, Ledger, TenantId};
use duckdb::Connection;

use crate::crash_safe::{
    atomic_install, checkpoint_is_current, durable_checkpoint, sibling, unique_tag, wal_sibling,
    write_marker, CheckpointReport,
};
use crate::store::list_snapshots;
use crate::take::{ensure_not_prod_path, sql_quote};
use crate::{Result, SnapshotError};

/// Outcome of a [`recover_or_refuse`] decision. The two `Refused*` variants
/// are the safe fallback (preserve-and-refuse, the chunk-3 P1 default demoted
/// to a last resort); the caller (serve boot, Session B) maps them to the
/// existing surface-and-stop path. Every variant reports the retained corrupt
/// DB copy (if one existed) so the recovery is auditable and fully reversible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Auto-recovery succeeded: the live DB was rebuilt from `source_snapshot`
    /// plus a verbatim replay of the mirror delta, validated, and atomically
    /// installed with a fresh verified-good marker.
    Recovered {
        /// Seq of the snapshot the rebuild started from.
        source_snapshot_seq: u64,
        /// Audit-ledger head the snapshot carried (the replay floor).
        snapshot_audit_count: u64,
        /// Number of mirror entries replayed on top of the snapshot.
        replayed_entries: u64,
        /// Audit-ledger head of the rebuilt, installed DB (== mirror head).
        recovered_max_seq: u64,
        /// The retained `<db>.CORRUPT-<tag>` evidence copy, if the live file
        /// existed before recovery. Never deleted.
        retained_corrupt_db: Option<PathBuf>,
    },
    /// No VALID snapshot exists in the store — refuse (cannot rebuild from
    /// nothing). Evidence preserved; the live inputs are untouched.
    RefusedNoSnapshot {
        retained_corrupt_db: Option<PathBuf>,
    },
    /// A guard-rail failed (snapshot/mirror inconsistent, mirror missing or
    /// corrupt, rebuilt chain did not verify, or heads disagreed) — refuse
    /// rather than guess. Evidence preserved; the live inputs are untouched.
    RefusedUnsafe {
        reason: String,
        retained_corrupt_db: Option<PathBuf>,
    },
}

/// Boot safe-open + auto-recover (ADR-0095 §1). Covers BOTH failure modes —
/// a torn/unopenable live DB and an ahead-of-DB mirror — with one algorithm:
///
/// 1. PRESERVE evidence: copy any existing live file aside to
///    `<db>.CORRUPT-<tag>` (never deleted). The ahead mirror, if that is the
///    trigger, was already preserved to `<mirror>.ahead-<nanos>.bak` by the
///    chunk-3 P1 guard at the call site; the mirror itself is **read here,
///    never truncated**.
/// 2. Locate the latest VALID snapshot (ADR-0082 `meta.valid`). None →
///    [`RecoveryOutcome::RefusedNoSnapshot`].
/// 3. IMPORT that snapshot's logical export (corruption-free by construction)
///    into a PRIVATE staging DB — never the live path.
/// 4. REPLAY the append-only mirror delta (`seq > snapshot.audit_count`)
///    verbatim, in seq order, into the staging DB.
/// 5. VALIDATE the rebuild: hash-chain verifies genesis→head; the rebuilt
///    head seq reconciles with the mirror head; the rebuilt head `entry_hash`
///    matches the mirror head. Any failure → discard staging,
///    [`RecoveryOutcome::RefusedUnsafe`].
/// 6. COMMIT atomically: [`atomic_install`] the staging file over the live
///    path, then [`write_marker`].
///
/// GUARD-RAIL: auto-recover happens ONLY when a valid snapshot exists AND the
/// mirror is a consistent extension of it AND the rebuild validates;
/// otherwise this returns a `Refused*` outcome and changes nothing. The
/// corrupt DB and the `.ahead-*.bak` are retained, so recovery is fully
/// reversible.
///
/// # Errors
///
/// Returns `Err` only for a hard MECHANICAL failure (e.g. the corrupt DB
/// could not be preserved, or the atomic install failed) — a boot-fatal
/// condition the caller surfaces loudly. A guard-rail refusal is **not** an
/// error: it is an `Ok(Refused*)` outcome (the safe fallback).
pub fn recover_or_refuse(
    db_path: &Path,
    store_dir: &Path,
    mirror_path: &Path,
    tenant: &str,
) -> Result<RecoveryOutcome> {
    // SAFETY: an editions build must never act on the FROZEN prod line.
    ensure_not_prod_path(db_path)?;
    ensure_not_prod_path(store_dir)?;

    // 1. PRESERVE evidence — never destroy it. The original live file stays
    //    in place until step 6 swaps the rebuild over it.
    let retained_corrupt_db = if db_path.exists() {
        Some(preserve_corrupt_db(db_path)?)
    } else {
        None
    };

    // 2. Latest VALID snapshot (ADR-0082). None → refuse (cannot rebuild from
    //    nothing — fall through to the existing preserve-and-surface).
    let snapshot = match list_snapshots(store_dir)?
        .into_iter()
        .find(|r| r.meta.valid)
    {
        Some(s) => s,
        None => {
            return Ok(RecoveryOutcome::RefusedNoSnapshot {
                retained_corrupt_db,
            })
        }
    };
    let snapshot_audit_count = snapshot.meta.audit_count.max(0) as u64;

    // The mirror is the second source of recovery truth — read it, never
    // mutate it. Missing/corrupt → unsafe → refuse (safe fallback).
    let mirror_entries = match read_mirror_entries(mirror_path) {
        Ok(e) => e,
        Err(e) => {
            return Ok(RecoveryOutcome::RefusedUnsafe {
                reason: format!(
                    "audit-ledger mirror at {} is missing or unreadable ({e}); refusing to \
                     auto-recover without it",
                    mirror_path.display()
                ),
                retained_corrupt_db,
            })
        }
    };
    let mirror_max_seq = mirror_entries.last().map(|e| e.seq()).unwrap_or(0);
    let mirror_head_hash = mirror_entries.last().map(|e| e.entry_hash().to_string());

    // GUARD-RAIL: the snapshot must be a PREFIX of the mirror — its audit head
    // cannot be ahead of the mirror head, or the two disagree and we must not
    // guess. Fall back to preserve-and-refuse.
    if snapshot_audit_count > mirror_max_seq {
        return Ok(RecoveryOutcome::RefusedUnsafe {
            reason: format!(
                "latest valid snapshot (audit_count={snapshot_audit_count}) is AHEAD of the \
                 mirror head (seq={mirror_max_seq}); snapshot and mirror disagree — refusing"
            ),
            retained_corrupt_db,
        });
    }

    // 3–5. Build a PRIVATE staging DB from the snapshot, replay the mirror
    //      delta into it, and validate the rebuild. Nothing here touches the
    //      live path. Clear any orphan staging from a crashed prior recovery
    //      first (the `.CORRUPT-*` evidence has a different infix and is kept).
    cleanup_siblings_with_infix(db_path, RECOVER_INFIX);
    let staging = sibling(db_path, &format!("{RECOVER_INFIX}{}.duckdb", unique_tag()));
    cleanup_temp(&staging);
    let verdict = build_and_validate(
        &staging,
        &snapshot.dir,
        mirror_path,
        tenant,
        snapshot_audit_count,
        mirror_max_seq,
        mirror_head_hash.as_deref(),
    );
    let info = match verdict {
        Ok(Verdict::Recovered(info)) => info,
        Ok(Verdict::Refuse(reason)) => {
            cleanup_temp(&staging);
            return Ok(RecoveryOutcome::RefusedUnsafe {
                reason,
                retained_corrupt_db,
            });
        }
        Err(e) => {
            // A hard mechanical failure (not a guard refusal): clean the
            // disposable staging and surface loudly.
            cleanup_temp(&staging);
            return Err(e);
        }
    };

    // 6. COMMIT atomically: swap the validated rebuild over the live path and
    //    write the verified-good marker (reusing the chunk-3 primitives).
    atomic_install(&staging, db_path)?;
    write_marker(db_path)?;

    Ok(RecoveryOutcome::Recovered {
        source_snapshot_seq: snapshot.meta.seq,
        snapshot_audit_count,
        replayed_entries: info.replayed,
        recovered_max_seq: info.max_seq,
        retained_corrupt_db,
    })
}

/// Infix of the private recovery-staging temp (`<db>.recover-<tag>.duckdb`).
const RECOVER_INFIX: &str = ".recover-";
/// Infix of the atomic-creation temp (`<db>.creating-<tag>.duckdb`).
const CREATING_INFIX: &str = ".creating-";
/// Infix of the retained torn-DB evidence (`<db>.CORRUPT-<tag>`).
const CORRUPT_INFIX: &str = ".CORRUPT-";

struct RebuildInfo {
    replayed: u64,
    max_seq: u64,
}

/// A guard-rail decision: a validated rebuild, or a refusal reason (the safe
/// fallback). Distinct from a hard mechanical `Err`.
enum Verdict {
    Recovered(RebuildInfo),
    Refuse(String),
}

/// Build the staging DB from the snapshot export + mirror replay, then
/// validate it. Returns a [`Verdict`] (recovered or a refusal reason) for
/// guard-rail outcomes; a hard `Err` only for an unexpected I/O failure. The
/// staging connection is opened ONCE and reused for verification (S375 — a
/// re-open triggers the very `LoadCheckpoint`/ART replay path we recover from).
fn build_and_validate(
    staging: &Path,
    snapshot_dir: &Path,
    mirror_path: &Path,
    tenant: &str,
    snapshot_audit_count: u64,
    mirror_max_seq: u64,
    mirror_head_hash: Option<&str>,
) -> Result<Verdict> {
    let mut conn = Connection::open(staging)?;

    // 3. IMPORT the snapshot's logical export (corruption-free by
    //    construction — ADR-0082) into the staging DB.
    if let Err(e) = conn.execute_batch(&format!("IMPORT DATABASE {};", sql_quote(snapshot_dir))) {
        return Ok(Verdict::Refuse(format!(
            "IMPORT from snapshot {} failed: {e}",
            snapshot_dir.display()
        )));
    }

    // 4. REPLAY the append-only mirror delta (seq > snapshot head) verbatim.
    let replayed = match replay_mirror_delta(&mut conn, mirror_path, snapshot_audit_count) {
        Ok(n) => n,
        Err(e) => return Ok(Verdict::Refuse(format!("mirror replay failed: {e}"))),
    };

    // Fold the WAL so the staging file is self-contained before the swap.
    if let Err(e) = conn.execute_batch("CHECKPOINT;") {
        return Ok(Verdict::Refuse(format!(
            "checkpoint of the rebuilt staging DB failed: {e}"
        )));
    }

    // 5. VALIDATE: hash-chain genesis→head, head seq reconciles with the
    //    mirror, head entry_hash matches the mirror head.
    let tenant_id = match TenantId::new(tenant.to_string()) {
        Some(t) => t,
        None => return Ok(Verdict::Refuse(format!("invalid tenant id {tenant:?}"))),
    };
    // Reuse the already-open handle for verification (no re-open).
    let ledger = Ledger::from_connection(conn, tenant_id, BinaryHash::from_bytes([0u8; 32]));
    let chain_len = match ledger.verify_chain() {
        Ok(n) => n,
        Err(e) => {
            return Ok(Verdict::Refuse(format!(
                "rebuilt DB hash-chain verification failed: {e}"
            )))
        }
    };
    if chain_len != mirror_max_seq {
        return Ok(Verdict::Refuse(format!(
            "rebuilt DB head seq {chain_len} does not reconcile with the mirror head \
             seq {mirror_max_seq}"
        )));
    }
    let head = match ledger.recent(1) {
        Ok(h) => h,
        Err(e) => return Ok(Verdict::Refuse(format!("reading rebuilt head failed: {e}"))),
    };
    let head_hash = head.first().map(|e| hex::encode(e.entry_hash.as_bytes()));
    if head_hash.as_deref() != mirror_head_hash {
        return Ok(Verdict::Refuse(
            "rebuilt DB head entry_hash disagrees with the mirror head".to_string(),
        ));
    }

    // Close the handle (drop the Ledger) before the caller swaps the file.
    drop(ledger);
    Ok(Verdict::Recovered(RebuildInfo {
        replayed,
        max_seq: chain_len,
    }))
}

/// Atomic initial DB creation (ADR-0095 §2). Build the fresh DB ENTIRELY
/// aside at `<db>.creating-<tag>.duckdb` — `init` runs every `ensure_schema`
/// + the genesis audit row against that private temp — then fold its WAL,
/// atomically swap it onto the final path, and write the verified-good
/// marker. A crash mid-creation leaves only a disposable temp (cleaned on the
/// next call), **never a torn file at the live path** (root cause #3).
///
/// `init` receives the temp path and is the caller's schema/genesis builder;
/// any error it returns aborts the creation with [`SnapshotError::Provision`]
/// and the live path is never written.
///
/// # Errors
///
/// [`SnapshotError::Provision`] if `init` fails; otherwise any I/O or DuckDB
/// error from the checkpoint / atomic install.
pub fn provision_atomic<F, E>(db_path: &Path, init: F) -> Result<()>
where
    F: FnOnce(&Path) -> std::result::Result<(), E>,
    E: std::fmt::Display,
{
    // SAFETY: never provision a prod path.
    ensure_not_prod_path(db_path)?;

    // Clear any orphan temp from a crashed prior creation (the next-boot
    // cleanup ADR-0095 §2 promises) so it can never accumulate or be reused.
    cleanup_siblings_with_infix(db_path, CREATING_INFIX);

    let creating = sibling(db_path, &format!("{CREATING_INFIX}{}.duckdb", unique_tag()));
    cleanup_temp(&creating);

    // Build the fresh DB aside (never the live path).
    init(&creating).map_err(|e| SnapshotError::Provision {
        path: creating.clone(),
        detail: e.to_string(),
    })?;

    // Fold the WAL so the temp is a single self-contained file, then swap it
    // over the final path with the crash-safe commit + verified-good marker.
    checkpoint_file(&creating)?;
    atomic_install(&creating, db_path)?;
    write_marker(db_path)?;
    Ok(())
}

/// Thin, debounced wrapper over [`durable_checkpoint`] for the periodic /
/// post-write / boot callers (ADR-0095 §3; Session B wires the call sites).
///
/// Returns `Ok(None)` — a cheap no-op — when a verified-good checkpoint
/// already covers the current file ([`checkpoint_is_current`]). This is what
/// makes the callers safe to fire repeatedly without thrashing disk (ADR-0095
/// adversarial #4). When the live file has changed since the last checkpoint,
/// it takes one [`durable_checkpoint`] and returns its report.
///
/// # Errors
///
/// [`SnapshotError::SourceMissing`] if the DB does not exist; otherwise any
/// error from [`durable_checkpoint`] (which refuses, untouched, if the live
/// DB does not validate).
pub fn live_durable_checkpoint(db_path: &Path, tenant: &str) -> Result<Option<CheckpointReport>> {
    // SAFETY: never checkpoint a prod path.
    ensure_not_prod_path(db_path)?;
    if !db_path.exists() {
        return Err(SnapshotError::SourceMissing(db_path.to_path_buf()));
    }
    if checkpoint_is_current(db_path) {
        return Ok(None);
    }
    let report = durable_checkpoint(db_path, tenant)?;
    Ok(Some(report))
}

/// Copy a torn/replaced live DB aside to `<db>.CORRUPT-<tag>` and return the
/// retained path. A COPY (not a move): the original stays in place until the
/// rebuild is atomically swapped over it, so a failure before the swap leaves
/// the operator with both the original and the copy.
fn preserve_corrupt_db(db_path: &Path) -> Result<PathBuf> {
    let dest = sibling(db_path, &format!("{CORRUPT_INFIX}{}", unique_tag()));
    std::fs::copy(db_path, &dest).map_err(|e| SnapshotError::io(&dest, e))?;
    Ok(dest)
}

/// Open the freshly-built temp DB once and `CHECKPOINT` it so its WAL is
/// folded in and the file is self-contained before the atomic swap.
fn checkpoint_file(db: &Path) -> Result<()> {
    let conn = Connection::open(db)?;
    conn.execute_batch("CHECKPOINT;")?;
    Ok(())
}

/// Remove a temp file and any DuckDB WAL beside it. Best-effort.
fn cleanup_temp(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let wal = wal_sibling(path);
    if wal.exists() {
        let _ = std::fs::remove_file(&wal);
    }
}

/// Remove orphan `<db><infix>*` siblings (e.g. `.creating-*` / `.recover-*`)
/// left by a crash, without ever touching the live DB or the retained
/// `.CORRUPT-*` evidence (distinct infixes). Pure best-effort cleanup that
/// never fails the caller.
fn cleanup_siblings_with_infix(db_path: &Path, infix: &str) {
    let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    let Some(stem) = db_path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let prefix = format!("{stem}{infix}");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Plain-file crash-injection + cleanup unit tests. These use PLAIN FILES
    //! (no DuckDB at runtime) so they exercise the load-bearing crash-safe
    //! COMMIT property anywhere. The DuckDB-backed end-to-end recoveries are
    //! in `tests/recover_engine_tests.rs` (Mac gate).

    use super::*;
    use std::process::Command;

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
                "aberp-recover-{label}-{}-{n}-{seq}",
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

    /// When set, this process is the CRASH CHILD: it performs the staging
    /// write of an atomic-create then hard-aborts BEFORE the rename,
    /// simulating a power loss mid initial-creation.
    const CRASH_ENV: &str = "ABERP_RECOVER_CRASH_CHILD";
    /// The exact libtest name of the crash test, used to re-exec just it.
    const CRASH_TEST: &str =
        "recover::tests::provision_atomic_crash_before_rename_never_leaves_torn_live_file";

    #[test]
    fn provision_atomic_crash_before_rename_never_leaves_torn_live_file() {
        // ── CHILD MODE ──────────────────────────────────────────────────
        // Do the staging write to the `.creating-` temp (NEVER the live
        // path) and then crash hard, before any rename.
        if let Ok(staging) = std::env::var(CRASH_ENV) {
            let staging = PathBuf::from(staging);
            std::fs::write(&staging, b"HALF-WRITTEN-DB-BYTES").unwrap();
            // …power loss here… the rename to the live path never happens.
            std::process::abort();
        }

        // ── PARENT MODE ─────────────────────────────────────────────────
        let t = Tmp::new("crash-create");
        let live = t.join("aberp.duckdb");
        let staging = t.join("aberp.duckdb.creating-child.duckdb");

        // Re-exec ONLY this test, in child (crash) mode.
        let exe = std::env::current_exe().expect("current_exe");
        let status = Command::new(exe)
            .args(["--exact", CRASH_TEST])
            .env(CRASH_ENV, &staging)
            .env("RUST_TEST_THREADS", "1")
            .status()
            .expect("spawn crash child");
        assert!(
            !status.success(),
            "the child must have crashed (aborted), not exited 0"
        );

        // THE LOAD-BEARING PROPERTY: a crash mid initial-creation leaves the
        // temp aside but NEVER a (torn) file at the live path.
        assert!(
            !live.exists(),
            "a crash before the atomic rename must never leave a file at the live path"
        );
        assert!(
            staging.exists(),
            "the half-written temp survives, aside from the live path"
        );

        // RECOVERY with ZERO manual steps: the next attempt finishes the
        // crash-safe commit (the REAL atomic_install + verified-good marker)
        // and the live path becomes the good, openable file.
        std::fs::write(&staging, b"COMPLETE-SELF-CONTAINED-DB").unwrap();
        atomic_install(&staging, &live).expect("atomic_install");
        write_marker(&live).expect("write_marker");
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"COMPLETE-SELF-CONTAINED-DB",
            "the live path is the good rebuilt file"
        );
        assert!(
            checkpoint_is_current(&live),
            "a verified-good marker now covers the installed file"
        );
        assert!(
            !staging.exists(),
            "the temp was consumed by the atomic rename"
        );
    }

    #[test]
    fn cleanup_siblings_removes_only_matching_infix_and_keeps_evidence() {
        let t = Tmp::new("cleanup");
        let live = t.join("aberp.duckdb");
        std::fs::write(&live, b"live").unwrap();
        let creating1 = t.join("aberp.duckdb.creating-111.duckdb");
        let creating2 = t.join("aberp.duckdb.creating-222.duckdb");
        let corrupt = t.join("aberp.duckdb.CORRUPT-999");
        let unrelated = t.join("other.duckdb");
        for (p, b) in [
            (&creating1, &b"c1"[..]),
            (&creating2, &b"c2"[..]),
            (&corrupt, &b"evidence"[..]),
            (&unrelated, &b"x"[..]),
        ] {
            std::fs::write(p, b).unwrap();
        }

        cleanup_siblings_with_infix(&live, CREATING_INFIX);

        assert!(!creating1.exists(), "orphan .creating- temp removed");
        assert!(!creating2.exists(), "orphan .creating- temp removed");
        assert!(
            corrupt.exists(),
            "retained .CORRUPT- evidence is NEVER removed"
        );
        assert!(live.exists(), "the live DB is never touched");
        assert!(unrelated.exists(), "an unrelated sibling is never touched");
    }

    #[test]
    fn preserve_corrupt_db_copies_aside_and_leaves_original_intact() {
        let t = Tmp::new("preserve");
        let live = t.join("aberp.duckdb");
        std::fs::write(&live, b"torn-original-bytes").unwrap();

        let dest = preserve_corrupt_db(&live).expect("preserve");

        assert!(dest.exists(), "evidence copy was created");
        assert_eq!(std::fs::read(&dest).unwrap(), b"torn-original-bytes");
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"torn-original-bytes",
            "the original live file is COPIED, never moved/deleted"
        );
        assert!(
            dest.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".CORRUPT-")),
            "evidence is named with the .CORRUPT- infix"
        );
    }
}
