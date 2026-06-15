//! Snapshot operations: take (EXPORT), validate (IMPORT + smoke), restore.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{BinaryHash, Ledger, TenantId};
use duckdb::Connection;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::store::{dir_size, next_seq, snapshot_dir_name, write_meta, PARTIAL_SUFFIX};
use crate::{Result, SnapshotError, SnapshotMeta, SnapshotRecord};

/// Outcome of [`validate_export`]. Validation *failing* is a normal result
/// (the snapshot is kept and marked invalid), not an error — so this is a
/// value, not a `Result`. The only hard errors (e.g. the source DB cannot
/// be opened for export) surface from [`take_snapshot`] itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    pub ok: bool,
    pub invoice_count: i64,
    pub audit_count: i64,
    pub chain_len: u64,
    pub error: Option<String>,
}

/// Single-quote a path for embedding in a DuckDB SQL string, doubling any
/// embedded single quote. Tenant DB paths never contain quotes in
/// practice, but escaping is cheap and removes the foot-gun.
fn sql_quote(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\'', "''");
    format!("'{s}'")
}

/// Hex SHA-256 of a file's bytes. Reads the whole file into memory — fine
/// at tenant scale (S393 `copy_atomic` does the same).
fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| SnapshotError::io(path, e))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Re-import an `EXPORT DATABASE` directory into a **throwaway in-memory**
/// DuckDB and run the smoke set:
///   1. `IMPORT DATABASE` must succeed (rebuilds schema + loads rows),
///   2. `count(*)` of `invoice` and `audit_ledger` are recorded,
///   3. the ADR-0008 hash chain re-verifies end-to-end against the tenant
///      genesis ([`Ledger::verify_chain`]).
///
/// In-memory (not a temp file) is deliberate: it avoids writing a second
/// on-disk DuckDB and the checkpoint/ART **re-open** replay path
/// (`duckdb#23046`, S375) entirely — the validation can never itself
/// trigger the corruption class it exists to guard against.
///
/// `invoice` count is best-effort: a brand-new tenant DB may not have the
/// table yet, which records `-1` but does **not** fail validation. The hard
/// gates are: import succeeds, `audit_ledger` is present, chain verifies.
pub fn validate_export(export_dir: &Path, tenant: &str) -> ValidationReport {
    let tenant_id = match TenantId::new(tenant.to_string()) {
        Some(t) => t,
        None => {
            return ValidationReport {
                ok: false,
                invoice_count: -1,
                audit_count: -1,
                chain_len: 0,
                error: Some(format!("invalid tenant id {tenant:?}")),
            }
        }
    };

    let conn = match Connection::open_in_memory() {
        Ok(c) => c,
        Err(e) => return fail(format!("open in-memory validation db: {e}")),
    };

    if let Err(e) = conn.execute_batch(&format!("IMPORT DATABASE {};", sql_quote(export_dir))) {
        return fail(format!("IMPORT DATABASE failed (corrupt/incomplete export): {e}"));
    }

    // invoice: informational, table may be absent on a fresh tenant.
    let invoice_count: i64 = conn
        .query_row("SELECT count(*) FROM invoice", [], |r| r.get(0))
        .unwrap_or(-1);

    // audit_ledger: hard gate — must be present.
    let audit_count: i64 = match conn.query_row("SELECT count(*) FROM audit_ledger", [], |r| r.get(0))
    {
        Ok(n) => n,
        Err(e) => return fail(format!("audit_ledger unreadable in snapshot: {e}")),
    };

    // Verify the hash chain on the imported connection WITHOUT re-opening a
    // file (S375). Binary hash is irrelevant to chain verification (which
    // checks prev/entry hashes against the tenant genesis), so a zero hash
    // is fine here.
    let ledger = Ledger::from_connection(conn, tenant_id, BinaryHash::from_bytes([0u8; 32]));
    match ledger.verify_chain() {
        Ok(chain_len) => ValidationReport {
            ok: true,
            invoice_count,
            audit_count,
            chain_len,
            error: None,
        },
        Err(e) => ValidationReport {
            ok: false,
            invoice_count,
            audit_count,
            chain_len: 0,
            error: Some(format!("hash-chain verification failed: {e}")),
        },
    }
}

fn fail(msg: String) -> ValidationReport {
    ValidationReport {
        ok: false,
        invoice_count: -1,
        audit_count: -1,
        chain_len: 0,
        error: Some(msg),
    }
}

/// Take one validated logical snapshot of `db_path` into `store_dir`.
///
/// 1. Derive the next seq by scanning the store.
/// 2. SHA-256 the live source file (records *which* physical state this
///    came from).
/// 3. `EXPORT DATABASE` into `<store>/snap-<seq>-<ts>.partial`.
/// 4. [`validate_export`] the partial — a failure does not abort; the
///    snapshot is kept and tagged `valid=false`.
/// 5. Write `meta.json`, then atomically rename `.partial` → final.
///
/// Returns the finalized [`SnapshotRecord`]. The caller inspects
/// `record.meta.valid` to decide whether to emit `SnapshotCreated` or
/// `SnapshotValidationFailed`. A hard error (source missing, export failed,
/// rename failed) is returned as `Err`.
pub fn take_snapshot(
    db_path: &Path,
    store_dir: &Path,
    tenant: &str,
    now: OffsetDateTime,
) -> Result<SnapshotRecord> {
    if !db_path.exists() {
        return Err(SnapshotError::SourceMissing(db_path.to_path_buf()));
    }
    std::fs::create_dir_all(store_dir).map_err(|e| SnapshotError::io(store_dir, e))?;

    let seq = next_seq(store_dir)?;
    let source_db_sha256 = sha256_file(db_path)?;

    let final_name = snapshot_dir_name(seq, now)?;
    let final_dir = store_dir.join(&final_name);
    let partial_dir = store_dir.join(format!("{final_name}{PARTIAL_SUFFIX}"));

    // A crashed prior run could leave a stale partial — clear it so EXPORT
    // (which creates the dir) starts clean.
    if partial_dir.exists() {
        std::fs::remove_dir_all(&partial_dir).map_err(|e| SnapshotError::io(&partial_dir, e))?;
    }

    // EXPORT runs against the live DB. When `serve` is running this opens a
    // second in-process connection (DuckDB shares one instance per process,
    // so no cross-process lock conflict); from the stopped-server CLI it is
    // the only opener. EXPORT is a logical table scan — it never touches
    // the ART/checkpoint structure that corrupts.
    {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(&format!(
            "EXPORT DATABASE {} (FORMAT PARQUET);",
            sql_quote(&partial_dir)
        ))?;
    }

    let report = validate_export(&partial_dir, tenant);
    let byte_size = dir_size(&partial_dir)?;

    let meta = SnapshotMeta {
        seq,
        created_at: now,
        source_db_sha256,
        byte_size,
        valid: report.ok,
        invoice_count: report.invoice_count,
        audit_count: report.audit_count,
        chain_len: report.chain_len,
        validation_error: report.error,
    };
    write_meta(&partial_dir, &meta)?;

    // Atomic finalize: the snapshot only becomes visible to listing/seq
    // derivation once it is whole.
    std::fs::rename(&partial_dir, &final_dir)
        .map_err(|e| SnapshotError::io(&final_dir, e))?;

    Ok(SnapshotRecord {
        dir: final_dir,
        meta,
    })
}

/// Guard executed BEFORE any restore touches disk. The safety lives here,
/// in the binary, not in operator discipline (`[[trust-code-not-operator]]`):
///
///   - `--confirm` must be passed, AND
///   - the target must NOT be under any `~/.aberp/` tenant home (which
///     includes the live `~/.aberp/prod/aberp.duckdb`).
///
/// A fat-fingered restore therefore cannot clobber a live DB. Recovering
/// prod is a deliberate two-step: restore to a side path, stop serve, swap.
pub fn ensure_restore_allowed(target: &Path, confirm: bool) -> Result<()> {
    if !confirm {
        return Err(SnapshotError::RestoreRefused(
            "pass --confirm to acknowledge this overwrites the target database".to_string(),
        ));
    }
    let abs = absolutise(target);
    if path_is_under_aberp_home(&abs) {
        return Err(SnapshotError::RestoreRefused(format!(
            "target {} is under a live ~/.aberp tenant home — restore to a side path, \
             stop `aberp serve`, then swap the file in manually. \
             Magyarul: ne állíts vissza közvetlenül az éles adatbázisra.",
            abs.display()
        )));
    }
    Ok(())
}

/// Make a path absolute without requiring it to exist (so a not-yet-created
/// restore target still gets checked). Joins the current dir for relatives.
fn absolutise(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// True if any component of `path` is `.aberp` — i.e. it lives under a live
/// tenant home. Broader than matching only `prod` on purpose: no live
/// tenant DB should ever be a restore target.
fn path_is_under_aberp_home(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".aberp")
}

/// Restore a snapshot directory into `target` via `IMPORT DATABASE`, then
/// checkpoint so `target` is a single self-contained, freshly-indexed file.
///
/// Refuses to restore from an export that does not itself validate (we
/// never rebuild a DB from a corrupt snapshot). Builds into a sibling
/// `*.restoring` file and renames over `target` so a crash mid-import never
/// leaves a torn target. **Does not** enforce the prod-overwrite guard —
/// callers MUST call [`ensure_restore_allowed`] first (the CLI does).
pub fn restore_into(export_dir: &Path, target: &Path, tenant: &str) -> Result<()> {
    // Refuse to restore from a snapshot that fails validation.
    let report = validate_export(export_dir, tenant);
    if !report.ok {
        return Err(SnapshotError::RestoreFromInvalid(
            export_dir.display().to_string(),
        ));
    }

    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| SnapshotError::io(parent, e))?;
    }

    let mut staging = target.as_os_str().to_owned();
    staging.push(".restoring");
    let staging = PathBuf::from(staging);
    let staging_wal = wal_sibling(&staging);
    // Clear any leftovers from a crashed prior restore.
    for p in [&staging, &staging_wal] {
        if p.exists() {
            std::fs::remove_file(p).map_err(|e| SnapshotError::io(p, e))?;
        }
    }

    {
        let conn = Connection::open(&staging)?;
        conn.execute_batch(&format!("IMPORT DATABASE {};", sql_quote(export_dir)))?;
        conn.execute_batch("CHECKPOINT;")?;
    }
    // The checkpointed staging file should have no WAL; drop any lingering
    // one so the rename moves a lone file.
    if staging_wal.exists() {
        let _ = std::fs::remove_file(&staging_wal);
    }

    // Swap staging over target, clearing target's stale WAL (the imported
    // DB is self-contained; an old WAL would corrupt it on next open).
    let target_wal = wal_sibling(target);
    std::fs::rename(&staging, target).map_err(|e| SnapshotError::io(target, e))?;
    if target_wal.exists() {
        std::fs::remove_file(&target_wal).map_err(|e| SnapshotError::io(&target_wal, e))?;
    }
    Ok(())
}

/// DuckDB names the WAL by appending `.wal` to the FULL filename (so
/// `x.duckdb` → `x.duckdb.wal`) — NOT `Path::with_extension`.
fn wal_sibling(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_owned();
    os.push(".wal");
    PathBuf::from(os)
}
