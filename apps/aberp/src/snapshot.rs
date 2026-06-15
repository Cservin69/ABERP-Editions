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

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_snapshot::{
    default_store_dir, ensure_restore_allowed, find_snapshot, list_snapshots, plan_retention,
    prune, restore_into, take_snapshot, RetentionPolicy, SnapshotRecord,
};

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

// ──────────────────────────────────────────────────────────────────────
// Configuration resolution
// ──────────────────────────────────────────────────────────────────────

/// Resolve the snapshot store directory: an explicit `--store` wins,
/// otherwise the per-tenant default `~/Documents/ABERP-snapshots/<tenant>`.
pub fn resolve_store(tenant: &str, explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(p) => Ok(p.to_path_buf()),
        None => default_store_dir(tenant).context("resolve default snapshot store dir"),
    }
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

/// Open a [`Ledger`] against the live DB to append a snapshot audit event.
fn open_ledger(db_path: &Path, tenant: &TenantId, binary_hash: BinaryHash) -> Result<Ledger> {
    Ledger::open(db_path, tenant.clone(), binary_hash)
        .map_err(|e| anyhow::anyhow!("open audit ledger for snapshot event: {e}"))
}

// ──────────────────────────────────────────────────────────────────────
// Shared operations (CLI + daemon + HTTP all call these)
// ──────────────────────────────────────────────────────────────────────

/// Take one validated snapshot and emit the appropriate audit event
/// (`SnapshotCreated` on success, `SnapshotValidationFailed` if the
/// snapshot was produced but failed its built-in validation — in which case
/// the invalid snapshot is kept on disk and the last-good is preserved by
/// retention). Returns the finalized record either way.
pub fn take_and_emit(
    db_path: &Path,
    store_dir: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    actor: Actor,
) -> Result<SnapshotRecord> {
    let now = OffsetDateTime::now_utc();
    let rec = take_snapshot(db_path, store_dir, tenant.as_str(), now)
        .with_context(|| format!("take snapshot of {}", db_path.display()))?;

    let created_at = rfc3339(rec.meta.created_at);
    let mut ledger = open_ledger(db_path, tenant, binary_hash)?;
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
        ledger
            .append(EventKind::SnapshotCreated, payload.to_bytes(), actor, None)
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
        ledger
            .append(
                EventKind::SnapshotValidationFailed,
                payload.to_bytes(),
                actor,
                None,
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
        let mut ledger = open_ledger(db_path, tenant, binary_hash)?;
        ledger
            .append(EventKind::SnapshotPruned, payload.to_bytes(), actor, None)
            .map_err(|e| anyhow::anyhow!("append SnapshotPruned: {e}"))?;
        tracing::info!(pruned = ?removed, retained = plan.keep.len(), "snapshot retention applied");
    }
    Ok(removed)
}

/// One full daemon cycle: take + validate + emit, then retention + emit.
/// Retention failure does not discard the snapshot just taken.
pub fn run_cycle(
    db_path: &Path,
    store_dir: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    actor: Actor,
    policy: &RetentionPolicy,
) -> Result<SnapshotRecord> {
    let rec = take_and_emit(db_path, store_dir, tenant, binary_hash.clone(), actor.clone())?;
    if let Err(e) = retention_and_emit(db_path, store_dir, tenant, binary_hash, actor, policy) {
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
    db_path_for_audit: &Path,
    store_dir: &Path,
    selector: &str,
    target: &Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    actor: Actor,
) -> Result<SnapshotRecord> {
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
    let mut ledger = open_ledger(db_path_for_audit, tenant, binary_hash)?;
    ledger
        .append(EventKind::SnapshotRestored, payload.to_bytes(), actor, None)
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

    let rec = run_cycle(&args.db, &store_dir, &tenant, binary_hash, actor, &policy)?;
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
        let bh = deps.binary_hash.clone();
        let policy = deps.policy;
        let actor = cli_actor("system:snapshot-daemon");
        let outcome =
            tokio::task::spawn_blocking(move || run_cycle(&db, &store, &tenant, bh, actor, &policy))
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
}
