//! S426 / ADR-0082 — periodic, validated, logical DuckDB snapshot system.
//!
//! This module is the `apps/aberp` glue around the [`aberp_snapshot`] crate:
//! it resolves the per-tenant snapshot store, takes/validates/prunes
//! snapshots, and **emits the audit events** (`snapshot.created`,
//! `snapshot.validation_failed`, `snapshot.restored`, `snapshot.pruned`)
//! that the crate deliberately does not emit (the crate is decoupled from
//! the ledger). The same shared helpers back three callers:
//!
//!   - the `aberp snapshot {now,list,restore}` CLI (this file's `run_*`),
//!   - the periodic daemon spawned by `aberp serve` ([`run_supervised`]),
//!   - the operator-UI HTTP endpoints in `serve.rs`.
//!
//! ## Why this replaced S393's file-copy panic button
//!
//! S393 copied the live `*.duckdb` file. The 2026-06-11 ART corruption is
//! internal to that file, so a copy copies the corruption. ADR-0082 switches
//! to `EXPORT DATABASE` (logical Parquet), which is corruption-free by
//! construction. The S393 `aberp snapshot` / `aberp restore-snapshot`
//! commands are gone; this is `aberp snapshot now` / `aberp snapshot
//! restore`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_db::HandleArc;
use aberp_snapshot::{
    edition_store_dir, ensure_not_prod_path, ensure_restore_allowed, find_snapshot, list_snapshots,
    plan_retention, prune, restore_into, take_snapshot_with, MirrorReconcile, RetentionPolicy,
    SnapshotRecord,
};

use crate::build_profile;

use crate::audit_payloads::{
    SnapshotCreatedPayload, SnapshotPrunedPayload, SnapshotRestoredPayload,
    SnapshotValidationFailedPayload,
};
use crate::cli::{SnapshotListArgs, SnapshotNowArgs, SnapshotRestoreArgs};

/// Default snapshot cadence: every 4 hours (ADR-0082). Overridable via
/// `ABERP_SNAPSHOT_INTERVAL_SECS`.
const DEFAULT_INTERVAL_SECS: u64 = 4 * 60 * 60;
/// Delay before the first snapshot after boot, so a snapshot never slows
/// `aberp serve` startup.
const BOOT_DELAY_SECS: u64 = 60;

/// Env var that disables the periodic daemon entirely (the manual CLI +
/// HTTP "snapshot now" still work).
pub const POLL_DISABLE_ENV: &str = "ABERP_SNAPSHOT_DISABLE";

/// Env kill-switch for the clean-shutdown durable checkpoint (below).
pub const CHECKPOINT_ON_SHUTDOWN_DISABLE_ENV: &str = "ABERP_CHECKPOINT_ON_SHUTDOWN_DISABLE";

/// How long the clean-shutdown checkpoint will wait for the Handle's writer
/// mutex before giving up and letting the process exit (ADR-0111 / PR #41
/// adversarial finding 2).
///
/// 2 s, chosen against the shutdown budget it runs after: the drain gets 5 s
/// (`shutdown_timeout_from_env`) and DETACHES whatever it could not join, so
/// this is time spent purely on stragglers that already ignored the drain.
/// Long enough to absorb an in-flight transaction; short enough that exit stays
/// prompt for an operator watching a terminal.
const SHUTDOWN_CHECKPOINT_LOCK_BUDGET: Duration = Duration::from_secs(2);

/// ADR-0082 follow-up (chunk 3) — on CLEAN shutdown, leave the live DB in a
/// crash-safe, verified-good state. This is the serve-side half of the
/// deferred crash-safe-checkpoint fix (the mechanism lives in
/// [`aberp_snapshot::durable_checkpoint`]).
///
/// If a verified-good checkpoint already covers the current file (the
/// `<db>.ckpt-ok` marker matches), this is a no-op; otherwise it takes ONE
/// durable checkpoint so the WAL is folded into a fresh file via an atomic
/// swap and the next boot needs no in-place `LoadCheckpoint`/`ReadIndex`
/// replay (the path that historically tripped `duckdb#23046`, S332/S375).
///
/// **ADR-0111** — the checkpoint is taken through the shared
/// [`aberp_db::Handle`], never on a bare path. Even at shutdown the handle is
/// still open (it lives in `recovery_state.db`), so a path-based
/// `durable_checkpoint` here would `rename` the live file out from under the
/// shared connection and unlink its WAL. That is the same orphaning the daemon
/// paths suffered; it also re-creates the ADR-0098 two-instance hazard, because
/// the primitive's own `Connection::open` would be a SECOND live opener beside
/// the handle's.
///
/// Best-effort by contract: every failure is logged LOUD (CLAUDE.md #12)
/// and swallowed — a checkpoint hiccup must NEVER wedge process exit (that
/// was the original S213 bug). Editions-tree ONLY; it refuses to act on a
/// prod path as defense-in-depth behind the compile-time edition binding.
pub fn checkpoint_on_clean_shutdown(db: &HandleArc) {
    let db_path = db.db_path();
    let disabled = std::env::var(CHECKPOINT_ON_SHUTDOWN_DISABLE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if disabled {
        tracing::info!(
            env = CHECKPOINT_ON_SHUTDOWN_DISABLE_ENV,
            "clean-shutdown durable checkpoint disabled by env"
        );
        return;
    }
    // Never checkpoint a prod path (impossible in an editions build — and
    // `Handle::open` already refuses one — but the guard keeps "never touches
    // prod" mechanical at this site too).
    if let Err(e) = ensure_not_prod_path(db_path) {
        tracing::error!(
            error = %e,
            "refusing clean-shutdown checkpoint on a prod path (unreachable in an editions build)"
        );
        return;
    }
    if !db_path.exists() {
        tracing::debug!(db = %db_path.display(), "no DB file at shutdown; nothing to checkpoint");
        return;
    }
    if aberp_snapshot::checkpoint_is_current(db_path) {
        tracing::info!(
            db = %db_path.display(),
            "clean shutdown: a verified-good checkpoint already covers the DB; skipping"
        );
        return;
    }
    // Under the writer lock, with the shared connection quiesced and reopened
    // around the swap. Logs its own success/failure (the report's sha/bytes move
    // into aberp-db's log line); failures are loud and swallowed there, so a
    // checkpoint ERROR never wedges exit (the original S213 bug).
    //
    // BOUNDED, and here is why (PR #41 adversarial, finding 2). The first cut
    // of this claimed "the drain already finished, so no WriteGuard is alive".
    // That is FALSE: `ShutdownCoordinator::shutdown` races each daemon's
    // `JoinHandle` against a shared 5 s budget with `tokio::time::timeout`, and
    // on expiry it DROPS the handle — which DETACHES the tokio task rather than
    // aborting it. A straggler daemon is therefore still running and may still
    // be inside a `WriteGuard`, so an unbounded `checkpoint_now()` would park on
    // its mutex (measured ~1.8 s against a slow straggler; unbounded in
    // principle) and block process exit. Rule 12 / S213 forbid that, so the wait
    // is capped and a miss is loud.
    //
    // Giving up costs little: the live file is left to the next boot's recovery
    // path, exactly as after any unclean stop, and the ADR-0095 §3 daemon
    // cadence has been folding it all along. What we do NOT do is drop back to
    // the path-based primitive — the handle is still open here (it lives in
    // `recovery_state.db`), so that would strand its connection AND make the
    // primitive's own `Connection::open` a second live opener (ADR-0098 1a).
    //
    // Only the SUCCESS arm announces success. The first cut logged "durable
    // checkpoint installed" unconditionally, including when the checkpoint had
    // just errored (PR #41 adversarial) — a line an operator would read during
    // a recovery as evidence the live file was folded when it was not. The
    // failure arms are logged loud inside `aberp-db` (the checkpoint error) and
    // inside `checkpoint_now_within` (the lock miss, with its fallback), so
    // there is nothing to add on either.
    // `Some(true)` and nothing else: `Some(false)` is a checkpoint that ERRORED
    // (logged in aberp-db) and `None` is the lock miss (logged there too).
    if db.checkpoint_now_within(SHUTDOWN_CHECKPOINT_LOCK_BUDGET) == Some(true) {
        tracing::info!(
            db = %db_path.display(),
            "clean shutdown: crash-safe durable checkpoint installed under the shared handle (ADR-0082 chunk 3 / ADR-0111)"
        );
    }
}

/// ADR-0095 §3 — take ONE durable checkpoint of the LIVE file off the request
/// path. Both the periodic daemon cadence ([`run_supervised`]) and the
/// post-write debouncer ([`crate::live_checkpoint`]) call this, so a recent
/// verified-good live file exists even with no clean shutdown — closing the
/// "nothing checkpoints the live file on a path a crash traverses" gap
/// (ADR-0095 root cause #2).
///
/// # ADR-0111 — why this takes a handle, not a path
///
/// This used to call [`aberp_snapshot::live_durable_checkpoint`] on a bare
/// path, from a `spawn_blocking` task, holding no lock. That primitive ends in
/// `atomic_install`: `rename(staging → db)` plus an unlink of `<db>.wal`. The
/// shared connection was left holding an fd on the OLD inode, so every commit
/// after a daemon tick landed in an unlinked file the kernel freed at exit —
/// while the post-commit `sync_mirror`, reading that same connection, durably
/// wrote those rows into the JSONL mirror. **Mirror ahead of DB**: the
/// direction `preserve_ahead_mirror` refuses and Defense's boot auto-heal
/// replays, i.e. the root behind the recurring audit-chain forks.
/// [`aberp_db::Handle::checkpoint_now`] takes the writer mutex, quiesces the
/// shared connection so the checkpoint is the sole opener, and reopens on the
/// freshly installed inode. Pinned by
/// `crates/aberp-db/tests/checkpoint_swap_orphan.rs`.
///
/// Best-effort by contract (mirrors [`checkpoint_on_clean_shutdown`]): a no-op
/// when a verified-good checkpoint already covers the file (cheap, via
/// [`aberp_snapshot::live_durable_checkpoint`] → `checkpoint_is_current`, which
/// the handle still calls underneath); every failure is logged LOUD
/// (CLAUDE.md #12) inside the handle and swallowed, so a checkpoint hiccup
/// never takes down `aberp serve`. Editions-tree ONLY — the wrapped primitive
/// refuses a prod path as defense in depth, and `Handle::open` already did.
///
/// **Never call this while a `WriteGuard` is alive** — the writer mutex is not
/// reentrant. Both callers run it after their write work has fully returned.
pub fn live_checkpoint_logged(db: &HandleArc) {
    // The name promises a log line, and the first cut stopped emitting one when
    // the body became a single delegation (PR #41 adversarial): the handle logs
    // an installed checkpoint at DEBUG and a failure at ERROR, so at the
    // default level a healthy daemon tick went completely silent — there was no
    // longer any way to see from the logs that the cadence was running at all.
    // One INFO line on success restores that, and costs one line per tick.
    if db.checkpoint_now() {
        tracing::info!(
            db = %db.db_path().display(),
            "live-path crash-safe durable checkpoint taken under the shared handle (ADR-0095 §3 / ADR-0111)"
        );
    }
    // The failure arm is logged LOUD inside the handle, with the fallback.
}

// ──────────────────────────────────────────────────────────────────────
// Configuration resolution
// ──────────────────────────────────────────────────────────────────────

/// Resolve the snapshot store directory: an explicit `--store` wins,
/// otherwise the EDITION-SCOPED default
/// `~/Documents/ABERP-snapshots-<edition>/<tenant>` (ADR-0093 §5).
///
/// The default is derived from the COMPILE-TIME
/// [`build_profile::edition_store_segment`] — never an env/launcher string —
/// so Defense and Portable get disjoint stores that can never share prod's.
/// Whichever store is chosen, it is refused if it points at the frozen prod
/// line (prod's `~/.aberp/` or `~/Documents/ABERP-snapshots/`), so even a
/// hand-passed `--store` can never reach prod.
pub fn resolve_store(tenant: &str, explicit: Option<&Path>) -> Result<PathBuf> {
    let store = match explicit {
        Some(p) => p.to_path_buf(),
        None => edition_store_dir(build_profile::edition_store_segment(), tenant)
            .context("resolve edition-scoped snapshot store dir")?,
    };
    ensure_not_prod_path(&store).map_err(|e| {
        anyhow::anyhow!("snapshot store must not be under the frozen prod line: {e}")
    })?;
    Ok(store)
}

/// Read the retention policy from the environment, falling back to the
/// ADR-0082 defaults. Overridable so an operator can widen/narrow retention
/// without a rebuild (`[[trust-code-not-operator]]` — the knob is explicit,
/// not buried).
pub fn policy_from_env() -> RetentionPolicy {
    let d = RetentionPolicy::default();
    RetentionPolicy {
        keep_last: env_usize("ABERP_SNAPSHOT_KEEP_LAST", d.keep_last),
        daily_days: env_i64("ABERP_SNAPSHOT_DAILY_DAYS", d.daily_days),
        weekly_weeks: env_i64("ABERP_SNAPSHOT_WEEKLY_WEEKS", d.weekly_weeks),
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Snapshot cadence from `ABERP_SNAPSHOT_INTERVAL_SECS` (default 4h). A
/// value of 0 or an unparseable value falls back to the default.
pub fn interval_from_env() -> Duration {
    let secs = std::env::var("ABERP_SNAPSHOT_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// ADR-0099 — how a snapshot audit event reaches the ledger. The seq-515 fork
/// was the periodic snapshot daemon's `snapshot.created` opening an INDEPENDENT
/// [`Ledger`] on the live DB (this module's old `open_ledger`) and self-assigning
/// a seq off a stale head while the quote-intake daemon did the same off the same
/// head — both in the ONE `aberp serve` process. The fix routes every in-process
/// snapshot audit append through the ONE shared [`aberp_db::Handle`]. The CLI
/// subcommands (`aberp snapshot now/restore`) are a SEPARATE process with no
/// Handle, so they keep the sanctioned reopen (cannot fork the serve writer).
pub enum SnapshotAudit<'a> {
    /// In-process (`aberp serve`): the periodic daemon AND the operator-UI HTTP
    /// endpoints. Appends through the shared Handle's serialized writer — never
    /// an independent opener (the WriteGuard drop runs the lockstep mirror sync).
    Handle(&'a HandleArc),
    /// Separate-process CLI one-shot (`aberp snapshot now/restore`): no Handle
    /// exists in that process, so reopen the live DB (see [`emit_reopen_cli`]).
    Reopen,
}

/// Append one snapshot audit event, routed per [`SnapshotAudit`]. In-process
/// callers append through the shared [`aberp_db::Handle`]; the CLI reopens.
fn emit_snapshot_event(
    audit: &SnapshotAudit<'_>,
    db_path: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    kind: EventKind,
    payload: Vec<u8>,
    actor: Actor,
) -> Result<()> {
    match audit {
        SnapshotAudit::Handle(handle) => {
            // Shared writer: the ONE serialized instance. No independent opener,
            // no stale-head seq collision. WriteGuard drop runs the lockstep
            // sync_mirror, so no separate `sync_mirror` is needed here either.
            let mut conn = handle
                .write()
                .map_err(|e| anyhow::anyhow!("shared writer for snapshot audit event: {e}"))?;
            aberp_audit_ledger::ensure_schema(&conn)
                .map_err(|e| anyhow::anyhow!("ensure audit-ledger schema (snapshot event): {e}"))?;
            let tx = conn
                .transaction()
                .map_err(|e| anyhow::anyhow!("begin DuckDB tx (snapshot event): {e}"))?;
            let meta = LedgerMeta::new(tenant.clone(), binary_hash);
            aberp_audit_ledger::append_in_tx(&tx, &meta, kind, payload, actor, None).map_err(
                |e| anyhow::anyhow!("append snapshot audit event via shared Handle: {e}"),
            )?;
            tx.commit()
                .map_err(|e| anyhow::anyhow!("commit DuckDB tx (snapshot event): {e}"))?;
            Ok(())
        }
        SnapshotAudit::Reopen => {
            emit_reopen_cli(db_path, tenant, binary_hash, kind, payload, actor)
        }
    }
}

/// SANCTIONED RESIDUAL (ADR-0099 gate allow-list: `emit_reopen_cli`) — the CLI
/// reopen path. Only the separate-process `aberp snapshot {now,restore}`
/// subcommands reach this; they have no [`aberp_db::Handle`] (a different
/// process from `aberp serve`), so reopening the live DB here cannot fork
/// against the serve-process writer. The `aberp serve` daemon + HTTP callers
/// NEVER reach this branch (they pass [`SnapshotAudit::Handle`]). Kept a
/// distinct, single-purpose fn so the cut-gate can allow-list it by name.
fn emit_reopen_cli(
    db_path: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    kind: EventKind,
    payload: Vec<u8>,
    actor: Actor,
) -> Result<()> {
    let mut ledger = Ledger::open(db_path, tenant.clone(), binary_hash)
        .map_err(|e| anyhow::anyhow!("open audit ledger for snapshot event (CLI): {e}"))?;
    ledger
        .append(kind, payload, actor, None)
        .map_err(|e| anyhow::anyhow!("append snapshot audit event (CLI): {e}"))?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Shared operations (CLI + daemon + HTTP all call these)
// ──────────────────────────────────────────────────────────────────────

/// ADR-0099 R2 — run the pre-snapshot audit-mirror reconcile through the ONE
/// shared [`aberp_db::Handle`] writer when this process has one, and report back
/// which owner `take_snapshot_with` should assume.
///
/// Holding the write guard across [`aberp_audit_ledger::ensure_consistent_with_db`]
/// is what makes it exclusive against the lockstep `sync_mirror` that fires from
/// every other `WriteGuard::drop` in the process; the reconciler's own mirror
/// `flock` (ADR-0099 R2) covers the cross-process half. Lock order is
/// handle-mutex → mirror-flock, the same order the lockstep path takes.
///
/// Best-effort, exactly as the in-`take_snapshot` call it replaces: a reconcile
/// failure is surfaced loud and the snapshot is still taken (the EXPORT of the
/// live DB is independently valuable, and boot `ensure_consistent_with_db` owns
/// the ahead-mirror P0).
fn reconcile_mirror_for(audit: &SnapshotAudit<'_>, db_path: &Path) -> MirrorReconcile {
    let handle = match audit {
        SnapshotAudit::Handle(h) => *h,
        // No Handle in this process — the export connection is the only opener,
        // so let `take_snapshot_with` reconcile on it as it always has.
        SnapshotAudit::Reopen => return MirrorReconcile::OnExportConnection,
    };
    let mirror_path = aberp_audit_ledger::mirror_path_for(db_path);
    match handle.write() {
        Ok(guard) => match aberp_audit_ledger::ensure_consistent_with_db(&guard, &mirror_path) {
            Ok(action) => tracing::debug!(
                ?action,
                mirror = %mirror_path.display(),
                "ADR-0099 R2 — pre-snapshot mirror reconcile via the shared Handle writer"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                mirror = %mirror_path.display(),
                "ADR-0099 R2 — pre-snapshot mirror reconcile via the shared Handle FAILED \
                 (best-effort); taking the snapshot anyway"
            ),
        },
        Err(e) => tracing::warn!(
            error = %e,
            "ADR-0099 R2 — could not take the shared writer for the pre-snapshot mirror \
             reconcile (best-effort); taking the snapshot anyway"
        ),
    }
    // The reconcile is owned here either way: on failure we must NOT fall back
    // to the export connection, which is the second-writer path R2 removes.
    MirrorReconcile::AlreadyDoneByCaller
}

/// Take one validated snapshot and emit the appropriate audit event
/// (`SnapshotCreated` on success, `SnapshotValidationFailed` if the
/// snapshot was produced but failed its built-in validation — in which case
/// the invalid snapshot is kept on disk and the last-good is preserved by
/// retention). Returns the finalized record either way.
pub fn take_and_emit(
    audit: &SnapshotAudit<'_>,
    db_path: &Path,
    store_dir: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    actor: Actor,
) -> Result<SnapshotRecord> {
    // ADR-0093 — an editions build never snapshots the frozen prod DB,
    // however `--db` arrived (defense-in-depth behind the compile-time
    // edition→root binding from chunk 2).
    ensure_not_prod_path(db_path).map_err(|e| {
        anyhow::anyhow!("snapshot source DB must not be under the frozen prod line: {e}")
    })?;
    let now = OffsetDateTime::now_utc();
    // ADR-0099 R2 — the pre-EXPORT audit-MIRROR reconcile is a WRITER of the
    // audit ledger's mirror half. In `aberp serve` it must run on the ONE shared
    // instance, under the ONE serialized writer — not on `take_snapshot`'s own
    // short-lived export connection, which is a separate DuckDB instance that
    // does not replay the live writer's WAL and therefore reads a STALE-LOW
    // `db_max_seq` (spurious `MirrorAheadOfDb`). The CLI has no Handle and is
    // the only opener in its process, so it keeps reconciling on the export
    // connection. Same seam as the audit append: `SnapshotAudit` already carries
    // the Handle, so no signature changes.
    let reconcile = reconcile_mirror_for(audit, db_path);
    let rec = take_snapshot_with(db_path, store_dir, tenant.as_str(), now, reconcile)
        .with_context(|| format!("take snapshot of {}", db_path.display()))?;

    let created_at = rfc3339(rec.meta.created_at);
    if rec.meta.valid {
        let payload = SnapshotCreatedPayload {
            seq: rec.meta.seq,
            created_at,
            source_db_sha256: rec.meta.source_db_sha256.clone(),
            byte_size: rec.meta.byte_size,
            invoice_count: rec.meta.invoice_count,
            audit_count: rec.meta.audit_count,
            chain_len: rec.meta.chain_len,
            store_dir: store_dir.display().to_string(),
        };
        emit_snapshot_event(
            audit,
            db_path,
            tenant,
            binary_hash,
            EventKind::SnapshotCreated,
            payload.to_bytes(),
            actor,
        )
        .map_err(|e| anyhow::anyhow!("append SnapshotCreated: {e}"))?;
        tracing::info!(
            seq = rec.meta.seq,
            audit = rec.meta.audit_count,
            invoices = rec.meta.invoice_count,
            "snapshot created and validated"
        );
    } else {
        let payload = SnapshotValidationFailedPayload {
            seq: rec.meta.seq,
            created_at,
            error: rec
                .meta
                .validation_error
                .clone()
                .unwrap_or_else(|| "unknown validation failure".to_string()),
        };
        emit_snapshot_event(
            audit,
            db_path,
            tenant,
            binary_hash,
            EventKind::SnapshotValidationFailed,
            payload.to_bytes(),
            actor,
        )
        .map_err(|e| anyhow::anyhow!("append SnapshotValidationFailed: {e}"))?;
        tracing::error!(
            seq = rec.meta.seq,
            error = rec.meta.validation_error.as_deref().unwrap_or("?"),
            "snapshot FAILED validation — kept and marked invalid; last-good preserved"
        );
    }
    Ok(rec)
}

/// Apply retention to the store and emit `SnapshotPruned` if anything was
/// removed. Returns the pruned seqs.
pub fn retention_and_emit(
    audit: &SnapshotAudit<'_>,
    db_path: &Path,
    store_dir: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    actor: Actor,
    policy: &RetentionPolicy,
) -> Result<Vec<u64>> {
    let records = list_snapshots(store_dir).context("list snapshots for retention")?;
    let plan = plan_retention(&records, policy, OffsetDateTime::now_utc());
    if plan.prune.is_empty() {
        return Ok(Vec::new());
    }
    let removed = prune(&records, &plan).context("prune snapshots")?;
    if !removed.is_empty() {
        let payload = SnapshotPrunedPayload {
            pruned_seqs: removed.clone(),
            retained_count: plan.keep.len(),
            ran_at: rfc3339(OffsetDateTime::now_utc()),
        };
        emit_snapshot_event(
            audit,
            db_path,
            tenant,
            binary_hash,
            EventKind::SnapshotPruned,
            payload.to_bytes(),
            actor,
        )
        .map_err(|e| anyhow::anyhow!("append SnapshotPruned: {e}"))?;
        tracing::info!(pruned = ?removed, retained = plan.keep.len(), "snapshot retention applied");
    }
    Ok(removed)
}

/// One full daemon cycle: take + validate + emit, then retention + emit.
/// Retention failure does not discard the snapshot just taken.
pub fn run_cycle(
    audit: &SnapshotAudit<'_>,
    db_path: &Path,
    store_dir: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    actor: Actor,
    policy: &RetentionPolicy,
) -> Result<SnapshotRecord> {
    // `BinaryHash` is `Copy`; `Actor` is cloned for the second emit.
    let rec = take_and_emit(
        audit,
        db_path,
        store_dir,
        tenant,
        binary_hash,
        actor.clone(),
    )?;
    if let Err(e) = retention_and_emit(
        audit,
        db_path,
        store_dir,
        tenant,
        binary_hash,
        actor,
        policy,
    ) {
        // A retention hiccup must not fail the cycle — the fresh snapshot is
        // the valuable output; stale extras are harmless.
        tracing::warn!(error = %e, "snapshot retention failed this cycle (snapshot itself is fine)");
    }
    Ok(rec)
}

/// Restore a snapshot into `target`, emitting `SnapshotRestored`. The guard
/// ([`ensure_restore_allowed`]) MUST already have passed — callers run it
/// first so a refusal never even finds the snapshot.
pub fn restore_and_emit(
    audit: &SnapshotAudit<'_>,
    db_path_for_audit: &Path,
    store_dir: &Path,
    selector: &str,
    target: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    actor: Actor,
) -> Result<SnapshotRecord> {
    // ADR-0093 — restore reads ONLY this edition's own store and never
    // writes a prod-line audit DB.
    ensure_not_prod_path(store_dir).map_err(|e| {
        anyhow::anyhow!("restore source store must not be under the frozen prod line: {e}")
    })?;
    ensure_not_prod_path(db_path_for_audit).map_err(|e| {
        anyhow::anyhow!("restore audit DB must not be under the frozen prod line: {e}")
    })?;
    let rec = find_snapshot(store_dir, selector)
        .map_err(|e| anyhow::anyhow!("find snapshot '{selector}': {e}"))?;
    restore_into(&rec.dir, target, tenant.as_str())
        .map_err(|e| anyhow::anyhow!("restore snapshot '{selector}': {e}"))?;

    let payload = SnapshotRestoredPayload {
        seq: rec.meta.seq,
        snapshot_dir: rec.dir.display().to_string(),
        target: target.display().to_string(),
        restored_at: rfc3339(OffsetDateTime::now_utc()),
    };
    // The audit row records the restore against the live DB's ledger (NOT
    // the freshly-restored side-DB), so the operator's main timeline shows
    // that a restore happened.
    emit_snapshot_event(
        audit,
        db_path_for_audit,
        tenant,
        binary_hash,
        EventKind::SnapshotRestored,
        payload.to_bytes(),
        actor,
    )
    .map_err(|e| anyhow::anyhow!("append SnapshotRestored: {e}"))?;
    tracing::info!(seq = rec.meta.seq, target = %target.display(), "snapshot restored");
    Ok(rec)
}

// ──────────────────────────────────────────────────────────────────────
// CLI entry points
// ──────────────────────────────────────────────────────────────────────

/// `aberp snapshot now` — take one managed, validated snapshot immediately
/// and apply retention.
pub fn run_now(args: &SnapshotNowArgs) -> Result<()> {
    let tenant = tenant_id(&args.tenant)?;
    let store_dir = resolve_store(&args.tenant, args.store.as_deref())?;
    let binary_hash = crate::binary_hash::compute().context("compute binary hash")?;
    let actor = cli_actor("system:snapshot-cli");
    let policy = policy_from_env();

    // CLI is a SEPARATE process from `aberp serve` (no Handle) — reopen.
    let rec = run_cycle(
        &SnapshotAudit::Reopen,
        &args.db,
        &store_dir,
        &tenant,
        binary_hash,
        actor,
        &policy,
    )?;
    if rec.meta.valid {
        println!(
            "Snapshot #{} written and validated → {}\n  invoices={}  audit_rows={}  chain={}  size={}",
            rec.meta.seq,
            rec.dir.display(),
            rec.meta.invoice_count,
            rec.meta.audit_count,
            rec.meta.chain_len,
            human_size(rec.meta.byte_size),
        );
    } else {
        println!(
            "Snapshot #{} FAILED validation (kept for inspection) → {}\n  reason: {}",
            rec.meta.seq,
            rec.dir.display(),
            rec.meta.validation_error.as_deref().unwrap_or("?"),
        );
    }
    Ok(())
}

/// `aberp snapshot list` — show seq / timestamp / size / validation / age.
pub fn run_list(args: &SnapshotListArgs) -> Result<()> {
    let store_dir = resolve_store(&args.tenant, args.store.as_deref())?;
    let records = list_snapshots(&store_dir).context("list snapshots")?;
    if records.is_empty() {
        println!("No snapshots in {}", store_dir.display());
        return Ok(());
    }
    let now = OffsetDateTime::now_utc();
    println!("Snapshots in {} (newest first):", store_dir.display());
    println!(
        "  {:>5}  {:<20}  {:>9}  {:<8}  {:<10}",
        "SEQ", "TIMESTAMP (UTC)", "SIZE", "STATUS", "AGE"
    );
    for r in &records {
        println!(
            "  {:>5}  {:<20}  {:>9}  {:<8}  {:<10}",
            r.meta.seq,
            rfc3339(r.meta.created_at),
            human_size(r.meta.byte_size),
            if r.meta.valid { "valid" } else { "INVALID" },
            human_age(r.age(now)),
        );
    }
    Ok(())
}

/// `aberp snapshot restore <seq|ts> --to <path> --confirm` — guarded
/// restore. Refuses without `--confirm` or onto any live `~/.aberp` DB,
/// BEFORE touching the store (`[[trust-code-not-operator]]`).
pub fn run_restore(args: &SnapshotRestoreArgs) -> Result<()> {
    // Guard first — the safety lives in the binary, not the operator.
    ensure_restore_allowed(&args.to, args.confirm).map_err(|e| anyhow::anyhow!("{e}"))?;

    let tenant = tenant_id(&args.tenant)?;
    let store_dir = resolve_store(&args.tenant, args.store.as_deref())?;
    let binary_hash = crate::binary_hash::compute().context("compute binary hash")?;
    let actor = cli_actor("system:snapshot-cli");

    let rec = restore_and_emit(
        &SnapshotAudit::Reopen,
        &args.db,
        &store_dir,
        &args.selector,
        &args.to,
        &tenant,
        binary_hash,
        actor,
    )?;
    println!(
        "Restored snapshot #{} → {}\n(verify it, then stop `aberp serve` and swap it into place if this is a prod recovery)",
        rec.meta.seq,
        args.to.display()
    );
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Periodic daemon (spawned by `aberp serve`)
// ──────────────────────────────────────────────────────────────────────

/// Everything the snapshot daemon needs, captured at boot.
pub struct SnapshotDaemonDeps {
    /// ADR-0099 — the ONE shared process-wide Handle. The daemon appends its
    /// `snapshot.created`/`.pruned`/… audit rows through this serialized writer,
    /// never an independent opener (the seq-515 fork was two independent openers
    /// off the same head). `db_path` is retained for the logical `EXPORT`
    /// (`take_snapshot`, the sanctioned read-only export seam) and prod-path
    /// guards — NOT to open the ledger.
    pub db: HandleArc,
    pub db_path: PathBuf,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub store_dir: PathBuf,
    pub interval: Duration,
    pub policy: RetentionPolicy,
}

/// `true` if the periodic daemon is disabled by env. The manual CLI/HTTP
/// "snapshot now" path is unaffected.
pub fn is_disabled() -> bool {
    std::env::var(POLL_DISABLE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Supervised periodic snapshot loop. Sleeps `BOOT_DELAY_SECS` after boot,
/// then snapshots every `interval`. Each cycle runs on a blocking thread
/// (DuckDB EXPORT/IMPORT is blocking) and logs-but-survives any error — a
/// snapshot failure never takes down `aberp serve`.
pub async fn run_supervised(deps: SnapshotDaemonDeps, cancel: CancellationToken) {
    tracing::info!(
        interval_secs = deps.interval.as_secs(),
        store = %deps.store_dir.display(),
        "snapshot daemon started (S426 / ADR-0082)"
    );
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(Duration::from_secs(BOOT_DELAY_SECS)) => {}
    }
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let db = deps.db_path.clone();
        let store = deps.store_dir.clone();
        let tenant = deps.tenant.clone();
        let bh = deps.binary_hash; // BinaryHash is Copy
        let policy = deps.policy;
        let actor = cli_actor("system:snapshot-daemon");
        // ADR-0099 — this daemon is IN the `serve` process; append its audit
        // rows through the ONE shared Handle, never an independent opener.
        let handle = deps.db.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let audit = SnapshotAudit::Handle(&handle);
            let rec = run_cycle(&audit, &db, &store, &tenant, bh, actor, &policy);
            // ADR-0095 §3 — also keep the LIVE file crash-safe between clean
            // shutdowns: fold a debounced durable checkpoint into the daemon
            // cadence so a recent verified-good live file always exists, even
            // if the process never reaches a clean shutdown. No-op when
            // `checkpoint_is_current`; logged-but-survives like the cycle.
            //
            // ADR-0111 — routed through the SHARED handle (`deps.db`), not the
            // path. The path form renamed the live file out from under the
            // shared connection and unlinked its WAL, orphaning every commit
            // made after this tick into a freed inode while the lockstep
            // `sync_mirror` still recorded them — mirror ahead of DB.
            //
            // Ordering matters and is deliberate: `run_cycle` has FULLY
            // returned here, so its audit `WriteGuard`s are dropped and the
            // writer mutex is free. `checkpoint_now` takes that mutex and the
            // mutex is NOT reentrant — calling it inside a guard would
            // self-deadlock this daemon thread.
            live_checkpoint_logged(&handle);
            rec
        })
        .await;
        match outcome {
            Ok(Ok(_rec)) => {}
            Ok(Err(e)) => {
                tracing::error!(error = %e, "snapshot cycle failed; daemon continues")
            }
            Err(join) => {
                tracing::error!(error = %join, "snapshot cycle task panicked; daemon continues")
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(deps.interval) => {}
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Small helpers
// ──────────────────────────────────────────────────────────────────────

fn tenant_id(tenant: &str) -> Result<TenantId> {
    TenantId::new(tenant.to_string()).with_context(|| format!("invalid tenant id {tenant:?}"))
}

fn cli_actor(login: &str) -> Actor {
    use ulid::Ulid;
    Actor::from_local_cli(Ulid::new().to_string(), login)
}

/// Format an `OffsetDateTime` as RFC-3339 (UTC, e.g. `2026-06-15T14:30:00Z`).
pub fn rfc3339(dt: OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| dt.unix_timestamp().to_string())
}

/// Human-readable byte size (KiB/MiB/GiB).
pub fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Coarse human age ("3h", "2d", "5w").
pub fn human_age(d: time::Duration) -> String {
    let secs = d.whole_seconds().max(0);
    if secs >= 7 * 86400 {
        format!("{}w", secs / (7 * 86400))
    } else if secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn human_age_buckets() {
        assert_eq!(human_age(time::Duration::seconds(45)), "45s");
        assert_eq!(human_age(time::Duration::hours(3)), "3h");
        assert_eq!(human_age(time::Duration::days(2)), "2d");
        assert_eq!(human_age(time::Duration::days(14)), "2w");
    }

    #[test]
    fn rfc3339_is_z_suffixed() {
        let dt = time::macros::datetime!(2026-06-15 14:30:00 UTC);
        assert_eq!(rfc3339(dt), "2026-06-15T14:30:00Z");
    }

    // ── ADR-0099 R2 — who owns the pre-snapshot mirror reconcile ────────────
    //
    // The seq-2508 fork was the snapshot daemon reconciling the audit MIRROR on
    // `take_snapshot`'s own export connection: a second mirror writer inside
    // `aberp serve`, on a DuckDB instance that does not replay the shared
    // writer's WAL. R2 hoists that reconcile onto the shared Handle for the
    // in-process arm and leaves the CLI arm (no Handle in that process) alone.
    //
    // Both halves are pinned, because either one failing is SILENT: an arm that
    // returns the wrong owner still produces a correct-looking mirror, and a
    // reconcile that never runs still produces a correct-looking snapshot.

    fn r2_tmp(label: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("aberp-r2-owner-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn reconcile_mirror_for_handle_runs_it_here_and_claims_ownership() {
        let dir = r2_tmp("handle");
        let db = dir.join("aberp.duckdb");
        let tenant = TenantId::new("defense".to_string()).unwrap();
        // Seed a short chain, then remove the mirror so "did the reconcile run?"
        // is answerable by the file's existence alone.
        {
            let mut ledger =
                Ledger::open(&db, tenant.clone(), BinaryHash::from_bytes([1u8; 32])).unwrap();
            ledger
                .append(EventKind::Test, b"{}".to_vec(), cli_actor("t"), None)
                .unwrap();
        }
        let mirror = aberp_audit_ledger::mirror_path_for(&db);
        let _ = std::fs::remove_file(&mirror);

        let handle = aberp_db::Handle::open(
            &db,
            tenant,
            aberp_db::HandleConfig {
                checkpoint_enabled: false,
                ..Default::default()
            },
        )
        .unwrap();
        let owner = reconcile_mirror_for(&SnapshotAudit::Handle(&handle), &db);

        assert_eq!(
            owner,
            MirrorReconcile::AlreadyDoneByCaller,
            "the in-process arm must claim the reconcile, or `take_snapshot_with` \
             does it again on its own export connection — the second mirror writer \
             ADR-0099 R2 removed"
        );
        assert!(
            mirror.exists(),
            "claiming ownership without actually reconciling would leave the mirror \
             un-reconciled before every snapshot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_mirror_for_cli_defers_to_the_export_connection() {
        let dir = r2_tmp("cli");
        let db = dir.join("aberp.duckdb");
        let owner = reconcile_mirror_for(&SnapshotAudit::Reopen, &db);
        assert_eq!(
            owner,
            MirrorReconcile::OnExportConnection,
            "the CLI process has no shared Handle, so `take_snapshot_with` must keep \
             reconciling on its export connection (its only opener)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
