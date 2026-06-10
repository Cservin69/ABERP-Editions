//! Orchestration for the `aberp audit-rebuild` subcommand (S341 / PR-36).
//!
//! # Why this exists
//!
//! S332 diagnosed a DuckDB ART corruption on PROD: the `audit_ledger`
//! table's on-disk secondary index (`UNIQUE(seq)` / `UNIQUE(id)`) lands
//! in a state where every subsequent insert panics inside
//! `FixedSizeAllocator::New` → `Prefix::New` → `ARTOperator::Insert`.
//! Because every audit-emitting transaction (material CRUD, partner
//! CRUD, catalogue-push, even the shutdown row) appends to that table in
//! the SAME transaction as the state change it describes, the panic
//! aborts the whole commit — Ervin's local "save a material → 500 →
//! nothing saved" symptom.
//!
//! S332 / S335 proved the fix can NOT be schema relaxation: dropping
//! `UNIQUE(seq)` silently forks the tamper-evident hash chain (a
//! coherence probe demonstrated row loss + seq reuse). The rows
//! themselves are intact; only the on-disk ART *image* is corrupt
//! (fresh DBs never reproduce the crash, even at 1M rows).
//!
//! # What this does
//!
//! A surgical, opt-in, out-of-serve-loop rebuild:
//!
//!   1. Refuse if an `aberp serve` process holds the tenant DB.
//!   2. Open the DB, dump every row in `seq` order.
//!   3. `verify_chain` the dumped rows — PROVE they are intact (only the
//!      index is corrupt). A broken chain ABORTS (we never rebuild a
//!      tampered ledger).
//!   4. (real run) Take a timestamped `.pre-rebuild-<ts>.bak` backup.
//!   5. In one transaction: `DROP TABLE` (destroys the corrupt ART),
//!      `CREATE TABLE` with the IDENTICAL schema (`UNIQUE(seq)` /
//!      `UNIQUE(id)` PRESERVED — never dropped), re-`INSERT` the rows
//!      verbatim (regenerates a clean ART), and append ONE
//!      `AuditLedgerRebuilt` marker as the last row.
//!   6. `COMMIT`, `VACUUM`.
//!   7. Re-`verify_chain` over the rebuilt table — the operator-facing
//!      gate. A post-verify failure loud-fails and points at the backup.
//!
//! `--dry-run` does steps 1–3 plus a non-destructive ART-health probe
//! (it copies the DB to a throwaway file and attempts one append against
//! the COPY, so it reports "ART healthy" vs "ART corrupt" without
//! touching the real ledger) and prints the plan.
//!
//! # The integrity invariant
//!
//! `verify_chain` runs BEFORE (rows intact?) AND AFTER (rebuild
//! preserved the chain?). If either fails, the rebuild aborts/loud-fails
//! — we never ship a rebuild that loses chain verification. This is the
//! same invariant the S335 probe found a persistent connection would
//! break; here it is the load-bearing gate.

use std::path::{Path, PathBuf};
use std::time::Instant;

use aberp_audit_ledger::{
    self as audit_ledger, Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId,
};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use ulid::Ulid;

use crate::audit_payloads::AuditLedgerRebuiltPayload;
use crate::binary_hash;
use crate::cli::AuditRebuildArgs;

/// Outcome of the non-destructive ART-health probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtHealth {
    /// A probe append against a throwaway copy of the DB succeeded —
    /// the ART rebuilds cleanly; no rebuild needed.
    Healthy,
    /// A probe append against the copy returned an error (the DuckDB
    /// ART `InternalException` surfaced as a catchable `Err` in release
    /// builds per S332 §5). Carries the error string for the operator.
    Corrupt(String),
    /// The probe could not be run (e.g. the copy step failed). Best-
    /// effort only — never blocks a real rebuild.
    Unknown(String),
}

/// Structured result of a rebuild (or dry-run), returned by
/// [`rebuild_at`] so tests can assert on it without parsing stdout.
#[derive(Debug, Clone)]
pub struct RebuildReport {
    pub dry_run: bool,
    pub rows_before: u64,
    pub rows_after: u64,
    pub seq_max_before: u64,
    pub seq_max_after: u64,
    pub chain_verified_before: bool,
    pub chain_verified_after: bool,
    pub took_ms: u64,
    pub backup_path: Option<PathBuf>,
    pub art_health: Option<ArtHealth>,
}

/// CLI entry point. Resolves the DB path from `--tenant` (or `--db`
/// override), computes the binary hash, and drives [`rebuild_at`].
pub fn run(args: &AuditRebuildArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "audit_rebuild",
        tenant = %args.tenant,
        dry_run = args.dry_run,
    )
    .entered();

    let tenant = TenantId::new(args.tenant.clone()).ok_or_else(|| {
        anyhow!(
            "--tenant value '{}' is empty or has a null byte",
            args.tenant
        )
    })?;
    let db_path = resolve_db_path(&args.tenant, args.db.as_deref())?;
    if !db_path.exists() {
        return Err(anyhow!(
            "tenant DuckDB file not found at {} — pass --db to override the default \
             ~/.aberp/<tenant>/aberp.duckdb location",
            db_path.display()
        ));
    }

    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;

    let report = rebuild_at(
        &db_path,
        tenant,
        binary_hash_bytes,
        args.dry_run,
        !args.no_backup,
    )?;

    print_report(&db_path, &report);
    Ok(())
}

/// Resolve the tenant DuckDB path: `--db` override if given, else
/// `~/.aberp/<tenant>/aberp.duckdb` (the canonical prod location,
/// matching `first_launch::touchfile_path`'s convention — CLAUDE.md
/// rule 8: one base-path convention across artifacts).
pub fn resolve_db_path(tenant: &str, db_override: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = db_override {
        return Ok(p.to_path_buf());
    }
    let home = std::env::var("HOME").map_err(|_| {
        anyhow!("HOME environment variable not set; cannot resolve ~/.aberp/<tenant>/aberp.duckdb")
    })?;
    Ok(PathBuf::from(home)
        .join(".aberp")
        .join(tenant)
        .join("aberp.duckdb"))
}

/// Testable core. Dumps + verifies the ledger, and (unless `dry_run`)
/// rebuilds the ART in place. `take_backup` is ignored when `dry_run`.
///
/// Returns a [`RebuildReport`] or a loud error. On a real-run error the
/// transaction rolls back (the original file is untouched) and, if a
/// backup was taken, its path is named in the error context.
pub fn rebuild_at(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    dry_run: bool,
    take_backup: bool,
) -> Result<RebuildReport> {
    // 1. Refuse if a serve process holds the DB. lsof is the primary
    //    check; DuckDB's single-writer lock is the backstop (the
    //    write-open below fails loudly if serve holds it).
    guard_no_serve_process(db_path)?;

    // 2. Dump every row in seq order (read path — safe against a corrupt
    //    ART; the crash is on INSERT, not SELECT, per S332 §5).
    let entries = {
        let ledger = Ledger::open_read_only(db_path, tenant.clone(), binary_hash)
            .with_context(|| format!("open audit ledger (read-only) at {}", db_path.display()))?;
        ledger
            .entries()
            .context("dump audit_ledger rows in seq order")?
    };
    let rows_before = entries.len() as u64;
    let seq_max_before = entries.last().map(|e| e.seq.as_u64()).unwrap_or(0);

    // 3. Verify the dumped rows. A broken chain means the DATA is
    //    suspect, not just the index — ABORT (CLAUDE.md rule 12). We
    //    never rebuild a tampered ledger.
    let chain_verified_before = {
        let ledger = Ledger::open_read_only(db_path, tenant.clone(), binary_hash)
            .context("re-open audit ledger (read-only) for pre-rebuild verify")?;
        match ledger.verify_chain() {
            Ok(n) => {
                if n != rows_before {
                    return Err(anyhow!(
                        "pre-rebuild verify walked {n} entries but the dump has {rows_before} \
                         rows — refusing to rebuild on an inconsistent read"
                    ));
                }
                true
            }
            Err(e) => {
                return Err(anyhow!(
                    "pre-rebuild chain verification FAILED ({e}): the audit-ledger ROWS are \
                     not intact, so this is NOT the S332 index-only corruption a rebuild can \
                     safely fix. ABORTING — the ledger data itself is suspect; do not rebuild. \
                     Preserve the file and investigate (the on-disk ART rebuild would faithfully \
                     re-index a tampered chain, masking the tamper)."
                ));
            }
        }
    };

    if dry_run {
        // Non-destructive ART probe on a throwaway copy.
        let art_health = probe_art_health(db_path, &tenant, binary_hash);
        return Ok(RebuildReport {
            dry_run: true,
            rows_before,
            rows_after: rows_before,
            seq_max_before,
            seq_max_after: seq_max_before,
            chain_verified_before,
            chain_verified_after: chain_verified_before,
            took_ms: 0,
            backup_path: None,
            art_health: Some(art_health),
        });
    }

    // 4. Backup before the in-place rebuild (unless explicitly skipped).
    let backup_path = if take_backup {
        Some(take_backup_copy(db_path).context("take pre-rebuild backup")?)
    } else {
        tracing::warn!("--no-backup set: skipping pre-rebuild backup (dangerous)");
        None
    };

    // 5. Rebuild in one transaction: drop (destroy corrupt ART) +
    //    recreate (same schema) + re-insert verbatim + append marker.
    let started = Instant::now();
    let mut conn = Connection::open(db_path).with_context(|| {
        format!(
            "open tenant DuckDB for rebuild at {} (a 'set lock'/'in use' error here means an \
             aberp serve process is still running — stop it first)",
            db_path.display()
        )
    })?;
    let seq_max_after = seq_max_before + 1; // verbatim tail + the marker
    let rows_after = rows_before + 1;
    {
        let tx = conn
            .transaction()
            .context("begin rebuild transaction (DROP + CREATE + reinsert)")?;

        audit_ledger::rebuild_table_in_tx(&tx, &entries)
            .context("rebuild audit_ledger table (drop + create + verbatim reinsert)")?;

        // Append the AuditLedgerRebuilt marker as the LAST row of the
        // same tx. append_in_tx reads the now-rebuilt head (the last
        // verbatim row) and chains the marker onto it.
        let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash);
        let actor = rebuild_actor()?;
        let took_so_far = started.elapsed().as_millis() as u64;
        let payload = AuditLedgerRebuiltPayload::new(
            rows_before,
            rows_after,
            seq_max_before,
            seq_max_after,
            chain_verified_before,
            took_so_far,
        );
        audit_ledger::append_in_tx(
            &tx,
            &ledger_meta,
            EventKind::AuditLedgerRebuilt,
            payload.to_bytes(),
            actor,
            None,
        )
        .context("append AuditLedgerRebuilt marker row")?;

        tx.commit().context("commit rebuild transaction")?;
    }

    // 6. VACUUM (best-effort; reclaims space + re-analyses). Outside the
    //    tx. A VACUUM failure does not undo the committed rebuild, so we
    //    log + continue rather than fail the whole operation.
    if let Err(e) = conn.execute_batch("VACUUM;") {
        tracing::warn!(error = %e, "VACUUM after rebuild failed (non-fatal; rebuild is committed)");
    }
    drop(conn);
    let took_ms = started.elapsed().as_millis() as u64;

    // 7. Post-commit re-verify — the operator-facing integrity gate.
    let ledger = Ledger::open_read_only(db_path, tenant.clone(), binary_hash)
        .context("re-open audit ledger (read-only) for post-rebuild verify")?;
    let rebuilt = ledger
        .entries()
        .context("re-read audit_ledger rows after rebuild")?;
    let actual_rows_after = rebuilt.len() as u64;
    let actual_seq_max_after = rebuilt.last().map(|e| e.seq.as_u64()).unwrap_or(0);
    let chain_verified_after = match ledger.verify_chain() {
        Ok(n) => n == actual_rows_after,
        Err(e) => {
            return Err(anyhow!(
                "POST-rebuild chain verification FAILED ({e}). The rebuild has been committed \
                 but does NOT verify — this must never happen. RESTORE THE BACKUP{} and \
                 investigate. Do not run ABERP against this file.",
                backup_path
                    .as_ref()
                    .map(|p| format!(" at {}", p.display()))
                    .unwrap_or_else(|| " (none was taken: --no-backup)".to_string())
            ));
        }
    };
    if !chain_verified_after
        || actual_rows_after != rows_after
        || actual_seq_max_after != seq_max_after
    {
        return Err(anyhow!(
            "post-rebuild sanity mismatch: expected rows_after={rows_after} \
             seq_max_after={seq_max_after} chain_verified=true, got rows_after={actual_rows_after} \
             seq_max_after={actual_seq_max_after} chain_verified={chain_verified_after}. \
             RESTORE THE BACKUP{}.",
            backup_path
                .as_ref()
                .map(|p| format!(" at {}", p.display()))
                .unwrap_or_else(|| " (none)".to_string())
        ));
    }

    // 7a. Sync the audit-ledger mirror (ADR-0030 §2) to its new head so
    //     the second-source assertion stays consistent. The N verbatim
    //     rows are byte-identical to before, so this only appends the one
    //     new marker.
    let mirror_path = audit_ledger::mirror_path_for(db_path);
    if let Err(e) = ledger.sync_mirror(&mirror_path) {
        tracing::warn!(error = %e, "sync audit-ledger mirror after rebuild failed (non-fatal)");
    }

    Ok(RebuildReport {
        dry_run: false,
        rows_before,
        rows_after,
        seq_max_before,
        seq_max_after,
        chain_verified_before,
        chain_verified_after,
        took_ms,
        backup_path,
        art_health: None,
    })
}

/// Build the local-CLI [`Actor`] for the rebuild marker. Mirrors
/// `mark_abandoned`'s OS-user derivation: a destructive maintenance
/// action must record WHO ran it.
fn rebuild_actor() -> Result<Actor> {
    let session_id = Ulid::new().to_string();
    let os_user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    Ok(Actor::from_local_cli(session_id, &os_user))
}

/// Refuse to rebuild while a serve process holds the DB. Runs `lsof`;
/// if it reports any holder, loud-fail. If `lsof` is unavailable we log
/// and rely on DuckDB's write-lock backstop (the rebuild's
/// `Connection::open` will then fail loudly).
fn guard_no_serve_process(db_path: &Path) -> Result<()> {
    match run_lsof(db_path) {
        Ok(stdout) => guard_from_lsof_output(&stdout),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not run lsof to check for a live serve process; relying on DuckDB's \
                 write-lock as the backstop"
            );
            Ok(())
        }
    }
}

/// Pure decision core for the serve-alive guard. Errors (refuses) iff
/// `lsof_output` lists any process holding the file. Split out so the
/// safety logic is unit-testable with synthetic `lsof -F pc` output
/// (CLAUDE.md rule 9).
fn guard_from_lsof_output(lsof_output: &str) -> Result<()> {
    let holders = holders_from_lsof_fpc(lsof_output);
    if holders.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "audit-rebuild REFUSED: the tenant DuckDB file is held open by {} \
         (likely a running `aberp serve`). Stop ABERP first — a rebuild while serve is live \
         would race the live audit-write path. Holders: {}",
        holders.first().cloned().unwrap_or_default(),
        holders.join(", ")
    ))
}

/// Parse `lsof -F pc <path>` field output into the list of holding
/// command names (the `c…` lines). Empty when nothing holds the file.
fn holders_from_lsof_fpc(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix('c').map(|s| s.to_string()))
        .collect()
}

/// Run `lsof -F pc -- <path>`. Returns the stdout. lsof exits non-zero
/// when nothing holds the file — that is NOT an error here (it means
/// "no holders"), so we return the (empty) stdout regardless of status.
fn run_lsof(db_path: &Path) -> Result<String> {
    let out = std::process::Command::new("lsof")
        .arg("-F")
        .arg("pc")
        .arg("--")
        .arg(db_path)
        .output()
        .context("spawn lsof")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Copy `<db>` (and its `<db>.wal` sidecar, if present) to a
/// timestamped `<db>.pre-rebuild-<unix_ts>.bak`. Returns the backup
/// path. The `.wal` sidecar is copied to `<bak>.wal` so the pair is
/// restorable (the runbook documents the rename-on-restore step).
fn take_backup_copy(db_path: &Path) -> Result<PathBuf> {
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    let mut name = db_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("aberp.duckdb"));
    name.push(format!(".pre-rebuild-{ts}.bak"));
    let backup = db_path.with_file_name(name);
    std::fs::copy(db_path, &backup)
        .with_context(|| format!("copy {} -> {}", db_path.display(), backup.display()))?;

    let wal = wal_sidecar(db_path);
    if wal.exists() {
        let backup_wal = append_ext(&backup, "wal");
        std::fs::copy(&wal, &backup_wal)
            .with_context(|| format!("copy WAL {} -> {}", wal.display(), backup_wal.display()))?;
    }
    tracing::info!(backup = %backup.display(), "pre-rebuild backup taken");
    Ok(backup)
}

/// Non-destructive ART-health probe. Copies the DB (+ WAL) to a
/// throwaway file and attempts ONE audit append against the copy. The
/// copy carries the same on-disk ART bytes, so a corrupt index
/// reproduces the S332 `InternalException` here — surfaced as a
/// catchable `Err` in release builds (S332 §5) — WITHOUT touching the
/// real ledger. The copy is deleted afterward.
fn probe_art_health(db_path: &Path, tenant: &TenantId, binary_hash: BinaryHash) -> ArtHealth {
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    let probe = append_ext(db_path, &format!("art-probe-{ts}"));

    if let Err(e) = std::fs::copy(db_path, &probe) {
        return ArtHealth::Unknown(format!("copy for probe failed: {e}"));
    }
    let wal = wal_sidecar(db_path);
    if wal.exists() {
        let _ = std::fs::copy(&wal, append_ext(&probe, "wal"));
    }

    let result = probe_append(&probe, tenant.clone(), binary_hash);

    // Clean up the throwaway copy + any WAL it produced.
    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(append_ext(&probe, "wal"));

    match result {
        Ok(()) => ArtHealth::Healthy,
        Err(e) => ArtHealth::Corrupt(e.to_string()),
    }
}

/// Attempt one audit append against `probe_path` (a throwaway copy).
/// Ok ⇒ the ART rebuilds cleanly; Err ⇒ the on-disk ART is corrupt.
/// The appended marker is discarded with the copy.
fn probe_append(probe_path: &Path, tenant: TenantId, binary_hash: BinaryHash) -> Result<()> {
    let mut ledger = Ledger::open(probe_path, tenant, binary_hash).context("open probe copy")?;
    let payload = AuditLedgerRebuiltPayload::new(0, 0, 0, 0, false, 0).to_bytes();
    ledger
        .append(
            EventKind::AuditLedgerRebuilt,
            payload,
            rebuild_actor()?,
            None,
        )
        .context("probe append")?;
    Ok(())
}

/// `<db>.wal` — DuckDB's write-ahead-log sidecar path.
fn wal_sidecar(db_path: &Path) -> PathBuf {
    append_ext(db_path, "wal")
}

/// Append `.<ext>` to a path's filename (preserving any existing
/// extension), e.g. `aberp.duckdb` + `wal` → `aberp.duckdb.wal`.
fn append_ext(path: &Path, ext: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".");
    name.push(ext);
    path.with_file_name(name)
}

/// Print the operator-facing summary.
fn print_report(db_path: &Path, report: &RebuildReport) {
    if report.dry_run {
        let health = match &report.art_health {
            Some(ArtHealth::Healthy) => "ART healthy — NO rebuild needed".to_string(),
            Some(ArtHealth::Corrupt(e)) => {
                format!("ART CORRUPT — rebuild recommended (probe error: {e})")
            }
            Some(ArtHealth::Unknown(e)) => format!("ART health UNKNOWN ({e})"),
            None => "ART health not probed".to_string(),
        };
        println!(
            "audit-rebuild DRY-RUN ({db}):\n  rows: {rows}  seq_max: {seq}  \
             chain_verified: {chain}\n  {health}\n  Plan: DROP + CREATE (UNIQUE(seq)/UNIQUE(id) \
             preserved) + reinsert {rows} rows verbatim + 1 AuditLedgerRebuilt marker.\n  \
             Re-run WITHOUT --dry-run to execute (a .pre-rebuild-<ts>.bak backup is taken first).",
            db = db_path.display(),
            rows = report.rows_before,
            seq = report.seq_max_before,
            chain = report.chain_verified_before,
            health = health,
        );
        return;
    }

    println!(
        "audit-rebuild OK ({db}):\n  rows: {rb} -> {ra}  seq_max: {sb} -> {sa}\n  \
         chain verified before: {cvb}, after: {cva}\n  took: {ms} ms\n  backup: {backup}\n  \
         UNIQUE(seq)/UNIQUE(id) preserved; AuditLedgerRebuilt marker appended as the last row.\n  \
         Restart ABERP and confirm a material save now succeeds.",
        db = db_path.display(),
        rb = report.rows_before,
        ra = report.rows_after,
        sb = report.seq_max_before,
        sa = report.seq_max_after,
        cvb = report.chain_verified_before,
        cva = report.chain_verified_after,
        ms = report.took_ms,
        backup = report
            .backup_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(skipped: --no-backup)".to_string()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holders_parsed_from_lsof_fpc_output() {
        // `lsof -F pc` emits p<pid> then c<command> per holder.
        let out = "p4321\ncaberp\np9988\ncaberp\n";
        let holders = holders_from_lsof_fpc(out);
        assert_eq!(holders, vec!["aberp".to_string(), "aberp".to_string()]);
    }

    #[test]
    fn no_holders_when_lsof_output_empty() {
        assert!(holders_from_lsof_fpc("").is_empty());
        // lsof can also print only a header-ish blank; no c-lines ⇒ none.
        assert!(holders_from_lsof_fpc("p123\n").is_empty());
    }

    #[test]
    fn guard_refuses_when_serve_holds_db() {
        // Synthetic "aberp serve holds the file" lsof output ⇒ refuse.
        let out = "p4321\ncaberp\n";
        let err = guard_from_lsof_output(out).expect_err("must refuse when a holder is present");
        let msg = err.to_string();
        assert!(msg.contains("REFUSED"), "got: {msg}");
        assert!(msg.contains("aberp"), "got: {msg}");
    }

    #[test]
    fn guard_allows_when_nothing_holds_db() {
        guard_from_lsof_output("").expect("no holders ⇒ allow");
    }

    #[test]
    fn append_ext_preserves_existing_extension() {
        let p = Path::new("/tmp/aberp.duckdb");
        assert_eq!(append_ext(p, "wal"), Path::new("/tmp/aberp.duckdb.wal"));
        assert_eq!(
            append_ext(p, "art-probe-7"),
            Path::new("/tmp/aberp.duckdb.art-probe-7")
        );
    }

    #[test]
    fn resolve_db_path_prefers_override() {
        let p = PathBuf::from("/custom/x.duckdb");
        assert_eq!(resolve_db_path("prod", Some(&p)).unwrap(), p);
    }
}
