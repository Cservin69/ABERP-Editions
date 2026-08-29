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
    edition_store_dir, ensure_not_prod_path, ensure_restore_allowed, list_snapshots,
    plan_retention, prune, resolve_selector, restore_in_place, restore_into, snapshot_identity,
    take_snapshot_with, validate_export, MirrorReconcile, RetentionPolicy, SnapshotRecord,
};

use crate::build_profile;

use crate::audit_payloads::EvidenceArchivedPayload;
use crate::audit_payloads::{
    SnapshotCreatedPayload, SnapshotPrunedPayload, SnapshotRestoredPayload,
    SnapshotValidationFailedPayload,
};
use crate::cli::{
    EvidenceArchiveArgs, EvidenceListArgs, RestoreInPlaceArgs, SnapshotListArgs, SnapshotNowArgs,
    SnapshotPruneArgs, SnapshotRestoreArgs,
};

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

/// How long the clean-shutdown snapshot will run before the process gives up
/// on it and exits anyway (ADR-0116 D5).
///
/// **Bounded for the same reason the shutdown checkpoint is** (S213 /
/// CLAUDE.md rule 12): a snapshot hiccup must NEVER wedge process exit. A
/// logical `EXPORT` of a tenant DB is a few seconds at today's sizes, but it
/// is unbounded in principle — a large tenant, a slow disk, or a DuckDB stall
/// would otherwise hold the terminal open indefinitely at the worst possible
/// moment.
///
/// Giving up costs a `*.partial` directory, which is inert by construction:
/// `list_snapshots` and `next_seq` both ignore `*.partial`, so an abandoned
/// export is invisible to retention, to restore, and to the daemon.
const SHUTDOWN_SNAPSHOT_BUDGET: Duration = Duration::from_secs(30);

/// Env kill-switch for the clean-shutdown snapshot (ADR-0116 D5).
pub const SNAPSHOT_ON_SHUTDOWN_DISABLE_ENV: &str = "ABERP_SNAPSHOT_ON_SHUTDOWN_DISABLE";

/// **ADR-0116 D5/G7** — on CLEAN shutdown, leave a ROLLBACK POINT, not just a
/// crash-safe file.
///
/// `checkpoint_on_clean_shutdown` already folds the WAL into a fresh verified-
/// good file — which makes the live file crash-safe and produces **no rollback
/// point at all**. Those are different guarantees, and the gap between them is
/// exactly G7: there is no snapshot trigger on clean shutdown, on
/// boot-after-unclean-shutdown, before a restore, or before a migration.
///
/// Skipped when the store already holds a snapshot within `interval`, so a
/// restart loop cannot fill the store.
///
/// **Call this BEFORE [`checkpoint_on_clean_shutdown`]**, never after: the
/// snapshot's audit row is a WRITE through the shared handle, so taking it
/// after the checkpoint would immediately stale the verified-good marker the
/// checkpoint just wrote. In this order the checkpoint covers the
/// post-snapshot state.
///
/// Best-effort by contract, and BOUNDED — see [`SHUTDOWN_SNAPSHOT_BUDGET`].
pub fn snapshot_on_clean_shutdown(
    db: &HandleArc,
    db_path: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
) {
    if std::env::var(SNAPSHOT_ON_SHUTDOWN_DISABLE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        tracing::info!(
            env = SNAPSHOT_ON_SHUTDOWN_DISABLE_ENV,
            "clean-shutdown snapshot disabled by env (ADR-0116 D5)"
        );
        return;
    }
    if is_disabled() {
        return;
    }
    let store_dir = match resolve_store(tenant.as_str(), None) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                error = %e,
                "ADR-0116 D5 — could not resolve the snapshot store at shutdown; NO rollback                  point was created for this shutdown"
            );
            return;
        }
    };
    let interval = interval_from_env();
    let policy = policy_from_env();

    // Bounded: run the cycle on its own thread and stop WAITING for it after
    // the budget. The thread is left to finish (or not) as the process exits;
    // whatever it leaves behind is a `*.partial` the next run ignores.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handle = db.clone();
    let db_path = db_path.to_path_buf();
    let tenant = tenant.clone();
    std::thread::spawn(move || {
        trigger_snapshot_if_stale(
            &SnapshotAudit::Handle(&handle),
            &db_path,
            &store_dir,
            &tenant,
            binary_hash,
            &policy,
            interval,
            "clean-shutdown",
        );
        let _ = tx.send(());
    });
    if rx.recv_timeout(SHUTDOWN_SNAPSHOT_BUDGET).is_err() {
        tracing::error!(
            budget_secs = SHUTDOWN_SNAPSHOT_BUDGET.as_secs(),
            "ADR-0116 D5 — the clean-shutdown snapshot did not finish within its budget;              exiting anyway (a snapshot must never wedge process exit — S213). NO rollback              point exists for this shutdown; the next boot or the scheduled floor will take              one."
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
        // ADR-0116 G8 — forensic retention of FAILED snapshots. Overridable
        // like the rest, but note that setting either to 0 restores the
        // pre-G8 behaviour in which a snapshot that CAUGHT a defect is
        // deleted by the cycle that created it. Prod lost real evidence to
        // that twice.
        keep_failed: env_usize("ABERP_SNAPSHOT_KEEP_FAILED", d.keep_failed),
        keep_failed_days: env_i64("ABERP_SNAPSHOT_KEEP_FAILED_DAYS", d.keep_failed_days),
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
    // ADR-0116 D3.5 — durably ack the row before returning.
    //
    // `Ledger::open` sets `PRAGMA disable_checkpoint_on_shutdown` and nothing
    // else, and `append` commits a transaction WITHOUT a `durable_ack` and
    // without syncing the mirror. So after D3.1 the restored FILE is durable
    // while the row recording the restore is not — a power cut moments later
    // leaves a **silently-restored database**: the DB is the snapshot's, and
    // nothing in the ledger says why. On the D-22 precedent the restore event
    // gets a real flush.
    durable_ack_cli(db_path)?;
    Ok(())
}

/// ADR-0116 D3.5 — flush a CLI-appended audit row (and the DB it landed in)
/// to the device.
///
/// The separate-process CLI has no `aberp_db::Handle`, so `Handle::durable_ack`
/// — which claims a parked outcome from a `WriteGuard` drop — is unavailable
/// by construction. This is the equivalent for a process that owns the file
/// outright: fsync the main DB file and its WAL, then the parent directory.
///
/// **Honest scope.** On macOS `File::sync_all` is routed by the stdlib to
/// `fcntl(F_FULLFSYNC)`, so this is a device flush, not merely a hand-off to
/// the OS page cache. Linux gets `fsync`; Windows `FlushFileBuffers`. The
/// residual bottoms out at the drive honouring the flush.
///
/// A failure is PROPAGATED, never downgraded to a `warn!` — the whole point
/// is that the operator learns the restore record is not durable while they
/// are still at the terminal.
fn durable_ack_cli(db_path: &Path) -> Result<()> {
    let mut wal = db_path.as_os_str().to_owned();
    wal.push(".wal");
    let wal = PathBuf::from(wal);
    for p in [db_path, wal.as_path()] {
        match std::fs::File::open(p) {
            Ok(f) => f
                .sync_all()
                .with_context(|| format!("durable ack: fsync {}", p.display()))?,
            // No WAL is the normal case after a checkpoint; an absent main DB
            // would already have failed the append above.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "durable ack: open {} for fsync: {e}",
                    p.display()
                ))
            }
        }
    }
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(f) = std::fs::File::open(parent) {
            // A platform that refuses to open a directory is a soft failure:
            // the file contents are already flushed, and only the directory
            // ENTRY (which did not change here — the file already existed) is
            // at stake.
            let _ = f.sync_all();
        }
    }
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
/// ADR-0099 R3 — that `flock` wait is BOUNDED (`MirrorLockTimeout`). It is
/// cross-process, and this fn holds the shared writer mutex across it, so an
/// untimed acquire let any stuck peer — a hung `aberp` CLI, a crashed-but-not-
/// reaped process still owning the fd — freeze EVERY serve DB write behind it,
/// with no diagnostic. The timeout fails loud rather than proceeding
/// unsynchronised, so the TOCTOU the lock exists to close stays closed.
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
    // ADR-0116 D3.3 — resolve against the STABLE identity and refuse on
    // ambiguity. `seq` is recycled after a prune, so the previous
    // first-match-wins behaviour could silently pick between a good snapshot
    // and one of the `validation_failed` pair that shared its seq.
    let rec = resolve_selector(store_dir, selector)
        .map_err(|e| anyhow::anyhow!("resolve snapshot selector '{selector}': {e}"))?;
    restore_into(&rec.dir, target, tenant.as_str())
        .map_err(|e| anyhow::anyhow!("restore snapshot '{selector}': {e}"))?;

    // ADR-0116 D4 — the anchor coverage recorded on the row comes from a LIVE
    // re-validation of the export, never from `meta.json`. One extra in-memory
    // IMPORT on a rare, operator-paced command, for a number that goes into an
    // audit row a court may read. See `anchor_verdict_of`.
    let live = validate_export(&rec.dir, tenant.as_str());
    let payload = SnapshotRestoredPayload {
        seq: rec.meta.seq,
        snapshot_dir: rec.dir.display().to_string(),
        target: target.display().to_string(),
        restored_at: rfc3339(OffsetDateTime::now_utc()),
        // ADR-0116 D4 — recorded on the SIDE-PATH restore too. The legal
        // claim is made wherever a restored database is produced, and this
        // command produces one; the difference is only which ledger the row
        // lands on (here, the live one — the restored file is a side path).
        anchor_verdict: anchor_verdict_slug(anchor_verdict_live(&live)).to_string(),
        anchor_coverage: describe_anchors_live(&live),
        // The side path never touches the live DB, so nothing is discarded.
        discarded_audit_rows: Some(0),
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
    // ADR-0116 D3.5 — **the restore row must be durably acked, on BOTH routing
    // arms.** The CLI arm acks inside `emit_reopen_cli`; the in-process arm
    // (the operator-UI HTTP route) is acked here.
    //
    // Without it, post-D3.1 the restored FILE is durable while the row
    // recording the restore is not — a power cut moments later leaves a
    // **silently-restored database**: the DB is the snapshot's, and nothing in
    // the ledger says why or from what.
    //
    // Called AFTER the append has fully returned, so the WriteGuard is dropped
    // and the writer mutex is free — it is not reentrant, and acking under a
    // live guard would self-deadlock. Per the D3 contract this is PROPAGATED,
    // never downgraded to a `warn!` (cut-gate CHECK D3-C).
    if let SnapshotAudit::Handle(handle) = audit {
        handle
            .durable_ack()
            .map_err(|e| anyhow::anyhow!("durable ack of the SnapshotRestored row: {e}"))?;
    }
    tracing::info!(seq = rec.meta.seq, target = %target.display(), "snapshot restored");
    Ok(rec)
}

// ──────────────────────────────────────────────────────────────────────
// CLI entry points
// ──────────────────────────────────────────────────────────────────────

/// ADR-0116 D1.2/D1.3 — how stale is the store?
///
/// `None` when the store is empty (which is maximally stale). Otherwise the
/// age of the newest snapshot, valid or not: a failed snapshot still proves
/// the cadence RAN, and re-running immediately because the last attempt failed
/// would turn one broken DB into a snapshot storm.
pub fn newest_snapshot_age(store_dir: &Path, now: OffsetDateTime) -> Option<time::Duration> {
    let records = list_snapshots(store_dir).ok()?;
    records.iter().map(|r| r.age(now)).min()
}

/// `true` if the store has no snapshot newer than `window`.
///
/// The shared idempotency predicate behind `--if-stale-secs`, the daemon's
/// catch-up (D1.2), and every D5 trigger. **This is what makes the
/// out-of-process floor and the in-process daemon safe to run together**:
/// whichever fires first satisfies the window and the other no-ops, so
/// scheduling a floor cannot multiply the store's growth rate.
pub fn store_is_stale(store_dir: &Path, window: Duration, now: OffsetDateTime) -> bool {
    match newest_snapshot_age(store_dir, now) {
        None => true,
        Some(age) => age.whole_seconds().max(0) as u64 >= window.as_secs(),
    }
}

/// `aberp snapshot now` — take one managed, validated snapshot immediately
/// and apply retention.
pub fn run_now(args: &SnapshotNowArgs) -> Result<()> {
    let tenant = tenant_id(&args.tenant)?;
    let store_dir = resolve_store(&args.tenant, args.store.as_deref())?;

    // ── ADR-0116 D1.3 — the out-of-process floor's kill switch + window ──
    //
    // `ABERP_SNAPSHOT_DISABLE` turns the IN-PROCESS daemon off. The scheduled
    // floor HONOURS it too: "disabled" must mean disabled, and a backup daemon
    // that ignores its own kill switch is worse than one that can be switched
    // off. But it logs LOUD every time it no-ops for this reason, because a
    // disable set for an unrelated reason must not silently remove the floor —
    // a floor that no-ops silently is indistinguishable from one that never
    // existed, which is exactly the condition G1 measured (18.5 % of cadence).
    if is_disabled() {
        tracing::error!(
            env = POLL_DISABLE_ENV,
            store = %store_dir.display(),
            "ADR-0116 D1.3 — `aberp snapshot now` is NO-OPPING because {POLL_DISABLE_ENV} is \
             set. If this is the scheduled daily floor, the RPO floor is currently ABSENT: \
             nothing outside `aberp serve` is creating rollback points. \
             Magyarul: a pillanatfelvétel ki van kapcsolva — nincs visszaállítási pont.",
        );
        println!("Snapshot skipped: {POLL_DISABLE_ENV} is set. No rollback point was created.");
        return Ok(());
    }
    if let Some(secs) = args.if_stale_secs {
        let now = OffsetDateTime::now_utc();
        if !store_is_stale(&store_dir, Duration::from_secs(secs), now) {
            let age = newest_snapshot_age(&store_dir, now).unwrap_or(time::Duration::ZERO);
            println!(
                "Snapshot skipped: the store already holds a snapshot {} old (< {}s window).",
                human_age(age),
                secs
            );
            return Ok(());
        }
    }

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
            "Snapshot #{} written and validated → {}\n  id={}  invoices={}  audit_rows={}  chain={}  anchors={}  size={}",
            rec.meta.seq,
            rec.dir.display(),
            snapshot_identity(&rec.meta),
            rec.meta.invoice_count,
            rec.meta.audit_count,
            rec.meta.chain_len,
            describe_anchors(&rec.meta),
            human_size(rec.meta.byte_size),
        );
    } else {
        println!(
            "Snapshot #{} FAILED validation (kept as forensic evidence — ADR-0116 G8) → {}\n  id={}\n  reason: {}",
            rec.meta.seq,
            rec.dir.display(),
            snapshot_identity(&rec.meta),
            rec.meta.validation_error.as_deref().unwrap_or("?"),
        );
    }
    Ok(())
}

/// ADR-0116 D4 — what the anchor coverage means for a restore decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorVerdict {
    /// Coverage was never recorded (a pre-D4 snapshot). Not the same as zero.
    NotRecorded,
    /// The anchors table was readable and held nothing — the anchoring
    /// rollout has not happened for this tenant. A fact about the system.
    NoAnchorsAtAll,
    /// Anchors exist but do not cover the chain head — a real gap.
    ShortCoverage,
    /// The chain is fully covered.
    FullCoverage,
}

/// ADR-0116 D4 — the stable slug recorded in the `SnapshotRestored` payload,
/// so the restored chain itself carries the coverage it was restored under.
fn anchor_verdict_slug(v: AnchorVerdict) -> &'static str {
    match v {
        AnchorVerdict::NotRecorded => "not-recorded",
        AnchorVerdict::NoAnchorsAtAll => "no-anchors-at-all",
        AnchorVerdict::ShortCoverage => "short-coverage",
        AnchorVerdict::FullCoverage => "full-coverage",
    }
}

/// The verdict, over whichever set of coverage numbers the caller trusts.
///
/// **ADR-0116 F5, same class, adjacent code.** F5 was "the data-loss gate keys
/// on the RECORDED `meta.audit_count` two lines after saying never to trust the
/// recorded verdict". The anchor sanction had the identical shape: it read
/// `meta.anchor_count`, and `meta.json` is a plain file beside the export with
/// no integrity binding to it. Editing `anchor_count` to `0` there downgrades a
/// Defense `ShortCoverage` REFUSAL to a warning — a one-line bypass of the
/// sanction, in a file an operator can write. The callers below therefore pass
/// the LIVE re-validation's numbers, which are derived from the export bytes.
fn anchor_verdict_of(
    anchor_count: i64,
    anchored_through: Option<u64>,
    chain_len: u64,
) -> AnchorVerdict {
    match (anchor_count, anchored_through) {
        (c, _) if c < 0 => AnchorVerdict::NotRecorded,
        (_, None) => AnchorVerdict::NotRecorded,
        (0, _) => AnchorVerdict::NoAnchorsAtAll,
        (_, Some(through)) if through < chain_len => AnchorVerdict::ShortCoverage,
        _ => AnchorVerdict::FullCoverage,
    }
}

/// The verdict from a LIVE [`validate_export`] re-run — the form every gating
/// and recording caller uses.
fn anchor_verdict_live(live: &aberp_snapshot::ValidationReport) -> AnchorVerdict {
    anchor_verdict_of(live.anchor_count, live.anchored_through_seq, live.chain_len)
}

/// ADR-0116 D4 — render anchor coverage without ever printing `0` for
/// "not recorded". The whole point of the `-1`/`None` sentinels is that an
/// operator deciding whether a restored DB can be relied on in court must be
/// able to tell "checked, none" from "never checked".
fn describe_anchors(meta: &aberp_snapshot::SnapshotMeta) -> String {
    describe_anchor_numbers(meta.anchor_count, meta.anchored_through_seq, meta.chain_len)
}

/// [`describe_anchors`] over a LIVE [`validate_export`] re-run.
fn describe_anchors_live(live: &aberp_snapshot::ValidationReport) -> String {
    describe_anchor_numbers(live.anchor_count, live.anchored_through_seq, live.chain_len)
}

fn describe_anchor_numbers(
    anchor_count: i64,
    anchored_through: Option<u64>,
    chain_len: u64,
) -> String {
    match (anchor_count, anchored_through) {
        (c, _) if c < 0 => "not-recorded".to_string(),
        (c, None) => format!("{c} rows, coverage not-recorded"),
        (c, Some(0)) => format!("{c} rows, NONE verified"),
        (c, Some(through)) => {
            let short = through < chain_len;
            format!(
                "{c} rows, verified through seq {through}/{}{}",
                chain_len,
                if short { " (SHORT)" } else { "" }
            )
        }
    }
}

/// `aberp snapshot list` — show identity / timestamp / size / validation /
/// age, newest first.
pub fn run_list(args: &SnapshotListArgs) -> Result<()> {
    let store_dir = resolve_store(&args.tenant, args.store.as_deref())?;
    let records = list_snapshots(&store_dir).context("list snapshots")?;
    let now = OffsetDateTime::now_utc();

    // ADR-0116 — `--verify` re-runs validation LIVE rather than trusting the
    // recorded verdict. `restore_into` refuses a snapshot that fails, so a
    // bit-rotted store is UNRESTORABLE with no warning until the incident.
    let live: Vec<Option<aberp_snapshot::ValidationReport>> = if args.verify {
        records
            .iter()
            .map(|r| Some(validate_export(&r.dir, &args.tenant)))
            .collect()
    } else {
        records.iter().map(|_| None).collect()
    };

    if args.json {
        let rows: Vec<serde_json::Value> = records
            .iter()
            .zip(live.iter())
            .map(|(r, v)| {
                serde_json::json!({
                    "id": snapshot_identity(&r.meta),
                    "seq": r.meta.seq,
                    "created_at": rfc3339(r.meta.created_at),
                    "source_db_sha256": r.meta.source_db_sha256,
                    "dir": r.dir.display().to_string(),
                    "byte_size": r.meta.byte_size,
                    "age_seconds": r.age(now).whole_seconds(),
                    "meta_version": r.meta.meta_version,
                    "valid_recorded": r.meta.valid,
                    "valid_live": v.as_ref().map(|v| v.ok),
                    "invoice_count": r.meta.invoice_count,
                    "audit_count": r.meta.audit_count,
                    "chain_len": r.meta.chain_len,
                    "anchor_count": r.meta.anchor_count,
                    "anchored_through_seq": r.meta.anchored_through_seq,
                    "validation_error": r.meta.validation_error,
                    // G8 — a retained failed snapshot is FORENSIC, not a
                    // rollback point. A consumer that treats `valid=false` as
                    // "restorable" would be badly wrong.
                    "retained_as": if r.meta.valid { "rollback-point" } else { "forensic-evidence" },
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "store": store_dir.display().to_string(),
                "count": rows.len(),
                "snapshots": rows,
            }))?
        );
        return Ok(());
    }

    if records.is_empty() {
        println!("No snapshots in {}", store_dir.display());
        return Ok(());
    }
    println!("Snapshots in {} (newest first):", store_dir.display());
    println!(
        "  {:<34}  {:>9}  {:<18}  {:<8}",
        "ID (seq@created_at#sha8)", "SIZE", "STATUS", "AGE"
    );
    for (r, v) in records.iter().zip(live.iter()) {
        // ADR-0116 G8 — invalid snapshots are RETAINED now, so `list` must
        // show them distinctly: a rollback store whose newest entries are all
        // invalid is an incident, not an inventory.
        let status = match (r.meta.valid, v.as_ref().map(|v| v.ok)) {
            (true, Some(true)) | (true, None) => "valid".to_string(),
            (true, Some(false)) => "BIT-ROTTED".to_string(),
            (false, Some(true)) => "FORENSIC(now-ok)".to_string(),
            (false, _) => "FORENSIC(invalid)".to_string(),
        };
        println!(
            "  {:<34}  {:>9}  {:<18}  {:<8}",
            snapshot_identity(&r.meta),
            human_size(r.meta.byte_size),
            status,
            human_age(r.age(now)),
        );
        if !r.meta.valid {
            println!(
                "      ↳ kept as forensic evidence (ADR-0116 G8), NOT restorable: {}",
                r.meta.validation_error.as_deref().unwrap_or("?")
            );
        }
        if let Some(v) = v {
            if !v.ok && r.meta.valid {
                println!(
                    "      ↳ LIVE re-validation FAILED — this rollback point has bit-rotted: {}",
                    v.error.as_deref().unwrap_or("?")
                );
            }
        }
    }
    Ok(())
}

/// The pre-flight ADR-0116 D3.2 prints, shared by `--dry-run`,
/// `--verify-only`, and the in-place restore's own pre-flight.
struct RestorePreflight {
    record: SnapshotRecord,
    live: aberp_snapshot::ValidationReport,
    /// Audit rows in the LIVE tenant's durable mirror right now.
    ///
    /// **A LOWER BOUND on the live chain, not the live chain.** The mirror is
    /// extended from the DB by `ensure_consistent_with_db` (every snapshot
    /// cycle) and by the shared `Handle`'s lockstep sync on every WriteGuard
    /// drop — but `Ledger::append`, which the 15 CLI money-submission sites
    /// use, commits WITHOUT syncing the mirror. So with `serve` down the
    /// mirror can lag the DB, in exactly the windows D-22 identified as the
    /// dangerous ones. `mirror > snapshot` therefore PROVES the live DB is
    /// ahead; `mirror == snapshot` proves nothing.
    live_mirror_head: Option<u64>,
    /// Newest live mirror entry's wall time, so "what would I lose" has a
    /// date on it and not just a count.
    live_mirror_newest: Option<String>,
    /// EXACT live counts, available only when the caller held the live DB
    /// exclusively (i.e. `restore --in-place`, which refuses unless serve is
    /// stopped). `None` on the side-path command, where opening the live DB
    /// beside a possibly-running serve is the ADR-0098 two-instance hazard.
    live_exact: Option<LiveCounts>,
    /// **ADR-0116 F4** — the live audit head the D3.3 comparison actually
    /// used: the exact count when serve was stopped and the table readable,
    /// otherwise the mirror's lower bound, otherwise `None`.
    live_head: Option<u64>,
    /// **ADR-0116 F4** — serve was stopped (so the count SHOULD be exact) but
    /// `audit_ledger` could not be read. Distinct from `live_head == None`,
    /// which merely means no source was available.
    live_head_unknown: bool,
    /// The snapshot's audit-row count as re-derived by the LIVE re-validation
    /// (ADR-0116 F5) — never the recorded `meta.audit_count`.
    snapshot_audit: u64,
    /// Reasons the restore would REFUSE. Empty ⇒ it would proceed.
    refusals: Vec<String>,
}

impl RestorePreflight {
    /// **ADR-0116 D3.3** — how many committed audit entries this restore would
    /// throw away. `Some(0)` = none; `None` = the live head is UNKNOWN, which
    /// is never the same statement as "none".
    fn discarded(&self) -> Option<u64> {
        if self.live_head_unknown {
            return None;
        }
        self.live_head
            .map(|h| h.saturating_sub(self.snapshot_audit))
    }
}

/// Exact row counts read from the live DB on a connection the caller already
/// holds exclusively. See [`RestorePreflight::live_exact`].
///
/// **ADR-0116 F4 — `None` means "could not read", NEVER zero.** These used to
/// be plain `i64`s carrying `-1` for an unreadable table (`unwrap_or(-1)`), and
/// `build_preflight` coerced that with `.max(0) as u64` — so a database whose
/// `audit_ledger` could not be read reported a confident **0** into the
/// data-loss arithmetic, `head > snap_audit` was false, and the D3.3
/// acknowledgement gate silently disarmed. In the one scenario a restore exists
/// for. This is the same sentinel discipline the ADR is careful about for
/// `anchor_count` (*"`-1` means not recorded, NEVER zero"*), applied in the
/// place where getting it wrong disarms the safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveCounts {
    pub invoice_count: Option<i64>,
    pub audit_count: Option<i64>,
}

/// **ADR-0116 rev 4 / F2** — the exact `aberp recover` invocation for THIS
/// tenant, ready to paste.
///
/// Every refusal on the damaged-database path ends here, so it is spelled once.
/// It carries `--store` because the pre-flight may be running against an
/// overridden store and a hint that silently pointed at the default one would
/// rebuild from the wrong snapshots — the same "name the thing precisely"
/// discipline the selector fix (F3) landed for.
fn recover_hint(db: &Path, tenant: &str, store_dir: &Path) -> String {
    format!(
        "aberp recover --db {} --tenant {tenant} --store {}",
        db.display(),
        store_dir.display()
    )
}

/// Which shape of interrupted in-place restore boot is looking at.
///
/// **ADR-0116 rev 5 / finding 2.** All three end in the same refusal text, so
/// they are one enum feeding one builder rather than three hand-rolled
/// `bail!`s. The rev-4 refusal was hand-rolled at its single call site and
/// promptly drifted: it offered `aberp restore --in-place` and a store-less
/// `aberp recover`, neither of which can run in the state it fires in, which
/// is F2's defect class reappearing in F2's own commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptedRestore {
    /// A `^C`, OOM kill or power cut inside `restore_in_place`'s
    /// preserve→install window: the live path is EMPTY and the unit is intact.
    LivePathMissing,
    /// The unit's DATABASE was moved back on its own and its `.wal` — holding
    /// every un-checkpointed commit — was left behind in the unit. Boot would
    /// come up CLEAN and EMPTY over it.
    PartlyMovedBack,
    /// A fresh, EMPTY database was provisioned beside an intact unit, and the
    /// live path existing is what stops the `!db.exists()` guard being
    /// consulted again. Counts read from the live DB, not assumed.
    LatchedEmptyDb { invoices: i64, audit: i64 },
}

/// **ADR-0116 rev 5 / finding 2** — the recovery that actually WORKS when a
/// restore was interrupted, spelled once.
///
/// `recover_hint` (above) is the same discipline for the damaged-database
/// path. This is its twin for the interrupted-restore path, and it exists for
/// the same reason: the rev-4 refusal named two routes by hand, and the
/// adversarial ran all three on a genuine `SIGINT` state carrying 40 005
/// invoices —
///
/// | route as the message spelled it | result |
/// |---|---|
/// | `aberp recover --db … --tenant …` | REFUSED — no `--store`, resolves the wrong store |
/// | the same **with** `--store` | REFUSED — the `.audit.log` mirror is inside the unit |
/// | `aberp restore --in-place …` | ABORTS — no live DB to take the mandatory pre-restore snapshot of |
/// | move the unit's DB back **alone** | boots `ok=true`, `counts=(0, 0)` — an EMPTY company |
/// | move **all four** files back | boots, `counts=(40005, 11)` ✅ |
///
/// Only the last one recovers anything, and it is the one the message did not
/// spell out. The `mv` pairs come from `aberp_snapshot::pre_restore_move_back`,
/// derived from the same sibling helpers `restore_in_place` wrote the unit
/// with, so the instructions cannot drift from the unit's shape.
pub(crate) fn interrupted_restore_refusal(
    db: &Path,
    units: &[PathBuf],
    state: InterruptedRestore,
) -> String {
    let db = aberp_snapshot::resolve_db_path(db);
    let unit = units
        .first()
        .cloned()
        .unwrap_or_else(|| db.with_extension("PRE-RESTORE-<tag>"));

    let headline = match state {
        InterruptedRestore::LivePathMissing => format!(
            "REFUSING to boot: the tenant database at {} is MISSING, but a .PRE-RESTORE- unit \
             sits beside it — that is an INTERRUPTED in-place restore (ADR-0116 D3.4), not a \
             first launch. Provisioning a fresh, empty company here would serve a company with \
             no invoices and no audit history while the real one sat on disk unread.\n\n  \
             the previous database is INTACT at {}",
            db.display(),
            unit.display(),
        ),
        InterruptedRestore::PartlyMovedBack => format!(
            "REFUSING to boot: an INTERRUPTED in-place restore (ADR-0116 D3.4) was only PARTLY \
             moved back. The .PRE-RESTORE- unit's database is gone from {} but its siblings are \
             still there — so every un-checkpointed commit is stranded in the orphaned .wal and \
             this database would open CLEAN and EMPTY. Every `Handle` commit is WAL-only until \
             a checkpoint (ADR-0098 R5): a database without its WAL is not a partial recovery, \
             it is a silent one.\n\n  \
             the unit is INCOMPLETE at {} — its database file is not there",
            db.display(),
            unit.display(),
        ),
        InterruptedRestore::LatchedEmptyDb { invoices, audit } => format!(
            "REFUSING to boot: the tenant database at {} is EMPTY ({invoices} invoices, {audit} \
             audit entries) and an intact .PRE-RESTORE- unit is sitting beside it. That is an \
             INTERRUPTED in-place restore (ADR-0116 D3.4) that a previous boot provisioned a \
             fresh company over — serving it would show an empty company while the real one sat \
             on disk unread.\n\n  \
             the previous database is INTACT at {}",
            db.display(),
            unit.display(),
        ),
    };

    let mut out = headline;
    if units.len() > 1 {
        out.push_str(&format!(
            "\n  NOTE: {} PRE-RESTORE units are present — the newest tag is the most recent \
             interruption; inspect all of them before moving any",
            units.len()
        ));
    }

    let moves = aberp_snapshot::pre_restore_move_back(&unit, &db);
    out.push_str(
        "\n\nTo recover, move the WHOLE unit back — the database AND its siblings. Moving only \
         the database leaves its un-checkpointed commits behind in the orphaned .wal and the \
         next boot comes up as an EMPTY company:\n",
    );
    // ADR-0116 rev 5 — the live-side files the move-back does NOT overwrite
    // would otherwise be left pairing with the restored database: a stale
    // empty `.wal` beside a full DB is the F4 hazard wearing the other mask.
    // Only ever emitted for a database this boot has just PROVEN is empty.
    if let InterruptedRestore::LatchedEmptyDb { .. } = state {
        let leftovers: Vec<PathBuf> = [
            db.clone(),
            aberp_snapshot::wal_path_for(&db),
            aberp_audit_ledger::mirror_path_for(&db),
            aberp_snapshot::marker_path(&db),
        ]
        .into_iter()
        .filter(|p| p.exists() && !moves.iter().any(|(_, to)| to == p))
        .collect();
        if !leftovers.is_empty() {
            out.push_str(
                "\n    # this database holds no invoices and no audit entries — verified by \
                 the boot that printed this\n",
            );
            for p in leftovers {
                out.push_str(&format!("    rm {}\n", p.display()));
            }
            out.push('\n');
        }
    }
    for (from, to) in &moves {
        out.push_str(&format!("    mv {} {}\n", from.display(), to.display()));
    }
    out.push_str(
        "\nthen run `aberp serve` again.\n\n\
         Neither `aberp restore --in-place` nor `aberp recover` can run before that move: the \
         first needs a live database to take its mandatory pre-restore snapshot of, and the \
         second needs the live .audit.log mirror — and both of those are inside the unit until \
         you move it back.",
    );
    out
}

/// Build the pre-flight for `selector` against `store_dir`.
///
/// The live side is read from the audit-ledger **mirror file**, never by
/// opening the live database. That is deliberate: `aberp serve` may be
/// running, and a second DuckDB instance on the live file is the ADR-0098
/// two-instance hazard this tree spent three sessions closing. The mirror is
/// the durable record of the chain, so it answers the question that matters
/// ("how many audit rows exist now that would not exist after") without
/// touching the file at all.
fn build_preflight(
    store_dir: &Path,
    selector: &str,
    tenant: &str,
    live_db: &Path,
    live_exact: Option<LiveCounts>,
    accept_data_loss: bool,
) -> Result<RestorePreflight> {
    let record = resolve_selector(store_dir, selector)
        .map_err(|e| anyhow::anyhow!("resolve snapshot selector '{selector}': {e}"))?;
    // Re-run validation LIVE — never trust the recorded verdict for a
    // decision this destructive.
    let live = validate_export(&record.dir, tenant);

    let mirror_path = aberp_audit_ledger::mirror_path_for(live_db);
    let (live_mirror_head, live_mirror_newest) =
        match aberp_audit_ledger::read_mirror_entries(&mirror_path) {
            Ok(entries) => (
                entries.iter().map(|e| e.seq).max(),
                entries.last().map(|e| e.time_wall.clone()),
            ),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    mirror = %mirror_path.display(),
                    "ADR-0116 D3.2 — no readable audit mirror beside the live DB; the \
                     pre-flight reports the snapshot's own numbers without a live delta"
                );
                (None, None)
            }
        };

    let mut refusals = Vec::new();
    if !live.ok {
        refusals.push(format!(
            "snapshot fails validation LIVE: {}",
            live.error.as_deref().unwrap_or("?")
        ));
    }
    if !record.meta.valid {
        refusals.push(format!(
            "snapshot is recorded valid=false (retained as forensic evidence, ADR-0116 G8): {}",
            record.meta.validation_error.as_deref().unwrap_or("?")
        ));
    }
    // "The live DB is ahead" — from the EXACT counts when we have them,
    // otherwise from the mirror's lower bound. Both directions matter: an
    // exact read can also prove the live DB is NOT ahead, which the mirror
    // never can.
    //
    // ADR-0116 F5 — the snapshot side is `live.audit_count`, the number the
    // LIVE re-validation just produced, NOT `record.meta.audit_count`. Two
    // lines above, this function says why it re-validates: *"never trust the
    // recorded verdict for a decision this destructive."* It then keyed the
    // most destructive comparison it makes on the recorded number anyway.
    // Inflating `meta.json`'s `audit_count` to 999999 — export bytes
    // untouched, so the live re-validation still reported the true 3 — let an
    // UNACKNOWLEDGED rollback proceed and discard 7 committed audit rows.
    // `meta.json` is a plain file beside the export with no integrity binding
    // to it; it is evidence, not authority.
    let snap_audit = live.audit_count.max(0) as u64;
    // ADR-0116 F4 / D3.3 — three states, not two. `Some` = a number we can
    // reason about; `None` = we could NOT read the live head and must say so.
    // `live_exact` being present but carrying `None` (serve was stopped and
    // the table was unreadable) is the dangerous case: it must not silently
    // degrade to the mirror's bound either, because the mirror is a LOWER
    // bound and would report "nothing newer" about a database whose rows
    // cannot be counted at all.
    let live_head: Option<u64> = match live_exact {
        Some(c) => c.audit_count.map(|n| n.max(0) as u64),
        None => live_mirror_head,
    };
    let live_head_unknown = matches!(live_exact, Some(c) if c.audit_count.is_none());
    if live_head_unknown && !accept_data_loss {
        // **ADR-0116 rev 4 / F2 — name the tool that WORKS, not a flag that
        // cannot.**
        //
        // This message used to end *"Pass --accept-data-loss to proceed
        // anyway"*. It cannot: the operator types the flag, this refusal
        // clears, and the command aborts one step later because the MANDATORY
        // pre-restore snapshot of the current database cannot validate a
        // database whose `audit_ledger` is unreadable (`!pre_snapshot.meta.
        // valid` → bail). Verified across every flag combination, including
        // `--accept-data-loss --accept-unanchored`, on both an unreadable table
        // and a tampered chain. No flag gets `restore --in-place` through the
        // damaged-DB case, which is the ONE case this whole programme exists
        // for, and neither refusal named the command that does work.
        //
        // The abort itself is right and stays exactly as it is — a database
        // that cannot be snapshotted cannot be safely replaced. What was wrong
        // is the operator contract: the product told an operator at 02:00 to
        // type a flag it knew would fail. `aberp recover` rebuilds from the
        // snapshot store WITHOUT requiring the damaged database to be
        // snapshottable first, and it is the route nobody named.
        let recover_cmd = recover_hint(live_db, tenant, store_dir);
        refusals.push(format!(
            "the LIVE database's audit_ledger could NOT be read, so how much this restore \
             would discard is UNKNOWN — not zero. The snapshot carries {snap_audit} audit \
             entries. A database whose tables cannot be read is exactly the case a restore \
             exists for, and it is also the case in which nothing can prove the rollback is \
             safe.\n    --accept-data-loss does NOT get past this: it clears this refusal and \
             the command then ABORTS at the mandatory pre-restore snapshot, which cannot \
             validate a database whose audit_ledger is unreadable. Use `aberp recover` \
             instead — it rebuilds from the snapshot store and does not require the damaged \
             database to be snapshottable first:\n      {recover_cmd}"
        ));
    } else if live_head_unknown {
        tracing::warn!(
            snapshot_audit = snap_audit,
            "ADR-0116 D3.3 — --accept-data-loss was passed: this restore will DISCARD \
             committed audit entries  live_head=UNKNOWN (the live audit_ledger could not be \
             read) snapshot_audit={snap_audit} discarded=UNKNOWN"
        );
    }
    if let Some(head) = live_head {
        if head > snap_audit && !accept_data_loss {
            // ADR-0116 D3.3 — "refuse when the live DB is AHEAD of the snapshot
            // IN A WAY THE OPERATOR HAS NOT ACKNOWLEDGED". Rolling backwards
            // past committed rows is a legitimate operation — it is what a
            // rollback IS — so this is an acknowledgement gate, not a ban. What
            // it removes is the silent case: nothing previously compared the
            // snapshot against the live DB, so the operator had no
            // machine-checked answer to "am I about to roll back 5 days of
            // invoices?".
            refusals.push(format!(
                "the LIVE database is AHEAD of this snapshot: it holds {head} audit entries and \
                 the snapshot carries {snap_audit}. Restoring would DISCARD {} committed audit \
                 entries. Pick a newer snapshot, or pass --accept-data-loss to acknowledge the \
                 rollback deliberately.",
                head - snap_audit,
            ));
        } else if head > snap_audit {
            tracing::warn!(
                live_head = head,
                snapshot_audit = snap_audit,
                discarded = head - snap_audit,
                "ADR-0116 D3.3 — --accept-data-loss was passed: this restore will DISCARD \
                 committed audit entries"
            );
        }
    }
    Ok(RestorePreflight {
        record,
        live,
        live_mirror_head,
        live_mirror_newest,
        live_exact,
        live_head,
        live_head_unknown,
        snapshot_audit: snap_audit,
        refusals,
    })
}

fn print_preflight(pf: &RestorePreflight, target: &Path, with_delta: bool) {
    let m = &pf.record.meta;
    let now = OffsetDateTime::now_utc();
    println!("Restore pre-flight (ADR-0116 D3.2 — NOTHING is written):");
    println!("  snapshot id      {}", snapshot_identity(m));
    println!("  directory        {}", pf.record.dir.display());
    println!(
        "  taken            {}  ({} ago)",
        rfc3339(m.created_at),
        human_age(pf.record.age(now))
    );
    println!("  size             {}", human_size(m.byte_size));
    println!("  source DB sha256 {}", m.source_db_sha256);
    println!(
        "  validation       recorded={}  live-rerun={}{}",
        if m.valid { "valid" } else { "INVALID" },
        if pf.live.ok { "valid" } else { "INVALID" },
        pf.live
            .error
            .as_deref()
            .map(|e| format!("  ({e})"))
            .unwrap_or_default()
    );
    println!(
        "  contents         invoices={}  audit_rows={}  chain_len={}",
        m.invoice_count, m.audit_count, m.chain_len
    );
    // ADR-0116 F5, same class — the LIVE re-validation is the authority.
    // `meta.json` is a plain file beside the export with no integrity binding
    // to it, and it is what the Defense anchor sanction used to read. Both are
    // printed when they disagree, because a disagreement is itself the signal.
    let anchors_live = describe_anchors_live(&pf.live);
    let anchors_recorded = describe_anchors(m);
    if anchors_live == anchors_recorded {
        println!("  anchors          {anchors_live}");
    } else {
        println!(
            "  anchors          {anchors_live}  (LIVE re-validation; meta.json RECORDS \
             {anchors_recorded} — they DISAGREE, and the live number is the one that gates)"
        );
    }
    println!(
        "  target           {}  ({})",
        target.display(),
        if target.exists() {
            "EXISTS — would be overwritten"
        } else {
            "does not exist — would be created"
        }
    );
    if with_delta {
        // ADR-0116 F5 — the snapshot's side of the comparison is the LIVE
        // re-validation's number, matching what `build_preflight` decided on.
        let snap = pf.snapshot_audit;
        match pf.live_exact {
            // ADR-0116 F4 — serve is stopped, but the audit table could not be
            // read. The one thing this must NOT print is `EXACT … 0`: the gate
            // used to coerce the `-1` sentinel with `.max(0)` and report a
            // confident zero, which is how it disarmed itself in exactly the
            // scenario a restore is for.
            Some(c) if c.audit_count.is_none() => {
                println!(
                    "  live delta       UNKNOWN (serve is stopped, but the live DB's \
                     audit_ledger could NOT be read); this snapshot carries {snap} audit \
                     entries and {} invoices",
                    m.invoice_count
                );
                println!(
                    "                   → how much would be discarded is UNKNOWN, which is NOT \
                     the same as zero. ADR-0116 rev 4 / F2 — no flag gets an in-place restore \
                     past this; `aberp recover` is the route that works (see the refusal \
                     below)."
                );
            }
            // EXACT — the caller holds the live DB exclusively (serve is
            // stopped), so this is the true delta in both directions.
            Some(c) => {
                let head = c.audit_count.unwrap_or(0).max(0) as u64;
                println!(
                    "  live delta       EXACT (serve is stopped): the live DB holds {head} audit \
                     entries and {} invoices; this snapshot carries {snap} / {}",
                    c.invoice_count
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "UNKNOWN".to_string()),
                    m.invoice_count
                );
                if head > snap {
                    println!(
                        "                   → {} audit entries exist NOW that would NOT exist \
                         after the restore",
                        head - snap
                    );
                } else {
                    println!("                   → nothing newer than the snapshot would be lost");
                }
            }
            // BOUND — `serve` may be running, so the live DB is deliberately
            // not opened (a second DuckDB instance on a live file is the
            // ADR-0098 two-instance hazard). The mirror is the durable record
            // and gives a LOWER bound.
            None => match pf.live_mirror_head {
                Some(head) => {
                    println!(
                        "  live delta       LOWER BOUND from the durable audit mirror: it holds \
                         {head} entries; this snapshot carries {snap}"
                    );
                    if head > snap {
                        println!(
                            "                   → at least {} audit entries exist NOW that would \
                             NOT exist after the restore{}",
                            head - snap,
                            pf.live_mirror_newest
                                .as_deref()
                                .map(|t| format!(" (newest: {t})"))
                                .unwrap_or_default()
                        );
                    } else {
                        println!(
                            "                   → the mirror shows nothing newer. NOTE: this is \
                             a LOWER BOUND, not proof. `Ledger::append` — which the CLI \
                             money-submission paths use — commits WITHOUT syncing the mirror, so \
                             with `serve` down the mirror can lag the DB. For an exact delta, \
                             stop serve and use `aberp restore --in-place --dry-run`."
                        );
                    }
                }
                None => println!(
                    "  live delta       UNAVAILABLE — no readable audit mirror beside the live \
                     DB, and the live database is deliberately not opened here (a second DuckDB \
                     instance on a live file is the ADR-0098 hazard)."
                ),
            },
        }
    }
    if pf.refusals.is_empty() {
        println!("\n  VERDICT: would PROCEED.");
    } else {
        println!("\n  VERDICT: would REFUSE —");
        for r in &pf.refusals {
            println!("    • {r}");
        }
    }
}

/// `aberp snapshot restore <selector> --to <path> --confirm` — guarded
/// restore to a SIDE PATH. Refuses without `--confirm` or onto any live
/// `~/.aberp` DB, BEFORE touching the store (`[[trust-code-not-operator]]`).
pub fn run_restore(args: &SnapshotRestoreArgs) -> Result<()> {
    let store_dir = resolve_store(&args.tenant, args.store.as_deref())?;

    // ── ADR-0116 D3.2 — dry-run / verify-only write NOTHING ─────────────
    if args.dry_run || args.verify_only {
        // `None` for the exact counts: this command restores to a SIDE path
        // and `aberp serve` may well be running, so the live DB is
        // deliberately not opened (ADR-0098 two-instance hazard). The delta
        // falls back to the mirror's lower bound, and the report says so.
        let pf = build_preflight(
            &store_dir,
            &args.selector,
            &args.tenant,
            &args.db,
            None,
            args.accept_data_loss,
        )?;
        print_preflight(&pf, &args.to, args.dry_run);
        if pf.refusals.is_empty() {
            return Ok(());
        }
        // Exit code distinguishes "would proceed" from "would refuse", so the
        // scheduled floor's `list --verify` job and any operator script can
        // branch on it without parsing prose.
        anyhow::bail!(
            "pre-flight REFUSES this restore ({} reason(s) above); nothing was written",
            pf.refusals.len()
        );
    }

    // Guard first — the safety lives in the binary, not the operator.
    ensure_restore_allowed(&args.to, args.confirm).map_err(|e| anyhow::anyhow!("{e}"))?;

    let tenant = tenant_id(&args.tenant)?;
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
        "Restored snapshot {} → {}\n(verify it; for an IN-PLACE recovery use `aberp restore \
         --in-place`, which journals the swap instead of leaving it to hand)",
        snapshot_identity(&rec.meta),
        args.to.display()
    );
    Ok(())
}

/// `aberp snapshot prune` — ADR-0116, retention on demand.
pub fn run_prune(args: &SnapshotPruneArgs) -> Result<()> {
    let tenant = tenant_id(&args.tenant)?;
    let store_dir = resolve_store(&args.tenant, args.store.as_deref())?;
    let policy = policy_from_env();
    let records = list_snapshots(&store_dir).context("list snapshots for retention")?;
    let plan = plan_retention(&records, &policy, OffsetDateTime::now_utc());

    println!(
        "Retention plan for {} ({} snapshots):",
        store_dir.display(),
        records.len()
    );
    println!("  keep  {:?}", plan.keep);
    println!("  prune {:?}", plan.prune);
    for r in records.iter().filter(|r| !r.meta.valid) {
        println!(
            "  (forensic) seq {} is INVALID and retained as evidence — ADR-0116 G8",
            r.meta.seq
        );
    }
    if args.dry_run || !args.confirm {
        println!(
            "\nNothing removed.{}",
            if args.dry_run {
                ""
            } else {
                " Pass --confirm to apply, or --dry-run to silence this line."
            }
        );
        return Ok(());
    }
    let binary_hash = crate::binary_hash::compute().context("compute binary hash")?;
    let actor = cli_actor("system:snapshot-cli");
    let removed = retention_and_emit(
        &SnapshotAudit::Reopen,
        &args.db,
        &store_dir,
        &tenant,
        binary_hash,
        actor,
        &policy,
    )?;
    println!("\nRemoved {} snapshot(s): {:?}", removed.len(), removed);
    Ok(())
}

/// **ADR-0116 D3.4** — `aberp restore --in-place`, the guarded in-place
/// restore that replaces the documented hand-swap.
pub fn run_restore_in_place(args: &RestoreInPlaceArgs) -> Result<()> {
    if !args.in_place {
        anyhow::bail!(
            "`aberp restore` has no mode other than --in-place. To restore to a side path use \
             `aberp snapshot restore <selector> --to <path> --confirm`."
        );
    }
    let tenant = tenant_id(&args.tenant)?;
    let store_dir = resolve_store(&args.tenant, args.store.as_deref())?;
    ensure_not_prod_path(&args.db)
        .map_err(|e| anyhow::anyhow!("in-place restore target must not be the prod line: {e}"))?;

    // Step 1 FIRST, even for a dry-run: it is a read-only probe, and holding
    // the file exclusively is what lets the pre-flight report an EXACT live
    // delta instead of the mirror's lower bound. A dry-run that cannot take
    // the lock still reports — it just says so.
    let live_exact = match ensure_serve_is_stopped(&args.db) {
        Ok(c) => c,
        Err(e) if args.dry_run || !args.confirm => {
            tracing::warn!(
                error = %e,
                "ADR-0116 D3.2 — could not take the live DB exclusively for the pre-flight; \
                 the live delta falls back to the audit mirror's LOWER BOUND"
            );
            None
        }
        Err(e) => return Err(e),
    };

    let pf = build_preflight(
        &store_dir,
        &args.snapshot,
        &args.tenant,
        &args.db,
        live_exact,
        args.accept_data_loss,
    )?;

    // ── D4 — the Defense anchor sanction, at RESTORE time ───────────────
    //
    // Defense's premise is court-admissibility, so the instinct to hard-gate
    // anchors is right in spirit and wrong in placement: a VALIDATION gate
    // punishes the snapshot, and `plan_retention` prunes invalid snapshots —
    // so gating there would delete the rollback store. The sanction belongs
    // where the legal claim is actually made, which is here.
    //
    // **DRIFT FROM THE ADR, taken deliberately — flagged for review.** The ADR
    // says "REFUSES without --accept-unanchored when anchored_through_seq <
    // chain_len", which today means EVERY Defense in-place restore refuses:
    // every audit_ledger_anchors.parquet in both live stores is exactly 300
    // bytes, i.e. zero anchor rows everywhere. A flag that must always be
    // passed is muscle-memoried within a week and stops being a decision — and
    // it would add a step to the most stressful operation in the product,
    // typed at 02:00 during an incident. ADR-0116 Phase 3 already says the
    // Defense refusal "waits on real anchor coverage existing".
    //
    // So the sanction is armed on the case it was designed for and loud on the
    // case it was not:
    //   * anchoring IS running and this snapshot is SHORT (anchor_count > 0,
    //     coverage < chain_len)  -> REFUSE without --accept-unanchored;
    //   * anchoring has not rolled out at all (anchor_count == 0)  -> proceed,
    //     with a LOUD warning stating exactly what the restored DB cannot
    //     prove. That is a fact about the system, not about this snapshot.
    //   * coverage NOT RECORDED (a pre-D4 snapshot) -> proceed with a warning;
    //     refusing would make every pre-D4 snapshot unrestorable.
    let anchor_verdict = anchor_verdict_live(&pf.live);
    let is_defense = matches!(
        crate::build_profile::EDITION,
        crate::build_profile::Edition::Defense
    );
    let anchor_refusal = match anchor_verdict {
        AnchorVerdict::ShortCoverage if is_defense && !args.accept_unanchored => Some(format!(
            "this snapshot's audit chain is only PARTIALLY covered by verified timestamp \
             anchors ({}). Anchoring is running for this tenant, so a short chain is a real \
             gap, not an un-rolled-out feature. On Defense a restored database is expected to \
             carry its eIDAS Art. 41(2) weight, which comes from the qualified timestamp over \
             the chain head, not from the hash chain. Pass --accept-unanchored to proceed with \
             a database that cannot prove when those entries were made. \
             Magyarul: a visszaállított lánc időbélyeg-fedezete hiányos.",
            describe_anchors_live(&pf.live)
        )),
        AnchorVerdict::NoAnchorsAtAll => {
            tracing::warn!(
                anchors = %describe_anchors_live(&pf.live),
                "ADR-0116 D4 — this snapshot carries NO timestamp anchors at all. The restored \
                 database will hold a verifiable hash chain but will NOT carry the eIDAS \
                 Art. 41(2) presumption: nothing proves WHEN its entries were made. This is \
                 the pre-anchoring-rollout state of the whole system, not a defect in this \
                 snapshot."
            );
            None
        }
        AnchorVerdict::NotRecorded => {
            tracing::warn!(
                "ADR-0116 D4 — this snapshot predates anchor recording; its eIDAS coverage is \
                 UNKNOWN, not zero. Refusing on it would make every pre-D4 snapshot \
                 unrestorable."
            );
            None
        }
        _ => None,
    };

    if args.dry_run || !args.confirm {
        print_preflight(&pf, &args.db, true);
        println!(
            "\n  This is an IN-PLACE restore: {} would be moved aside as a .PRE-RESTORE-<tag> \
             unit (DB + .wal + .ckpt-ok + .audit.log mirror) and replaced. The mirror moves \
             WITH the database it belongs to, and a FRESH mirror is written for the restored \
             chain, so the next `aberp serve` boot has nothing to reconcile.",
            args.db.display()
        );
        if let Some(r) = &anchor_refusal {
            println!("\n  WOULD ALSO REFUSE (ADR-0116 D4):\n    • {r}");
        }
        if !args.dry_run {
            anyhow::bail!("nothing was written — pass --confirm to perform the restore");
        }
        if !pf.refusals.is_empty() {
            anyhow::bail!(
                "pre-flight REFUSES this restore ({} reason(s) above); nothing was written",
                pf.refusals.len()
            );
        }
        return Ok(());
    }
    let mut all_refusals = pf.refusals.clone();
    if let Some(r) = anchor_refusal {
        all_refusals.push(r);
    }
    if !all_refusals.is_empty() {
        anyhow::bail!(
            "REFUSING this in-place restore:\n{}\nNothing was written. Re-run with --dry-run to \
             see the full pre-flight.",
            all_refusals
                .iter()
                .map(|r| format!("  • {r}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    // Step 1 was performed above (the exclusive probe that also produced the
    // exact live counts). Re-assert it here so a serve started between the
    // pre-flight and the commit cannot slip through: an in-place swap under a
    // live writer strands the shared connection on an unlinked inode and every
    // later commit is lost (ADR-0111).
    ensure_serve_is_stopped(&args.db)?;

    let binary_hash = crate::binary_hash::compute().context("compute binary hash")?;
    let actor = cli_actor("system:restore-cli");
    // NOTE: no retention pass here, deliberately. The pre-restore snapshot
    // goes through `take_and_emit`, not `run_cycle`, so nothing is pruned
    // during a restore. Pruning at the moment an operator is recovering is how
    // G8's forensic evidence got destroyed in the first place — the cycle that
    // creates a snapshot should not also be the cycle that deletes one, least
    // of all here.

    // ── Step 2 — snapshot the CURRENT database FIRST (G7's sharpest case) ─
    //
    // Restoring is the single most destructive operation the system offers,
    // and until now it did not first preserve what it was about to overwrite.
    //
    // F5 — this runs `take_snapshot`, which unconditionally runs
    // `ensure_consistent_with_db` on the live DB, and THAT can trim the live
    // mirror in place and mint a `.bak`. It runs BEFORE anything is
    // overwritten, on the DB that is about to be replaced, and it is the same
    // reconcile the 4-hourly daemon has been performing all along — so it
    // introduces no new behaviour, only a new occasion. It is logged as a
    // DISTINCT pre-restore reconcile so a mirror trim in this window is
    // attributable, and a mirror that is ahead or deeply corrupt at the moment
    // of an in-place restore ABORTS the restore rather than being logged past.
    tracing::warn!(
        db = %args.db.display(),
        "ADR-0116 D3.4 step 2 / F5 — PRE-RESTORE snapshot of the live database (this \
         reconciles the audit mirror; any outcome other than clean/extended aborts)"
    );
    let pre_snapshot = take_and_emit(
        &SnapshotAudit::Reopen,
        &args.db,
        &store_dir,
        &tenant,
        binary_hash,
        actor.clone(),
    )
    .context("ADR-0116 D3.4 step 2 — mandatory pre-restore snapshot of the live database")?;
    if !pre_snapshot.meta.valid {
        // ADR-0116 rev 4 / F2 — this abort is the second half of the dead-end
        // the pre-flight used to send operators down, and it is the one they
        // reach AFTER typing the flag the pre-flight recommended. The refusal
        // stands; what it must not do is stop without naming the command that
        // works. `aberp recover` rebuilds from the store without needing the
        // damaged database to be snapshottable first.
        anyhow::bail!(
            "ABORTING the in-place restore: the mandatory pre-restore snapshot of the CURRENT \
             database failed validation ({}). The live database is untouched. A database that \
             cannot be snapshotted cannot be safely replaced — investigate first; the failed \
             snapshot is retained at {} as forensic evidence (ADR-0116 G8).\n\n  \
             No `restore --in-place` flag gets past this — `--accept-data-loss` clears the \
             data-loss gate but not this one. `aberp recover` is the tool for a damaged live \
             database: it rebuilds from the snapshot store without snapshotting the damaged \
             one first:\n      {}",
            pre_snapshot.meta.validation_error.as_deref().unwrap_or("?"),
            pre_snapshot.dir.display(),
            recover_hint(&args.db, &args.tenant, &store_dir),
        );
    }
    println!(
        "Pre-restore snapshot of the CURRENT database: {} → {}",
        snapshot_identity(&pre_snapshot.meta),
        pre_snapshot.dir.display()
    );

    // ── Steps 3-6 — preserve the unit, install, re-marker, re-verify ────
    let tag = restore_tag(OffsetDateTime::now_utc());
    let report = restore_in_place(&pf.record.dir, &args.db, &args.tenant, &tag)
        .map_err(|e| anyhow::anyhow!("ADR-0116 D3.4 in-place restore: {e}"))?;

    // ── Step 7 — the restore event, on the RESTORED chain ───────────────
    //
    // ADR-0116 D3.5 / F8 — WHICH ledger differs per command, and a single
    // global rule would regress shipped behaviour. `aberp snapshot restore`
    // (side path) writes to the LIVE ledger, deliberately, so the operator's
    // main timeline shows a restore happened. Here the live DB IS the restored
    // DB, so the two collapse and the row is simply the next seq on the
    // restored chain. No pre-seeded seq, no out-of-band edit of the restored
    // file (the 2026-08-03 heal-path lesson), and no mirror reconcile AFTER
    // the install — if the mirror disagrees with the restored DB, that is
    // `recover_or_refuse`'s decision at next boot.
    // ADR-0116 D4 / D3.3 — the restored chain records what it was restored
    // UNDER: the anchor verdict (so the database itself says "this chain had
    // no timestamp coverage at restore time") and how much committed audit
    // history the operator acknowledged discarding.
    let payload = crate::audit_payloads::SnapshotRestoredPayload {
        seq: pf.record.meta.seq,
        snapshot_dir: pf.record.dir.display().to_string(),
        target: args.db.display().to_string(),
        restored_at: rfc3339(OffsetDateTime::now_utc()),
        anchor_verdict: anchor_verdict_slug(anchor_verdict).to_string(),
        anchor_coverage: describe_anchors_live(&pf.live),
        discarded_audit_rows: pf.discarded(),
    };
    emit_snapshot_event(
        &SnapshotAudit::Reopen,
        &args.db,
        &tenant,
        binary_hash,
        EventKind::SnapshotRestored,
        payload.to_bytes(),
        actor,
    )
    .context("append SnapshotRestored to the RESTORED chain (ADR-0116 D3.5)")?;

    println!(
        "\nIn-place restore complete.\n  restored from  {}\n  into           {}\n  preserved      {}\n                 {}\n                 {}\n                 {}\n  re-verified    invoices={} audit_rows={} chain={}  (read back from the INSTALLED \
database, ADR-0116 F2)\n  fresh mirror   {}\n  anchors        {}\n  discarded      {}\n\nThe .audit.log mirror moved INTO the .PRE-RESTORE- unit and a fresh one was written \
from the restored chain, so the next `aberp serve` boot has nothing to reconcile. The \
preserved mirror is protected recovery evidence (ADR-0116 D2) and holds the audit tail \
this rollback discarded.",
        snapshot_identity(&pf.record.meta),
        args.db.display(),
        report.preserved.db.display(),
        report
            .preserved
            .wal
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no WAL to preserve)".into()),
        report
            .preserved
            .ckpt_ok
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no checkpoint marker to preserve)".into()),
        report
            .preserved
            .mirror
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no audit mirror to preserve)".into()),
        report.installed.invoice_count,
        report.installed.audit_count,
        report.installed.chain_len,
        report
            .mirror_entries_written
            .map(|n| format!("{n} entries written for the restored chain"))
            .unwrap_or_else(|| {
                "NOT written — the live path has no mirror; the next boot creates one from the \
                 DB (RecoveryAction::Created)"
                    .to_string()
            }),
        describe_anchors_live(&pf.live),
        // ADR-0116 D3.3 — the discarded count lands on stdout, not only in a
        // `tracing::warn!`. "I threw away 7 committed audit entries" must be
        // readable in the operator's own terminal transcript, and it is now
        // also on the restored chain (see the payload above).
        match pf.discarded() {
            Some(0) => "0 audit entries (the snapshot was not behind the live DB)".to_string(),
            Some(n) => format!(
                "{n} committed audit entries DISCARDED — acknowledged with --accept-data-loss"
            ),
            None => "UNKNOWN — the live audit_ledger could not be read before the restore"
                .to_string(),
        },
    );
    Ok(())
}

/// ADR-0116 D3.4 step 1 — refuse unless `aberp serve` is stopped.
///
/// DuckDB holds an exclusive file lock while a read-write connection is open,
/// so an exclusive open failing IS the signal that serve is up. **Fail on the
/// lock; never race it** — an in-place swap under a live writer is precisely
/// the orphaned-inode failure ADR-0111 closed (the shared connection keeps an
/// fd on the old inode while the rename installs a new one, and every later
/// commit lands in a file the kernel frees at exit).
fn ensure_serve_is_stopped(db_path: &Path) -> Result<Option<LiveCounts>> {
    if !db_path.exists() {
        return Ok(None);
    }
    // SANCTIONED RESIDUAL (ADR-0098 category (c): CLI-only one-shot, separate
    // process). DuckDB's own exclusive file lock is the ground truth for "is
    // serve running" — and the alternatives are wrong in the DANGEROUS
    // direction: a liveness touchfile is stale after a crash, so it would
    // answer "serve is stopped" while serve is up, and an in-place swap under
    // a live writer strands its connection on an unlinked inode (ADR-0111).
    // Frozen in tools/adr0098_c2_frozen_residuals.txt and
    // tools/adr0098_r4_opener_fingerprints.txt.
    let probe = duckdb::Connection::open(db_path);
    match probe {
        Ok(conn) => {
            // ADR-0098 C2 — never let this probe's drop fold the WAL in place.
            let _ = conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;");
            // ADR-0116 D3.2 — read the EXACT live counts on THIS connection.
            //
            // No second opener: we already hold the file exclusively, and this
            // is the only moment in the product where reading the live DB is
            // provably safe (serve is stopped, which is what this function has
            // just established). The alternative — the audit MIRROR — is only
            // a lower bound, because `Ledger::append` commits without syncing
            // it, so the 15 CLI money-submission sites (D-22) can leave the
            // mirror lagging the DB in exactly the serve-down windows where an
            // operator reaches for a restore.
            // ADR-0116 F4 — `.ok()`, not `unwrap_or(-1)`. An unreadable table
            // is UNKNOWN and must stay unknown all the way to the refusal
            // arithmetic; a sentinel that looks like a number gets treated
            // like one two functions away.
            let audit_count: Option<i64> = conn
                .query_row("SELECT count(*) FROM audit_ledger", [], |r| r.get(0))
                .ok();
            let invoice_count: Option<i64> = conn
                .query_row("SELECT count(*) FROM invoice", [], |r| r.get(0))
                .ok();
            drop(conn);
            Ok(Some(LiveCounts {
                invoice_count,
                audit_count,
            }))
        }
        Err(e) => Err(anyhow::anyhow!(
            "REFUSING the in-place restore: the live database at {} could not be opened \
             exclusively ({e}). `aberp serve` is almost certainly running. Stop it and re-run — \
             swapping the file under a live writer strands its connection on an unlinked inode \
             and every later commit is lost (ADR-0111). \
             Magyarul: állítsd le az `aberp serve`-t a visszaállítás előtt.",
            db_path.display()
        )),
    }
}

/// `PRE-RESTORE-<tag>` timestamp, ISO-shaped so it groups with every other
/// evidence artefact under the ADR-0116 D2 incident keying.
fn restore_tag(now: OffsetDateTime) -> String {
    use time::macros::format_description;
    const TS: &[time::format_description::FormatItem<'_>] =
        format_description!("[year][month][day]T[hour][minute][second]Z");
    now.format(TS)
        .unwrap_or_else(|_| now.unix_timestamp().to_string())
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0116 D2 — the evidence commands
// ──────────────────────────────────────────────────────────────────────

/// Resolve the tenant home `~/.aberp-<edition>/<tenant>/`, or an explicit
/// override. Refused if it points at the frozen prod line.
pub fn resolve_tenant_home(tenant: &str, explicit: Option<&Path>) -> Result<PathBuf> {
    let home = match explicit {
        Some(p) => p.to_path_buf(),
        None => {
            let base = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("HOME is not set — cannot resolve tenant home"))?;
            base.join(crate::build_profile::edition_data_dirname())
                .join(tenant)
        }
    };
    ensure_not_prod_path(&home)
        .map_err(|e| anyhow::anyhow!("tenant home must not be under the frozen prod line: {e}"))?;
    Ok(home)
}

/// The archive store `~/Documents/ABERP-evidence/`, mirroring the snapshot
/// store's "outside the repo, outside `~/.aberp/`" property.
fn resolve_archive_root(explicit: Option<&Path>) -> Result<PathBuf> {
    let root = match explicit {
        Some(p) => p.to_path_buf(),
        None => {
            let base = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("HOME is not set — cannot resolve archive root"))?;
            base.join("Documents").join("ABERP-evidence")
        }
    };
    ensure_not_prod_path(&root)
        .map_err(|e| anyhow::anyhow!("archive root must not be under the frozen prod line: {e}"))?;
    Ok(root)
}

/// `aberp evidence list` — the ~600 MB nobody could previously see.
pub fn run_evidence_list(args: &EvidenceListArgs) -> Result<()> {
    let home = resolve_tenant_home(&args.tenant, args.home.as_deref())?;
    let artefacts = aberp_snapshot::list_evidence(&home)
        .map_err(|e| anyhow::anyhow!("list evidence in {}: {e}", home.display()))?;
    let policy = aberp_snapshot::EvidencePolicy::default();
    let dispositions =
        aberp_snapshot::plan_evidence_release(&artefacts, &policy, OffsetDateTime::now_utc());
    let total: u64 = artefacts.iter().map(|a| a.byte_size).sum();

    if args.json {
        let rows: Vec<serde_json::Value> = dispositions
            .iter()
            .map(|d| {
                serde_json::json!({
                    "name": d.artefact.name,
                    "path": d.artefact.path.display().to_string(),
                    "byte_size": d.artefact.byte_size,
                    "modified_at": rfc3339(d.artefact.modified_at),
                    "incident_tag": d.artefact.incident_tag,
                    "credential_material": d.artefact.is_credential_material,
                    "releasable": d.retained_because.is_none(),
                    "retained_because": d.retained_because.as_ref().map(|r| format!("{r:?}")),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "tenant_home": home.display().to_string(),
                "count": rows.len(),
                "total_bytes": total,
                "artefacts": rows,
            }))?
        );
        return Ok(());
    }

    if artefacts.is_empty() {
        println!("No recovery evidence in {}", home.display());
        return Ok(());
    }
    println!(
        "Recovery evidence in {} — {} artefact(s), {}:",
        home.display(),
        artefacts.len(),
        human_size(total)
    );
    println!(
        "  {:<58}  {:>9}  {:<18}  STATUS",
        "NAME", "SIZE", "INCIDENT"
    );
    for d in &dispositions {
        println!(
            "  {:<58}  {:>9}  {:<18}  {}",
            d.artefact.name,
            human_size(d.artefact.byte_size),
            d.artefact.incident_tag.as_deref().unwrap_or("(untagged)"),
            match &d.retained_because {
                None => "releasable".to_string(),
                Some(r) => format!("RETAINED: {r:?}"),
            }
        );
    }
    println!(
        "\nNothing here is ever deleted by the periodic daemon. `aberp evidence archive` \
         copies releasable artefacts to the archive store, verifies the copy, and only then \
         unlinks the original — release is never deletion."
    );
    Ok(())
}

/// `aberp evidence archive` — the ONLY sanctioned release path.
pub fn run_evidence_archive(args: &EvidenceArchiveArgs) -> Result<()> {
    let tenant = tenant_id(&args.tenant)?;
    let home = resolve_tenant_home(&args.tenant, args.home.as_deref())?;
    let archive_root = resolve_archive_root(args.archive_root.as_deref())?;
    let artefacts = aberp_snapshot::list_evidence(&home)
        .map_err(|e| anyhow::anyhow!("list evidence in {}: {e}", home.display()))?;

    // The 90-day floor cannot be LOWERED by the flag: `--older-than-days`
    // narrows the release, never widens it. An operator who wants a shorter
    // floor is asking to delete evidence from a live incident.
    let default = aberp_snapshot::EvidencePolicy::default();
    let policy = aberp_snapshot::EvidencePolicy {
        age_floor_days: args.older_than_days.max(default.age_floor_days),
        ..default
    };
    if args.older_than_days < default.age_floor_days {
        tracing::warn!(
            requested = args.older_than_days,
            enforced = policy.age_floor_days,
            "ADR-0116 D2 — --older-than-days is below the {}-day policy floor and was RAISED to \
             it. The floor narrows a release, never widens one.",
            default.age_floor_days
        );
    }
    let dispositions =
        aberp_snapshot::plan_evidence_release(&artefacts, &policy, OffsetDateTime::now_utc());
    let releasable: Vec<_> = dispositions
        .iter()
        .filter(|d| d.retained_because.is_none())
        .collect();

    println!(
        "Evidence release plan for {} (archive → {}):",
        home.display(),
        archive_root.display()
    );
    for d in &dispositions {
        println!(
            "  {:<58}  {}",
            d.artefact.name,
            match &d.retained_because {
                None => "would ARCHIVE".to_string(),
                Some(r) => format!("retained ({r:?})"),
            }
        );
    }
    if releasable.is_empty() {
        println!("\nNothing is releasable under the policy. Nothing written.");
        return Ok(());
    }
    if args.dry_run || !args.confirm {
        println!(
            "\n{} artefact(s) would be archived. Nothing written.{}",
            releasable.len(),
            if args.dry_run {
                ""
            } else {
                " Pass --confirm to apply."
            }
        );
        return Ok(());
    }

    let binary_hash = crate::binary_hash::compute().context("compute binary hash")?;
    let actor = cli_actor("system:evidence-cli");
    let mut archived = 0usize;
    for d in &releasable {
        let out = aberp_snapshot::archive_then_remove(&d.artefact, &archive_root, &args.tenant)
            .map_err(|e| anyhow::anyhow!("archive {}: {e}", d.artefact.path.display()))?;
        let payload = EvidenceArchivedPayload {
            archived_from: out.from.display().to_string(),
            archived_to: out.to.display().to_string(),
            byte_size: out.byte_size,
            sha256: out.sha256,
            incident_tag: d
                .artefact
                .incident_tag
                .clone()
                .unwrap_or_else(|| "untagged".into()),
            archived_at: rfc3339(OffsetDateTime::now_utc()),
        };
        emit_snapshot_event(
            &SnapshotAudit::Reopen,
            &args.db,
            &tenant,
            binary_hash,
            EventKind::EvidenceArchived,
            payload.to_bytes(),
            actor.clone(),
        )
        .context("append EvidenceArchived")?;
        archived += 1;
        println!("  archived {} → {}", out.from.display(), out.to.display());
    }
    println!("\n{archived} artefact(s) archived. Each was verified by SHA-256 before its original was unlinked.");
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
    // ── ADR-0116 D5 — boot-after-auto-recovery trigger ──────────────────
    //
    // A recovered database is a state worth being able to roll back TO: it is
    // the output of the most invasive automatic operation the system performs,
    // and today nothing preserves it. Fires BEFORE the boot delay because the
    // value is in having the rollback point promptly, and it is subject to the
    // same staleness check as every other trigger so it cannot storm.
    //
    // **`recover_or_refuse` owns the mirror at boot, unconditionally, and this
    // cannot precede it** — see [`BOOT_RECOVERY`]. The ordering is structural:
    // this daemon needs the shared `Handle`, which does not exist until
    // recovery has returned a non-`Refuse` outcome.
    if let Some(reason) = boot_recovery_reason() {
        let db = deps.db_path.clone();
        let store = deps.store_dir.clone();
        let tenant = deps.tenant.clone();
        let bh = deps.binary_hash;
        let policy = deps.policy;
        let interval = deps.interval;
        let handle = deps.db.clone();
        let _ = tokio::task::spawn_blocking(move || {
            trigger_snapshot_if_stale(
                &SnapshotAudit::Handle(&handle),
                &db,
                &store,
                &tenant,
                bh,
                &policy,
                interval,
                reason,
            );
        })
        .await;
    }

    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(Duration::from_secs(BOOT_DELAY_SECS)) => {}
    }

    // ── ADR-0116 D1.2 — CATCH-UP on start ───────────────────────────────
    //
    // Before entering the loop, compare `now` against the newest snapshot in
    // the store; if the store is staler than `interval`, take one immediately
    // instead of waiting a full interval.
    //
    // **This is a FRESHNESS improvement, not an RPO improvement**, and the
    // ADR's first draft had that backwards. Trace it through the real incident
    // gap: catch-up takes a snapshot at `restart + 60 s`, which in the
    // 2026-08-17 → 08-23 gap lands a rollback point on 08-23 — *after* the
    // 08-22 incident. A post-incident rollback point cannot roll back the
    // incident. D1.2 creates ZERO rollback points inside a gap; its whole
    // benefit is bounding staleness to <= `interval` once serve is back. The
    // change that creates rollback points inside a gap is the out-of-process
    // floor (D1.3), which is a host-level `launchd` artefact, not this loop.
    loop {
        if cancel.is_cancelled() {
            return;
        }
        // The staleness check applies to EVERY tick, not only the first: a
        // scheduled out-of-process floor (D1.3) or a D5 trigger may already
        // have satisfied this window, and taking a second snapshot for the
        // same window is pure store growth. Whichever ran first wins; the
        // other no-ops. This is what makes the floor and the daemon safe to
        // run together.
        if !store_is_stale(&deps.store_dir, deps.interval, OffsetDateTime::now_utc()) {
            tracing::debug!(
                store = %deps.store_dir.display(),
                "ADR-0116 D1.2 — the store already holds a snapshot within one interval; \
                 skipping this tick's SNAPSHOT (the floor or a trigger got there first). The \
                 live-file durable checkpoint still runs — see below."
            );
            // ── ADR-0095 §3 STILL RUNS ON A SKIPPED TICK ────────────────
            //
            // The live-file durable checkpoint is folded into this cadence and
            // is durability behaviour **independent of whether a snapshot was
            // taken**: it is what keeps a recent verified-good live file
            // existing between clean shutdowns, closing ADR-0095 root cause #2
            // ("nothing checkpoints the live file on a path a crash
            // traverses").
            //
            // Letting D1.2's skip `continue` past it would have silently
            // un-wired that — and precisely in the configuration ADR-0116 sets
            // up, where a scheduled out-of-process floor satisfies the
            // staleness window on most ticks. The snapshot cadence and the
            // checkpoint cadence share a loop; they must not share a
            // condition.
            let handle = deps.db.clone();
            let _ = tokio::task::spawn_blocking(move || live_checkpoint_logged(&handle)).await;

            let nap = sleep_to_next_grid_boundary(deps.interval, OffsetDateTime::now_utc());
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(nap) => {}
            }
            continue;
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
        // ── ADR-0116 D1.1 — sleep to the next wall-clock GRID boundary ──
        //
        // The loop used to sleep `interval` AFTER the cycle completed, so each
        // tick drifted by however long the cycle took. Measured drift on the
        // clean runs is ~0.27 s per tick (20:01:09.406 → 00:01:09.679 →
        // 04:01:09.948), so this is **cosmetic** — it is here because it is
        // nearly free, and it must not be counted as part of the risk
        // reduction. The RPO fix is the out-of-process floor.
        let nap = sleep_to_next_grid_boundary(deps.interval, OffsetDateTime::now_utc());
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(nap) => {}
        }
    }
}

/// ADR-0116 D1.1 — how long until the next `interval` boundary on the UTC
/// wall-clock grid.
///
/// Pure (the caller passes `now`) so the boundary arithmetic is testable
/// without waiting four hours. Never returns zero: landing exactly on a
/// boundary sleeps a full interval rather than spinning.
pub fn sleep_to_next_grid_boundary(interval: Duration, now: OffsetDateTime) -> Duration {
    let secs = interval.as_secs();
    if secs == 0 {
        return interval;
    }
    let epoch = now.unix_timestamp().max(0) as u64;
    let past = epoch % secs;
    let remaining = secs - past;
    // `past == 0` gives `remaining == secs`, which is what we want.
    Duration::from_secs(remaining)
}

// ──────────────────────────────────────────────────────────────────────
// ADR-0116 D5 — snapshot at the moments that warrant one
// ──────────────────────────────────────────────────────────────────────

/// ADR-0116 D5 — did this boot run a successful auto-recovery?
///
/// `0` = no; anything else is a [`BootRecoveryReason`] code. A plain atomic
/// rather than a channel because the two setters are deep inside `serve`'s
/// boot sequence and the single reader is the snapshot daemon, which cannot
/// exist until recovery has completed (it needs `recovery_state.db`, and the
/// shared `Handle` is not created until after `recover_or_refuse` returns).
///
/// **That is what makes ADR-0116 D5's ordering structural rather than
/// conventional**: `recover_or_refuse_with_audit` owns the mirror at boot
/// unconditionally, and no snapshot trigger can fire before it because the
/// daemon that fires it has no handle to fire with until recovery has
/// returned. A snapshot must never be the thing that first touches a mirror
/// at boot.
static BOOT_RECOVERY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Which boot-recovery path ran, for the D5 trigger's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BootRecoveryReason {
    /// The live DB could not be opened and was rebuilt from a snapshot.
    TornOpen = 1,
    /// The audit mirror was ahead of the DB and was reconciled.
    MirrorAhead = 2,
}

/// Record that this boot auto-recovered the live DB, so the snapshot daemon
/// makes the RECOVERED state a rollback point (ADR-0116 D5/G7).
///
/// Called from `serve`'s boot recovery sites AFTER
/// `recover_or_refuse_with_audit` has returned a non-`Refuse` outcome.
pub fn note_boot_recovery(code: BootRecoveryReason) {
    BOOT_RECOVERY.store(code as u8, std::sync::atomic::Ordering::SeqCst);
}

/// The reason string for the D5 boot trigger, or `None` when this boot did
/// not auto-recover.
pub fn boot_recovery_reason() -> Option<&'static str> {
    match BOOT_RECOVERY.load(std::sync::atomic::Ordering::SeqCst) {
        1 => Some("boot-after-auto-recovery:torn_open"),
        2 => Some("boot-after-auto-recovery:mirror_ahead"),
        _ => None,
    }
}

/// ADR-0116 D5 — take a snapshot at a moment that warrants one, unless the
/// store already holds one within `interval`.
///
/// Every D5 trigger routes through here, so none of them can produce a
/// snapshot storm: the same staleness check D1.2 uses bounds them all.
/// Best-effort by contract — a trigger that fails logs LOUD and returns; a
/// snapshot hiccup must never wedge a shutdown or a boot.
///
/// `reason` names the trigger in the log so a snapshot appearing outside the
/// 4-hourly cadence is attributable.
pub fn trigger_snapshot_if_stale(
    audit: &SnapshotAudit<'_>,
    db_path: &Path,
    store_dir: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    policy: &RetentionPolicy,
    interval: Duration,
    reason: &'static str,
) {
    let now = OffsetDateTime::now_utc();
    if !store_is_stale(store_dir, interval, now) {
        tracing::debug!(
            reason,
            "ADR-0116 D5 — trigger skipped: the store already holds a snapshot within one \
             interval"
        );
        return;
    }
    tracing::info!(
        reason,
        db = %db_path.display(),
        "ADR-0116 D5 — taking a snapshot at a moment that warrants one"
    );
    let actor = cli_actor("system:snapshot-trigger");
    match run_cycle(
        audit,
        db_path,
        store_dir,
        tenant,
        binary_hash,
        actor,
        policy,
    ) {
        Ok(rec) => tracing::info!(
            reason,
            seq = rec.meta.seq,
            valid = rec.meta.valid,
            "ADR-0116 D5 — trigger snapshot taken"
        ),
        Err(e) => tracing::error!(
            reason,
            error = %e,
            "ADR-0116 D5 — trigger snapshot FAILED. This is logged and swallowed: a snapshot \
             must never wedge a shutdown or a boot. The rollback point for this moment does \
             NOT exist."
        ),
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
