//! Snapshot operations: take (EXPORT), validate (IMPORT + smoke), restore.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{
    ensure_consistent_with_db, mirror_path_for, AppendError, BinaryHash, Ledger, TenantId,
};
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
    /// ADR-0116 D4 — rows in `audit_ledger_anchors`. `-1` == NOT RECORDED
    /// (the table was unreadable, or validation never got that far), never
    /// "zero anchors". See [`crate::SnapshotMeta::anchor_count`].
    pub anchor_count: i64,
    /// ADR-0116 D4 — highest `audit_ledger` seq covered by a VERIFIED anchor.
    /// `None` == not recorded; `Some(0)` == recorded, nothing anchored.
    pub anchored_through_seq: Option<u64>,
}

/// Single-quote a path for embedding in a DuckDB SQL string, doubling any
/// embedded single quote. Tenant DB paths never contain quotes in
/// practice, but escaping is cheap and removes the foot-gun.
pub(crate) fn sql_quote(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\'', "''");
    format!("'{s}'")
}

/// Hex SHA-256 of a file's bytes. Reads the whole file into memory — fine
/// at tenant scale (S393 `copy_atomic` does the same).
pub(crate) fn sha256_file(path: &Path) -> Result<String> {
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
///
/// # ADR-0116 D4 — anchors are RECORDED, never gating
///
/// `EXPORT DATABASE` captures all tables, so `audit_ledger_anchors` rides
/// along automatically — but nothing read it, so a snapshot could be marked
/// `valid=true` on hash-chain grounds while carrying zero or truncated anchor
/// coverage, and a DB restored from it would silently lose the eIDAS
/// Art. 41(2) presumption ADR-0087 says must survive a restore.
///
/// This now reads the anchors (through `audit-ledger`'s own API — never raw
/// SQL here, so the table stays owned by the crate that defines it and
/// `[[no-sql-specific]]` holds) and records coverage. **A missing or short
/// anchor set does NOT fail validation**, and the reason is not a preference:
/// every `audit_ledger_anchors.parquet` in both live stores is exactly 300
/// bytes, consistent with zero anchor rows everywhere. A hard gate would mark
/// EVERY existing snapshot invalid — and `plan_retention` prunes invalid
/// snapshots, so a hard gate would not merely fail validation, it would
/// delete the entire rollback store on the next cycle. That is a durability
/// regression in service of a legal property. The sanction lives at RESTORE
/// time instead (`--accept-unanchored`), where the legal claim is made.
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
                anchor_count: -1,
                anchored_through_seq: None,
            }
        }
    };

    let conn = match Connection::open_in_memory() {
        Ok(c) => c,
        Err(e) => return fail(format!("open in-memory validation db: {e}")),
    };

    if let Err(e) = conn.execute_batch(&format!("IMPORT DATABASE {};", sql_quote(export_dir))) {
        return fail(format!(
            "IMPORT DATABASE failed (corrupt/incomplete export): {e}"
        ));
    }

    // invoice: informational, table may be absent on a fresh tenant.
    let invoice_count: i64 = conn
        .query_row("SELECT count(*) FROM invoice", [], |r| r.get(0))
        .unwrap_or(-1);

    // audit_ledger: hard gate — must be present.
    let audit_count: i64 =
        match conn.query_row("SELECT count(*) FROM audit_ledger", [], |r| r.get(0)) {
            Ok(n) => n,
            Err(e) => return fail(format!("audit_ledger unreadable in snapshot: {e}")),
        };

    // Verify the hash chain on the imported connection WITHOUT re-opening a
    // file (S375). Binary hash is irrelevant to chain verification (which
    // checks prev/entry hashes against the tenant genesis), so a zero hash
    // is fine here.
    let ledger = Ledger::from_connection(conn, tenant_id, BinaryHash::from_bytes([0u8; 32]));

    // ADR-0116 D4 — anchor coverage, recorded before the chain verdict so it
    // is reported even when the chain fails (a broken chain is exactly when an
    // operator wants to know what the snapshot could still prove).
    let (anchor_count, anchored_through_seq) = anchor_coverage(&ledger);

    match ledger.verify_chain() {
        Ok(chain_len) => ValidationReport {
            ok: true,
            invoice_count,
            audit_count,
            chain_len,
            error: None,
            anchor_count,
            anchored_through_seq,
        },
        Err(e) => ValidationReport {
            ok: false,
            invoice_count,
            audit_count,
            chain_len: 0,
            error: Some(format!("hash-chain verification failed: {e}")),
            anchor_count,
            anchored_through_seq,
        },
    }
}

/// ADR-0116 D4 — how much of this snapshot's chain is covered by a **verified**
/// anchor.
///
/// Returns `(anchor_count, anchored_through_seq)`. On any read failure it
/// returns the **not-recorded** pair `(-1, None)` — never `(0, Some(0))`,
/// which would claim the snapshot was checked and found to carry nothing.
///
/// "Verified" here means two things, both checkable from the snapshot's own
/// bytes with no network:
///
///   1. the anchor actually carries a timestamp token (`tsa_status` is
///      `Anchored`; a `Pending` row is a queued anchor that proves nothing —
///      ADR-0087 never blocks the chain on the TSA, so pending rows are
///      normal and must not be counted as coverage), and
///   2. its `chain_head_hash_at_anchor` resolves to an entry present in
///      **this snapshot's** chain — an anchor over a head the snapshot does
///      not contain covers nothing the snapshot can produce.
///
/// **Honest scope, stated because a reviewer would otherwise assume more:**
/// this does NOT cryptographically verify the RFC-3161 token against the
/// TSA's certificate chain. That needs the authority's trust anchors and is
/// an operational question (ADR-0116 Phase 3), not something a snapshot
/// validator can answer offline. What is verified is that the anchor points
/// at a head this snapshot really has.
fn anchor_coverage(ledger: &Ledger) -> (i64, Option<u64>) {
    let anchors = match ledger.anchors() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ADR-0116 D4 — audit_ledger_anchors unreadable in this snapshot; recording                  anchor coverage as NOT RECORDED (-1), never as zero"
            );
            return (-1, None);
        }
    };
    let entries = match ledger.entries() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ADR-0116 D4 — snapshot entries unreadable while verifying anchors; recording                  anchor coverage as NOT RECORDED (-1)"
            );
            return (-1, None);
        }
    };
    // seq by entry-hash, so an anchor's claimed head resolves to a position.
    let mut seq_by_hash: std::collections::HashMap<String, u64> =
        std::collections::HashMap::with_capacity(entries.len());
    for e in &entries {
        seq_by_hash.insert(hex::encode(e.entry_hash.as_bytes()), e.seq.0);
    }
    let mut through: Option<u64> = None;
    for a in &anchors {
        if a.tsa_status != aberp_audit_ledger::session::anchors::TsaStatus::Anchored {
            continue;
        }
        if let Some(&seq) = seq_by_hash.get(&a.chain_head_hash_at_anchor) {
            through = Some(through.map_or(seq, |cur: u64| cur.max(seq)));
        }
    }
    // The table WAS readable, so the count is recorded even when it is zero,
    // and `anchored_through_seq` is `Some(0)` — "checked, nothing anchored" —
    // rather than `None`, which means "never checked".
    (anchors.len() as i64, Some(through.unwrap_or(0)))
}

fn fail(msg: String) -> ValidationReport {
    ValidationReport {
        ok: false,
        invoice_count: -1,
        audit_count: -1,
        chain_len: 0,
        error: Some(msg),
        anchor_count: -1,
        anchored_through_seq: None,
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
/// ADR-0099 R2 — who reconciles the audit MIRROR before the EXPORT.
///
/// The pre-snapshot reconcile ([`ensure_consistent_with_db`]) is a WRITER of the
/// audit mirror, and the mirror is half of the audit ledger. Running it on this
/// module's own short-lived `Connection::open` made it a SECOND audit writer
/// inside `aberp serve`, on a connection that is not the shared instance — and
/// a separate DuckDB instance does not replay the live writer's WAL, so its
/// `db_max_seq` reads STALE-LOW while the lockstep `sync_mirror` has already
/// carried the mirror to the true head. The reconciler then sees
/// `mirror_max > db_max` and fires a spurious `MirrorAheadOfDb` — preserving a
/// side file and raising a P0 signal on a perfectly healthy pair.
///
/// The "sanctioned residual" rationale below justified this connection as
/// READ-ONLY *with respect to the live DB*. That is true and was never the
/// question: nobody asked whether it was read-only with respect to the MIRROR.
/// It is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorReconcile {
    /// `aberp serve`: the caller has ALREADY reconciled the mirror through the
    /// ONE shared `aberp_db::Handle` writer, on the coherent shared instance.
    /// This module must not touch the mirror at all.
    AlreadyDoneByCaller,
    /// Separate-process CLI one-shot (`aberp snapshot now`): no shared Handle
    /// exists, and the export connection is the only opener in the process, so
    /// reconciling on it is coherent and cannot race anything.
    OnExportConnection,
}

pub fn take_snapshot(
    db_path: &Path,
    store_dir: &Path,
    tenant: &str,
    now: OffsetDateTime,
) -> Result<SnapshotRecord> {
    take_snapshot_with(
        db_path,
        store_dir,
        tenant,
        now,
        MirrorReconcile::OnExportConnection,
    )
}

/// [`take_snapshot`] with the mirror-reconcile owner made explicit. See
/// [`MirrorReconcile`].
pub fn take_snapshot_with(
    db_path: &Path,
    store_dir: &Path,
    tenant: &str,
    now: OffsetDateTime,
    reconcile: MirrorReconcile,
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

    // EXPORT runs against the live DB via its OWN short-lived connection.
    //
    // ADR-0098 Session C — SANCTIONED RESIDUAL (gate allow-listed; FLAGGED).
    // Post-Session-C this is the ONE remaining live-tenant-DB opener outside
    // the shared `aberp_db::Handle`. It is retained deliberately, NOT migrated,
    // for three reasons:
    //   1. It is a LOGICAL operation that never writes the LIVE DB FILE —
    //      `EXPORT DATABASE ... PARQUET` is a table scan, and it never touches
    //      the ART/checkpoint metadata path that is the `duckdb#23046`
    //      corruption locus (the 17:02 re-tear came from concurrent CHECKPOINT
    //      actors, not logical read scans).
    //
    //      ADR-0116 D1 — THIS CLAIM USED TO SAY "READ-ONLY", FULL STOP. It is
    //      read-only with respect to the live DB and that is all it ever was.
    //      The preceding `ensure_consistent_with_db` step is NOT read-only: it
    //      can TRIM THE LIVE AUDIT MIRROR IN PLACE (the torn-tail branch
    //      preserves the original and truncates the file) and MINT EVIDENCE
    //      ARTEFACTS (`preserve_ahead_mirror` -> the `.ahead-<nanos>.bak` /
    //      `AHEAD-*` files found on disk). So on the CLI arm this performs
    //      **audit-mirror recovery surgery**, outside the boot recovery path
    //      that is supposed to own it, on a best-effort/log-and-continue basis.
    //
    //      That is not a write to `audit_ledger` and it is NOT the seq-515 fork
    //      shape — every branch of `ensure_consistent_with_db` was traced and
    //      it never tops the DB up from the mirror — but it is not "read-only"
    //      either, and the comment that said so has been corrected rather than
    //      left to mislead the next reader. In `aberp serve` the reconcile is
    //      hoisted onto the shared Handle (ADR-0099 R2, `MirrorReconcile`), so
    //      the surgery-on-a-timer shape survives only on the separate-process
    //      CLI arm, where it is the sole opener.
    //   2. It runs at the snapshot daemon's LOW cadence (default 4 h) — the
    //      lowest-frequency opener in the process, versus the ~2 s email-relay
    //      drain that drove the incident.
    //   3. The alternative — routing it through a Handle quiesce-around-EXPORT
    //      (the pattern B used for `durable_checkpoint`) — would (a) add a new
    //      public Handle API, and (b) hold the single writer mutex for the
    //      ENTIRE multi-second EXPORT every cycle, freezing all request-handler
    //      writes. That availability cost + the un-sandbox-compilable change to
    //      the durability core is the LESS conservative option; it is FLAGGED
    //      for the adversarial review as the path to full closure if wanted.
    // The gate's CHECK 10 carries this file on its explicit allow-list.
    // NOTE: the pre-ADR-0098 comment here asserted "DuckDB shares one instance
    // per process" — that assumption is what the re-tear DISPROVED; corrected.
    {
        let conn = Connection::open(db_path)?;
        // ADR-0098 C2 (review F6) — disable DuckDB's implicit checkpoint-on-
        // close on THIS connection too. The EXPORT is a logical read-only table
        // scan, but dropping a plain read-write connection triggers DuckDB's
        // implicit close-checkpoint, which folds the on-disk WAL IN PLACE — the
        // precise `duckdb#23046` write locus the "sanctioned residual"
        // rationale (reason #1) had overlooked. With the pragma set, this
        // connection's drop touches the live file no more than the Handle's own
        // runtime connections do, making reason #1 ("never touches the
        // ART/checkpoint metadata locus") actually true on the drop path.
        // (Pragma spelling validated for libduckdb 1.5+, same as aberp-db's
        // Handle; an unknown pragma errors hard — a future rename surfaces
        // loudly, never silently. FLAGGED CI/Mac-gated.)
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")?;

        // ADR-0098 Part 2b (Gap 2b) — reconcile + fsync the audit-ledger
        // mirror to the live DB BEFORE the EXPORT, on this SAME connection (no
        // re-open — S375). Afterwards `mirror_head == db_head`, so the
        // snapshot's `audit_count` can never exceed the durable mirror head
        // (`snapshot.audit_count <= mirror_head` by construction) and the
        // recovery guard's ahead-snapshot branch (Gap 2a) becomes unreachable
        // going forward. Best-effort: a mirror-AHEAD-of-DB condition (the P0
        // owned by boot `ensure_consistent_with_db`) and any other reconcile
        // error are SURFACED, never fatal — the EXPORT of the live DB is
        // independently valuable and Gap 2a remains the safety net.
        let mirror_path = mirror_path_for(db_path);
        // ADR-0099 R2 — SKIPPED when the caller already reconciled through the
        // shared Handle. See [`MirrorReconcile`] for why this connection must
        // not be the one that writes the mirror inside `aberp serve`.
        match reconcile {
            MirrorReconcile::AlreadyDoneByCaller => tracing::debug!(
                mirror = %mirror_path.display(),
                "ADR-0099 R2 — pre-snapshot mirror reconcile SKIPPED here; the caller \
                 already ran it through the shared aberp_db::Handle writer"
            ),
            MirrorReconcile::OnExportConnection => {
                match ensure_consistent_with_db(&conn, &mirror_path) {
                    Ok(action) => tracing::debug!(
                        ?action,
                        mirror = %mirror_path.display(),
                        "ADR-0098 2b — pre-snapshot mirror reconcile + fsync"
                    ),
                    Err(AppendError::MirrorAheadOfDb {
                        mirror_max_seq,
                        db_max_seq,
                        preserved,
                    }) => tracing::warn!(
                        mirror_max_seq,
                        db_max_seq,
                        preserved,
                        "ADR-0098 2b — pre-snapshot mirror is AHEAD of the DB; preserved + \
                         surfaced (boot recovery owns the ahead-mirror P0). Taking the \
                         snapshot anyway"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        mirror = %mirror_path.display(),
                        "ADR-0098 2b — pre-snapshot mirror reconcile failed (best-effort); \
                         taking the snapshot anyway"
                    ),
                }
            }
        }

        conn.execute_batch(&format!(
            "EXPORT DATABASE {} (FORMAT PARQUET);",
            sql_quote(&partial_dir)
        ))?;
    }

    let report = validate_export(&partial_dir, tenant);
    let byte_size = dir_size(&partial_dir)?;

    let meta = SnapshotMeta {
        meta_version: crate::META_VERSION_CURRENT,
        seq,
        created_at: now,
        source_db_sha256,
        byte_size,
        valid: report.ok,
        invoice_count: report.invoice_count,
        audit_count: report.audit_count,
        chain_len: report.chain_len,
        validation_error: report.error,
        anchor_count: report.anchor_count,
        anchored_through_seq: report.anchored_through_seq,
    };
    write_meta(&partial_dir, &meta)?;

    // ── ADR-0116 D1 / F6 — make the snapshot DURABLE before it is visible ──
    //
    // This path had **no `fsync` at all**: `EXPORT DATABASE` wrote the parquet
    // files, `schema.sql`, `load.sql` and `meta.json` through the page cache,
    // and the finalize rename made the directory visible immediately. After a
    // power cut a snapshot directory could therefore be present and complete
    // by name while its parquet bytes had never reached the device — and
    // `meta.json` would still say `valid: true`, because validation ran
    // against the page cache that the crash discarded.
    //
    // That is the exact defect D3.1 fixed on the RESTORE install path, on the
    // argument that "the path whose entire job is producing a trustworthy
    // database after a durability incident was itself not crash-safe". It is
    // sharper here: this is the path that PRODUCES the artefact the whole
    // feature exists to create, and the one event most likely to make you need
    // a snapshot is the one that can tear it.
    //
    // The recipe is `crash_safe::atomic_install`'s, generalised to a directory:
    // every file durable, then the directory that indexes them, then the
    // rename, then the store directory that indexes THAT. Ordering is the
    // whole point — a rename made durable before its contents would publish a
    // name for bytes that are not there.
    fsync_export_dir(&partial_dir)?;

    // Atomic finalize: the snapshot only becomes visible to listing/seq
    // derivation once it is whole.
    std::fs::rename(&partial_dir, &final_dir).map_err(|e| SnapshotError::io(&final_dir, e))?;
    crate::crash_safe::fsync_dir(store_dir)?;

    Ok(SnapshotRecord {
        dir: final_dir,
        meta,
    })
}

/// `fsync` every regular file directly inside `dir`, then `dir` itself.
///
/// The export is flat (one parquet per table + `schema.sql` + `load.sql` +
/// `meta.json`), so a single non-recursive pass covers it — the same shape
/// [`dir_size`] already assumes. A file that vanishes between the `read_dir`
/// and the open is not an error: nothing else writes into a `.partial` we own.
fn fsync_export_dir(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(|e| SnapshotError::io(dir, e))? {
        let entry = entry.map_err(|e| SnapshotError::io(dir, e))?;
        let meta = entry.metadata().map_err(|e| SnapshotError::io(dir, e))?;
        if meta.is_file() {
            crate::crash_safe::fsync_file(&entry.path())?;
        }
    }
    crate::crash_safe::fsync_dir(dir)
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
    // Chunk 3 / ADR-0093 — explicit FROZEN-prod refusal FIRST, with a
    // prod-named message: a restore can never target prod's DB root or
    // prod's snapshot store, however the path arrived.
    ensure_not_prod_path(&abs)?;
    // ADR-0082 — never restore directly onto ANY live `~/.aberp*` tenant
    // home (prod OR this edition's own): restore to a side path, stop
    // `aberp serve`, then swap the file in. Intentional friction on the one
    // irreversible operation.
    if path_is_under_live_db_home(&abs) {
        return Err(SnapshotError::RestoreRefused(format!(
            "target {} is under a live ~/.aberp* tenant home — restore to a side path, \
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

/// Path component naming the FROZEN prod line's live DB root
/// (`~/.aberp/…`, including `~/.aberp/prod/aberp.duckdb`).
const PROD_DB_ROOT_COMPONENT: &str = ".aberp";
/// Path component naming the FROZEN prod line's snapshot store
/// (`~/Documents/ABERP-snapshots/…`). The edition stores
/// `ABERP-snapshots-defense` / `-portable` are DIFFERENT components.
const PROD_SNAPSHOT_STORE_COMPONENT: &str = "ABERP-snapshots";

/// True if any component of `path` equals `name` exactly.
fn path_has_component(path: &Path, name: &str) -> bool {
    path.components().any(|c| c.as_os_str() == name)
}

/// True if any component of `path` starts with `.aberp` — i.e. it lives
/// under SOME live DB home: prod's `.aberp`, or an edition's
/// `.aberp-defense` / `.aberp-portable`. Broadened in chunk 3 from the
/// prod-only check so an editions build also refuses to restore directly
/// onto its OWN live tenant DB (ADR-0082: restore to a side path, then
/// swap — never clobber a live file in place).
fn path_is_under_live_db_home(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.starts_with(".aberp"))
    })
}

/// Refuse any path that belongs to the FROZEN PROD line (ADR-0093). This is
/// the mechanical guarantee that an editions build can never snapshot, list,
/// prune, or restore prod — called by the binary on every snapshot source
/// DB, store dir, and restore target. Two prod surfaces are refused:
///
///   - **prod's live DB root** — any path under `~/.aberp/` (a component
///     exactly `.aberp`), which includes `~/.aberp/prod/aberp.duckdb`. The
///     edition roots `.aberp-defense` / `.aberp-portable` are different
///     components and are NOT refused here.
///   - **prod's snapshot store** — any path under
///     `~/Documents/ABERP-snapshots/` (a component exactly
///     `ABERP-snapshots`). The edition stores `ABERP-snapshots-defense` /
///     `-portable` are different components and are allowed.
///
/// Pure and total, so it is cheap to call on every operation.
pub fn ensure_not_prod_path(path: &Path) -> Result<()> {
    let abs = absolutise(path);
    if path_has_component(&abs, PROD_DB_ROOT_COMPONENT) {
        return Err(SnapshotError::RestoreRefused(format!(
            "path {} is under the FROZEN prod DB root ~/.aberp/ — an editions build must \
             never read, snapshot, or restore the prod line. \
             Magyarul: az éles ~/.aberp/ tilos az editions buildnek.",
            abs.display()
        )));
    }
    if path_has_component(&abs, PROD_SNAPSHOT_STORE_COMPONENT) {
        return Err(SnapshotError::RestoreRefused(format!(
            "path {} is under prod's snapshot store ~/Documents/{}/ — an editions build \
             snapshots only to its own ~/Documents/ABERP-snapshots-<edition>/ store \
             (ADR-0093).",
            abs.display(),
            PROD_SNAPSHOT_STORE_COMPONENT
        )));
    }
    Ok(())
}

/// Restore a snapshot directory into `target` via `IMPORT DATABASE`, then
/// install it **crash-safely** over the target.
///
/// Refuses to restore from an export that does not itself validate (we never
/// rebuild a DB from a corrupt snapshot). Builds into a sibling `*.restoring`
/// file, checkpoints it so it is self-contained, then hands it to
/// [`crate::atomic_install`]. **Does not** enforce the prod-overwrite guard —
/// callers MUST call [`ensure_restore_allowed`] first (the CLI does).
///
/// # ADR-0116 D3.1 / G2 — why this changed, and why the WAL goes first
///
/// This was the ONE file-install path in the tree with **no `fsync` at all**
/// (`grep -c sync_all take.rs` → 0), while its sibling `atomic_install` did
/// the full durable recipe. The path whose entire job is *producing a
/// trustworthy database after a durability incident* was itself not
/// crash-safe — and it has been used in anger twice, for the 2026-08-03 and
/// 2026-08-08 recoveries.
///
/// Routing through `atomic_install` fixes the fsyncs but leaves one window,
/// which the ADR's first draft claimed (wrongly) was closed by an
/// `install-intent` journal. It is not: `write_install_intent` has exactly one
/// non-test caller, inside `durable_checkpoint`, and `resume_pending_install`
/// is keyed on the LIVE db path, so nothing would ever resume an intent left
/// beside a side-path restore target. The window is:
///
/// ```text
/// atomic_install:  fsync(staged)
///                  rename(staged -> target)     <- new file is now visible
///                  remove target's stale WAL    <- crash HERE: new file + OLD WAL
///                  fsync(parent dir)
/// ```
///
/// and an old WAL beside a fresh self-contained file is the corruption vector
/// this function's own comment has always warned about.
///
/// **The fix is simpler than a journal, not more complex.** A restore is
/// destroying the target *by definition*, so there is no reason to preserve
/// its WAL past the point of no return — unlike `durable_checkpoint`, where
/// the target is the live DB and must survive an aborted swap. So:
/// **delete `<target>.wal` FIRST, then install.** `atomic_install`'s own WAL
/// step becomes a no-op and the window disappears with no journal and no
/// resume path.
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

    // ── ADR-0116 D3.1 — the target's stale WAL dies BEFORE the rename ──
    //
    // Ordering is the whole point. `atomic_install` removes it AFTER the
    // rename, which leaves a crash window in which the new file is visible
    // beside the old WAL. Doing it here closes that window with no journal:
    // after this line there is no WAL for a crash to orphan, and
    // `atomic_install`'s own removal step is a proven no-op.
    let target_wal = wal_sibling(target);
    if target_wal.exists() {
        std::fs::remove_file(&target_wal).map_err(|e| SnapshotError::io(&target_wal, e))?;
    }

    // fsync(staged) -> atomic rename -> (no-op WAL drop) -> fsync(parent dir).
    // The same primitive every other file install in the tree commits with.
    crate::crash_safe::atomic_install(&staging, target)?;
    Ok(())
}

/// The infix every preserved-unit name carries: `<db>.PRE-RESTORE-<tag>`.
///
/// **ADR-0116 rev 4 / F1 — one constant, two readers.** `restore_in_place`
/// WRITES this name and [`find_pre_restore_units`] READS it, and the boot path
/// refuses to provision over a unit it finds. A rename that touched only the
/// writer would leave the detector matching nothing and the refusal silently
/// gone — the "a public rename blinded the name-keyed gate" class this tree has
/// already paid for twice (ADR-0099 round 6, PR #41). Spelling it once makes
/// that failure impossible rather than merely unlikely.
pub const PRE_RESTORE_INFIX: &str = "PRE-RESTORE-";

/// **ADR-0116 rev 4 / F1** — the `.PRE-RESTORE-<tag>` units sitting beside
/// `db_path`, deduplicated to one path per unit and sorted by name.
///
/// # What this is for
///
/// [`restore_in_place`] moves the live DB aside as step 2 and only installs the
/// replacement in step 3. Between the two the live path holds **nothing**, and
/// that window is seconds on a fixture and minutes on a real tenant. A `^C`, an
/// OOM kill or a power cut inside it leaves exactly this on disk:
///
/// ```text
///   aberp.duckdb.PRE-RESTORE-20260829T170712Z            <- intact, complete
///   aberp.duckdb.PRE-RESTORE-20260829T170712Z.wal
///   aberp.duckdb.PRE-RESTORE-20260829T170712Z.audit.log
///   (no aberp.duckdb, no aberp.duckdb.wal, no mirror)
/// ```
///
/// Until rev 3 the mirror stayed at the live path, so the next boot met an
/// AHEAD mirror and REFUSED — loudly, and correctly. Moving the mirror into the
/// unit (the rev-2 blocker fix) closed the boot-after-rollback failure and, in
/// the same stroke, **deleted the only detector of an interrupted restore**:
/// boot now finds an absent DB and an absent mirror, which is byte-for-byte
/// what a first launch looks like, and provisions a fresh EMPTY tenant over a
/// half-done restore with nothing louder than an `INFO` line.
///
/// The preserved unit is the marker, and it is the right one: it cannot be lost
/// independently of the thing it describes, which is the objection that sank
/// the alternative (a separate rollback-marker file — a second source of truth
/// that can be lost, forged, or left behind).
///
/// # Precision, in both directions
///
/// - after a **successful** restore the live DB exists, so no caller asks;
/// - a genuine **first launch** has no `.PRE-RESTORE-` sibling, so this returns
///   empty and provisioning proceeds untouched.
///
/// Siblings inside a unit carry a further suffix after the tag (`.wal`,
/// `.audit.log`, `.ckpt-ok`), so they are folded onto their unit rather than
/// reported as four findings. An unreadable directory returns empty: this is a
/// guard that ADDS a refusal, and it must never be the reason a boot fails.
pub fn find_pre_restore_units(db_path: &Path) -> Vec<PathBuf> {
    let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Vec::new();
    };
    let Some(db_name) = db_path.file_name() else {
        return Vec::new();
    };
    let prefix = format!("{}.{PRE_RESTORE_INFIX}", db_name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut units: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let tag = name.strip_prefix(&prefix)?.split('.').next()?.to_string();
            if tag.is_empty() {
                return None;
            }
            Some(parent.join(format!("{prefix}{tag}")))
        })
        .collect();
    units.sort();
    units.dedup();
    units
}

/// What [`restore_in_place`] moved aside, so the caller can tell the operator
/// exactly what to look at if anything went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedUnit {
    /// `<db>.PRE-RESTORE-<tag>` — the original database file.
    pub db: PathBuf,
    /// `<db>.wal.PRE-RESTORE-<tag>`, when the live DB had a WAL.
    pub wal: Option<PathBuf>,
    /// `<db>.ckpt-ok.PRE-RESTORE-<tag>`, when a marker existed.
    pub ckpt_ok: Option<PathBuf>,
    /// **ADR-0116 D3.4 rev 2** — `<db>.PRE-RESTORE-<tag>.audit.log`, the audit
    /// mirror as it stood before the rollback, when one existed.
    ///
    /// Named off the PRESERVED path (like the WAL and the marker), so
    /// `mirror_path_for(&preserved.db)` finds it and the preserved unit is a
    /// complete, self-consistent DB+WAL+marker+mirror group.
    pub mirror: Option<PathBuf>,
    /// The `PRE-RESTORE-<tag>` suffix itself.
    pub tag: String,
}

/// Outcome of a successful [`restore_in_place`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InPlaceRestoreReport {
    /// The `.PRE-RESTORE-` unit the previous database was moved into.
    pub preserved: PreservedUnit,
    /// **ADR-0116 F2** — validation of the **installed database file**,
    /// re-read from `db_path` after the install.
    ///
    /// This used to be a second call to `validate_export(export_dir)` — a pure
    /// function of the export directory that never mentions `db_path` — so the
    /// line the CLI prints as `re-verified` was byte-identical to the
    /// pre-install validation and still reported `ok` after the installed file
    /// was overwritten with garbage. It now opens the file that is actually on
    /// disk. See [`validate_installed_db`].
    pub installed: ValidationReport,
    /// **ADR-0116 D3.4 rev 2** — entries written into the FRESH mirror built
    /// from the restored chain, or `None` if that rebuild failed (in which
    /// case the live path has no mirror and the next boot creates one).
    pub mirror_entries_written: Option<u64>,
}

/// **ADR-0116 D3.4 — the guarded in-place restore that replaces the hand-swap.**
///
/// The documented recovery procedure today is *restore to a side path → stop
/// serve → swap the file in by hand*. That hand-swap is precisely the
/// per-incident manual step this programme set out to eliminate, and it is
/// unjournalled: a crash mid-swap is on the operator, not on the code.
/// `[[trust-code-not-operator]]` is satisfied for the guard and violated for
/// the procedure; this is the closure.
///
/// The sequence, in code:
///
/// 1. the caller has already refused unless serve is stopped and has taken the
///    mandatory pre-restore snapshot (both live in the app layer, which owns
///    the lock and the ledger);
/// 2. **move the current DB aside as `.PRE-RESTORE-<tag>` — as ONE UNIT:
///    `aberp.duckdb` + `aberp.duckdb.wal` + `aberp.duckdb.ckpt-ok` +
///    `aberp.duckdb.audit.log`.**
/// 3. install via [`restore_into`] (WAL deleted first, then `atomic_install`);
/// 4. write a fresh `.ckpt-ok` marker for the installed file;
/// 5. re-verify the INSTALLED FILE (not the export — see [`validate_installed_db`]);
/// 6. write a FRESH mirror matching the restored chain.
///
/// # Why the mirror moves too (the rev-2 blocker)
///
/// Steps 2 and 3 read the other way round until this revision: *"the
/// `.audit.log` mirror does NOT move. It is the durable record and stays at
/// the live path."* That rule is right in general and **wrong once the
/// operator has acknowledged that the mirror's tail is not to be replayed**,
/// and leaving it in place made this command's headline case a no-op across a
/// restart:
///
/// After a BACKWARDS rollback the mirror still holds the tail the operator
/// discarded, and the app layer then appends `SnapshotRestored` at the next
/// seq on the RESTORED chain — a seq the mirror already holds with a different
/// entry. At the next boot `ensure_consistent_with_db` reports
/// `MirrorDivergedFromDb`, `serve.rs::boot_mirror_route` classifies that as
/// `RefuseFatal`, and **`aberp serve` does not start**. The only way out was
/// the hand-reconciliation this command exists to eliminate. Reproduced end to
/// end through the shipped binary (adversarial review §1, `adv_b6`).
///
/// The premise the old rule rested on has also changed: `boot_mirror_route`'s
/// comment says *"a mirror that disagrees with the DB is the fingerprint of a
/// torn-write / lost DB commit"*. Since D3.4 it is **also** the fingerprint of
/// a deliberate, acknowledged rollback, and the two are indistinguishable on
/// disk. Rather than teach boot to tell them apart from a marker file — a
/// second source of truth that can be lost, forged, or left behind — the
/// restore removes the ambiguity at the point where the decision is actually
/// made: the discarded tail becomes **protected evidence at its new name**
/// (`is_protected_evidence` covers the whole `.PRE-RESTORE-` unit), and boot is
/// left with a DB and a mirror that agree.
///
/// A fresh mirror is written from the restored DB inside this same operation
/// (step 6), so there is no window where the live path carries a database and
/// no durable record of its chain. If that write fails it is logged and NOT
/// fatal: an ABSENT mirror is the one disagreement the boot path resolves
/// safely on its own (`RecoveryAction::Created`).
///
/// # Why the WAL moves with it (F4)
///
/// The ADR's first draft said only "move the current DB aside". That would
/// have been a real defect, twice over. A DB moved without its WAL is
/// **stripped of its un-checkpointed commits** — not a recoverable original,
/// so the "recoverable on every injected failure path" acceptance criterion
/// would have been *unsatisfiable*. And the orphaned `aberp.duckdb.wal` would
/// stay at the live path and pair with the freshly restored file: the exact
/// corruption vector [`restore_into`]'s own comment warns about, reintroduced
/// by the command written to eliminate the hand-swap. Prod's live DB carries a
/// WAL right now; this is not hypothetical.
///
/// # Failure semantics
///
/// Any failure leaves the `.PRE-RESTORE-` unit intact and the original
/// recoverable. If the install fails after the move, the unit is **not**
/// rolled back automatically — an automatic rollback would be a second
/// unjournalled swap at the worst possible moment. The error names the unit;
/// the operator (or `aberp snapshot restore` from the pre-restore snapshot)
/// decides.
pub fn restore_in_place(
    export_dir: &Path,
    db_path: &Path,
    tenant: &str,
    tag: &str,
) -> Result<InPlaceRestoreReport> {
    // Validate BEFORE moving anything aside. A snapshot that cannot restore
    // must not cost the operator a swapped-out live database.
    let pre = validate_export(export_dir, tenant);
    if !pre.ok {
        return Err(SnapshotError::RestoreFromInvalid(
            export_dir.display().to_string(),
        ));
    }
    if !db_path.exists() {
        return Err(SnapshotError::SourceMissing(db_path.to_path_buf()));
    }

    let suffix = format!("{PRE_RESTORE_INFIX}{tag}");
    let db_wal = wal_sibling(db_path);
    let db_ckpt = crate::crash_safe::marker_path(db_path);
    let db_mirror = mirror_path_for(db_path);

    // ── Step 2 — move DB + .wal + .ckpt-ok + .audit.log as ONE unit ─────
    //
    // **The siblings are named off the PRESERVED path, not suffixed in place.**
    // `<db>.PRE-RESTORE-<tag>` + `<db>.PRE-RESTORE-<tag>.wal`, NOT
    // `<db>.wal.PRE-RESTORE-<tag>`. DuckDB finds a WAL by appending `.wal` to
    // the FULL database filename, so the second spelling produces a WAL that
    // pairs with nothing: the preserved unit would look complete on disk and
    // silently open WITHOUT its un-checkpointed commits — which is F4's defect
    // wearing a different mask, and it is not hypothetical (this exact test
    // caught it here).
    //
    // This follows the in-tree ADR-0099 R3 precedent set by
    // `recover::preserve_corrupt_db`, which copies to `wal_sibling(&dest)` for
    // the same reason and states it: "Copying the WAL to `<dest>.wal` keeps the
    // evidence an openable database." The marker follows the same rule via
    // `marker_path`, so `checkpoint_is_current` can be asked about the
    // preserved file directly.
    let preserved_db = suffixed(db_path, &suffix);

    // **Order within the unit matters, and the obvious order is wrong.**
    //
    // The first cut moved the WAL and the marker first, then the DB. If the
    // DB rename then failed, the LIVE database had lost its WAL while
    // remaining live — stripped of every un-checkpointed commit, which is
    // precisely the F4 failure this unit exists to prevent, caused by the
    // preserve step itself. Every `Handle` commit is WAL-only until a
    // checkpoint (proven on duckdb 1.5.3 by ADR-0098 R5), so that is not a
    // narrow window; it is the most recent rows.
    //
    // So the DB moves FIRST — it is the point of no return — and the WAL
    // follows. A failure before the DB rename has moved nothing at all; a
    // failure of the WAL move ROLLS THE DB BACK, restoring the original state
    // exactly, because the reverse rename is the same operation that just
    // succeeded in the forward direction.
    std::fs::rename(db_path, &preserved_db).map_err(|e| SnapshotError::io(&preserved_db, e))?;
    let preserved_wal = match move_aside_to(&db_wal, &wal_sibling(&preserved_db)) {
        Ok(w) => w,
        Err(e) => {
            roll_preserve_back(&preserved_db, db_path, None, "WAL");
            return Err(e);
        }
    };
    // The MIRROR follows the WAL, for the reason the WAL follows the DB: it is
    // moved only once the point of no return is behind us, and a failure here
    // rolls the whole unit back so a failed preserve leaves the live path
    // EXACTLY as it was. The mirror is half the audit ledger — a live database
    // stranded without it, or with it, while the restore aborted would be a
    // torn preserve in the one place the tree cannot tolerate one.
    let preserved_mirror = match move_aside_to(&db_mirror, &mirror_path_for(&preserved_db)) {
        Ok(m) => m,
        Err(e) => {
            roll_preserve_back(
                &preserved_db,
                db_path,
                preserved_wal.as_deref(),
                "audit mirror",
            );
            return Err(e);
        }
    };
    // The marker is regenerable (step 5 writes a fresh one for the installed
    // file regardless), so a failure to move it must not abort a restore that
    // has already passed its point of no return. A stale marker left at the
    // live path is harmless: `checkpoint_is_current` simply returns false.
    let preserved_ckpt =
        match move_aside_to(&db_ckpt, &crate::crash_safe::marker_path(&preserved_db)) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "ADR-0116 D3.4 — could not move the checkpoint marker into the PRE-RESTORE \
                     unit; continuing. The marker is regenerable and a stale one is inert."
                );
                None
            }
        };
    let preserved = PreservedUnit {
        db: preserved_db.clone(),
        wal: preserved_wal,
        ckpt_ok: preserved_ckpt,
        mirror: preserved_mirror,
        tag: suffix.clone(),
    };
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        // The preserved unit must be durable before the install overwrites the
        // live path: same ordering invariant as everywhere else in the tree.
        if let Ok(f) = std::fs::File::open(parent) {
            let _ = f.sync_all();
        }
    }
    tracing::warn!(
        db = %db_path.display(),
        preserved_db = %preserved.db.display(),
        preserved_wal = ?preserved.wal.as_ref().map(|p| p.display().to_string()),
        preserved_ckpt = ?preserved.ckpt_ok.as_ref().map(|p| p.display().to_string()),
        preserved_mirror = ?preserved.mirror.as_ref().map(|p| p.display().to_string()),
        "ADR-0116 D3.4 — live database moved aside as PRE-RESTORE evidence (DB + WAL + \
         checkpoint marker + audit mirror as one unit). The mirror moves WITH the database \
         it belongs to: after an acknowledged backwards rollback its tail is exactly what \
         the operator discarded, and leaving it at the live path made the next `aberp serve` \
         boot refuse with MirrorDivergedFromDb. It is protected evidence at its new name."
    );

    // ── Step 3 — install ────────────────────────────────────────────────
    restore_into(export_dir, db_path, tenant)?;

    // ── Step 4 — a fresh marker for the file that is actually there ─────
    //
    // Without this, the restored file sits beside a marker describing the OLD
    // one. `checkpoint_is_current` then returns false on the SHA mismatch, so
    // the next debounced checkpoint runs — benign, but the restored file's
    // provenance record would be a lie, and provenance is the whole reason the
    // marker exists.
    match crate::crash_safe::write_marker(db_path) {
        Ok(_) => {}
        Err(e) => {
            tracing::error!(
                error = %e,
                db = %db_path.display(),
                "ADR-0116 D3.4 — could not write the post-restore .ckpt-ok marker; the \
                 restored file is installed and durable, but its provenance record is absent. \
                 The next boot will simply checkpoint it."
            );
        }
    }

    // ── Step 5 — re-verify what is now ON DISK, not what we imported ────
    //
    // ADR-0116 F2 — this used to be a second `validate_export(export_dir)`
    // call: the identical pure function of the export directory that produced
    // `pre` twenty lines above, with no reference to `db_path` at all. The
    // numbers the CLI prints as `re-verified` were therefore the SNAPSHOT's,
    // re-derived, and they still came back `ok` after the installed database
    // was overwritten with `b"not a duckdb file at all"`.
    let installed = validate_installed_db(db_path, tenant);
    if !installed.ok {
        return Err(SnapshotError::InstalledVerifyFailed {
            db: db_path.to_path_buf(),
            preserved: preserved.db.display().to_string(),
            detail: installed
                .error
                .unwrap_or_else(|| "installed database failed verification".to_string()),
        });
    }
    // A parity mismatch is as damning as an unopenable file: the install
    // succeeded mechanically but did not produce the snapshot's content.
    if (
        installed.invoice_count,
        installed.audit_count,
        installed.chain_len,
    ) != (pre.invoice_count, pre.audit_count, pre.chain_len)
    {
        return Err(SnapshotError::InstalledVerifyFailed {
            db: db_path.to_path_buf(),
            preserved: preserved.db.display().to_string(),
            detail: format!(
                "the installed database does not match the snapshot it was restored from: \
                 installed invoices={} audit_rows={} chain={} but the snapshot carries \
                 invoices={} audit_rows={} chain={}",
                installed.invoice_count,
                installed.audit_count,
                installed.chain_len,
                pre.invoice_count,
                pre.audit_count,
                pre.chain_len,
            ),
        });
    }

    // ── Step 6 — a FRESH mirror for the RESTORED chain ──────────────────
    //
    // The blocker this revision closes. See the rev-2 note on this function.
    // Best-effort by design: an ABSENT mirror is the ONE disagreement the boot
    // path resolves safely by itself (`RecoveryAction::Created` rebuilds it
    // from the DB), so a failure here degrades to "the next boot writes it",
    // never to "the next boot refuses".
    let mirror_entries_written = match rebuild_mirror_for_restored_db(db_path) {
        Ok(n) => {
            tracing::info!(
                db = %db_path.display(),
                entries = n,
                "ADR-0116 D3.4 — wrote a FRESH audit mirror matching the restored chain; the \
                 pre-rollback mirror is preserved inside the PRE-RESTORE unit"
            );
            Some(n)
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                db = %db_path.display(),
                "ADR-0116 D3.4 — could not write a fresh audit mirror for the restored \
                 database. The restore itself is durable and the live path now has NO mirror, \
                 which the next boot creates from the DB (RecoveryAction::Created). Nothing \
                 needs to be done by hand."
            );
            None
        }
    };

    Ok(InPlaceRestoreReport {
        preserved,
        installed,
        mirror_entries_written,
    })
}

/// Undo a partially-completed preserve so a failure leaves the live path
/// EXACTLY as it was. The reverse renames are the same operations that just
/// succeeded in the forward direction, so they fail only under a filesystem
/// fault — which is why the failure to roll back is the loud one.
fn roll_preserve_back(
    preserved_db: &Path,
    db_path: &Path,
    preserved_wal: Option<&Path>,
    what: &str,
) {
    if let Some(w) = preserved_wal {
        if let Err(e) = std::fs::rename(w, wal_sibling(db_path)) {
            tracing::error!(error = %e, wal = %w.display(), "ADR-0116 D3.4 — could not roll the preserved WAL back");
        }
    }
    if let Err(rollback) = std::fs::rename(preserved_db, db_path) {
        tracing::error!(
            error = %rollback,
            failed_step = what,
            preserved = %preserved_db.display(),
            db = %db_path.display(),
            "ADR-0116 D3.4 — the pre-restore {what} move FAILED and the database could not be \
             rolled back. The live path has no database; the database is at the preserved \
             path. Move it back BY HAND before doing anything else — the unit belongs together."
        );
    }
}

/// **ADR-0116 F2** — validate the database file that is actually at
/// `db_path`, as opposed to the export directory it was built from.
///
/// Same smoke set as [`validate_export`] (invoice count, `audit_ledger`
/// count, ADR-0008 chain verification from genesis, anchor coverage) run
/// against the real file, so an install that produced an unopenable or
/// wrong-content database is caught by the step whose name claims to check it.
///
/// The connection carries `PRAGMA disable_checkpoint_on_shutdown` for the
/// ADR-0098 C2 reason every opener in this tree does: a plain read-write
/// connection's drop folds the on-disk WAL in place. Verification must not be
/// able to write to the thing it verifies.
pub fn validate_installed_db(db_path: &Path, tenant: &str) -> ValidationReport {
    let tenant_id = match TenantId::new(tenant.to_string()) {
        Some(t) => t,
        None => return fail(format!("invalid tenant id {tenant:?}")),
    };
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            return fail(format!(
                "open installed database {}: {e}",
                db_path.display()
            ))
        }
    };
    if let Err(e) = conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;") {
        return fail(format!("installed database is not usable: {e}"));
    }
    let invoice_count: i64 = conn
        .query_row("SELECT count(*) FROM invoice", [], |r| r.get(0))
        .unwrap_or(-1);
    let audit_count: i64 =
        match conn.query_row("SELECT count(*) FROM audit_ledger", [], |r| r.get(0)) {
            Ok(n) => n,
            Err(e) => return fail(format!("audit_ledger unreadable in the installed DB: {e}")),
        };
    let ledger = Ledger::from_connection(conn, tenant_id, BinaryHash::from_bytes([0u8; 32]));
    let (anchor_count, anchored_through_seq) = anchor_coverage(&ledger);
    match ledger.verify_chain() {
        Ok(chain_len) => ValidationReport {
            ok: true,
            invoice_count,
            audit_count,
            chain_len,
            error: None,
            anchor_count,
            anchored_through_seq,
        },
        Err(e) => ValidationReport {
            ok: false,
            invoice_count,
            audit_count,
            chain_len: 0,
            error: Some(format!(
                "hash-chain verification failed on the INSTALLED database: {e}"
            )),
            anchor_count,
            anchored_through_seq,
        },
    }
}

/// **ADR-0116 D3.4 rev 2** — write a fresh `<db>.audit.log` for the restored
/// database, replacing the one that moved into the PRE-RESTORE unit.
///
/// Reaches [`ensure_consistent_with_db`] with NO mirror at the live path, which
/// is its `Created` branch: `rebuild_mirror_from_db_locked` writes
/// `[1..=db_max_seq]` from the DB and `fsync`s the file under its own lock. It
/// is deliberately the same entry point the boot path uses, so the mirror this
/// leaves behind is byte-for-byte the one boot would have produced.
///
/// SANCTIONED NON-SHARED WRITER (cut-gate CHECK 10P residual). This opens the
/// tenant DB independently, but it can only run inside `restore --in-place`,
/// which has already PROVEN `aberp serve` is stopped by taking DuckDB's own
/// exclusive file lock and re-asserting it at the commit point. There is no
/// shared `Handle` in that process to route through.
fn rebuild_mirror_for_restored_db(db_path: &Path) -> Result<u64> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")?;
    let mirror_path = mirror_path_for(db_path);
    match ensure_consistent_with_db(&conn, &mirror_path) {
        Ok(aberp_audit_ledger::RecoveryAction::Created { entries_written }) => Ok(entries_written),
        // Anything other than `Created` means a mirror was already at the live
        // path — which cannot happen after a successful preserve, so it is
        // reported rather than silently accepted.
        Ok(other) => {
            tracing::warn!(
                ?other,
                mirror = %mirror_path.display(),
                "ADR-0116 D3.4 — the post-restore mirror rebuild found an EXISTING mirror at \
                 the live path. The preserve step should have moved it; the reconcile result \
                 is reported as-is."
            );
            Ok(0)
        }
        Err(e) => Err(SnapshotError::MirrorRebuildFailed(e.to_string())),
    }
}

/// `<path>.<suffix>` — appends to the FULL filename, so
/// `aberp.duckdb.wal` becomes `aberp.duckdb.wal.PRE-RESTORE-<tag>` and the
/// unit stays recognisable as a group.
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".");
    os.push(suffix);
    PathBuf::from(os)
}

/// Rename `path` to `dest` when it exists. Returns the new path, or `None`
/// when there was nothing to move (a freshly-checkpointed DB has no WAL, and a
/// never-checkpointed one has no marker — both are normal, neither is an
/// error).
fn move_aside_to(path: &Path, dest: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::rename(path, dest).map_err(|e| SnapshotError::io(dest, e))?;
    Ok(Some(dest.to_path_buf()))
}

/// DuckDB names the WAL by appending `.wal` to the FULL filename (so
/// `x.duckdb` → `x.duckdb.wal`) — NOT `Path::with_extension`.
fn wal_sibling(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_owned();
    os.push(".wal");
    PathBuf::from(os)
}
