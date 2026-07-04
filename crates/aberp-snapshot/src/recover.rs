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

use aberp_audit_ledger::{
    read_mirror_under_tail_policy, replay_mirror_delta, Actor, BinaryHash, EventKind, Ledger,
    MirrorEntry, MirrorTailPolicy, TenantId,
};
use duckdb::Connection;

use crate::crash_safe::{
    atomic_install, checkpoint_is_current, durable_checkpoint, sibling, unique_tag, wal_sibling,
    write_marker, CheckpointReport,
};
use crate::store::{list_snapshots, SnapshotRecord};
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

/// Recovery metadata handed to the pre-install audit hook
/// ([`recover_or_refuse_with_audit`]) so the app layer can build the
/// `db.auto_recovered` payload from the SAME numbers the returned
/// [`RecoveryOutcome::Recovered`] carries — computed BEFORE the swap.
#[derive(Debug, Clone)]
pub struct RecoveredMeta {
    /// Seq of the snapshot the rebuild started from.
    pub source_snapshot_seq: u64,
    /// Audit-ledger head the snapshot carried (the replay floor).
    pub snapshot_audit_count: u64,
    /// Number of mirror entries replayed on top of the snapshot.
    pub replayed_entries: u64,
    /// Rebuilt head seq BEFORE the pre-install audit row is appended.
    pub recovered_max_seq: u64,
    /// The retained `<db>.CORRUPT-<tag>` evidence copy (display form), if the
    /// live file existed before recovery. Never deleted.
    pub retained_corrupt_db: Option<String>,
}

/// A recovery-audit row the app layer asks the recovery engine to append into
/// the STAGING DB (and mirror) BEFORE install. It is carried as opaque audit
/// primitives — [`aberp_audit_ledger`] types plus PRE-SERIALIZED `payload`
/// bytes — so `aberp-snapshot` stays strictly BELOW the app's audit-payload
/// types (crate layering: the app owns the payload shape; the recovery engine
/// owns the durable append seam). The engine opens the private staging DB,
/// appends this row, and tops the mirror up to it (staging head == mirror
/// head) all BEFORE the atomic swap.
pub struct StagedAuditRow {
    /// Binary hash recorded on the appended entry (the app's running binary).
    pub binary_hash: BinaryHash,
    /// The audit event kind (e.g. `EventKind::DbAutoRecovered`).
    pub kind: EventKind,
    /// Pre-serialized payload bytes (the app owns the payload schema).
    pub payload: Vec<u8>,
    /// The actor to attribute the entry to.
    pub actor: Actor,
}

/// Back-compat entry point: guarded auto-recovery with NO pre-install audit
/// hook. Crate-level callers/tests that do not emit an app-layer audit row use
/// this; it delegates to [`recover_or_refuse_with_audit`] with a no-op hook, so
/// it is byte-identical to the historical behaviour (rebuild → validate →
/// atomic-install + marker, with no post-install mutation).
pub fn recover_or_refuse(
    db_path: &Path,
    store_dir: &Path,
    mirror_path: &Path,
    tenant: &str,
) -> Result<RecoveryOutcome> {
    recover_or_refuse_with_audit(db_path, store_dir, mirror_path, tenant, |_| None)
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
/// 6. PRE-INSTALL AUDIT (ADR-0098 R3 fix): ask `build_row` for the caller's
///    `db.auto_recovered` row, then append it into the STAGING DB and top the
///    mirror up to it (staging head == mirror head) — BEFORE the swap, never
///    after. Best-effort: `build_row` returning `None` or a failing append is
///    logged and recovery still installs the validated rebuild (row omitted),
///    preserving the "audit append never fails a durable recovery" posture.
/// 7. COMMIT atomically: fold any WAL the pre-install append left on the
///    staging file, [`atomic_install`] it over the live path, then
///    [`write_marker`] — so the verified-good marker covers the FINAL on-disk
///    bytes and NO post-install mutation can stale it.
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
pub fn recover_or_refuse_with_audit<F>(
    db_path: &Path,
    store_dir: &Path,
    mirror_path: &Path,
    tenant: &str,
    build_row: F,
) -> Result<RecoveryOutcome>
where
    F: FnOnce(&RecoveredMeta) -> Option<StagedAuditRow>,
{
    // SAFETY: an editions build must never act on the FROZEN prod line.
    ensure_not_prod_path(db_path)?;
    ensure_not_prod_path(store_dir)?;

    // 1. PRESERVE evidence — never destroy it. The original live file stays
    //    in place until the rebuild is swapped over it.
    let retained_corrupt_db = if db_path.exists() {
        Some(preserve_corrupt_db(db_path)?)
    } else {
        None
    };

    // 2. The recovery candidates are the VALID snapshots (ADR-0082),
    //    newest-seq first (the order `list_snapshots` returns). None → refuse
    //    (cannot rebuild from nothing — the existing preserve-and-surface).
    let valid_snapshots: Vec<SnapshotRecord> = list_snapshots(store_dir)?
        .into_iter()
        .filter(|r| r.meta.valid)
        .collect();
    let latest = match valid_snapshots.first() {
        Some(s) => s,
        None => {
            return Ok(RecoveryOutcome::RefusedNoSnapshot {
                retained_corrupt_db,
            })
        }
    };
    let snapshot_audit_count = latest.meta.audit_count.max(0) as u64;

    // The mirror is the second source of recovery truth. Under the unified
    // ADR-0098 R1 torn-tail policy it is READ here: a lone torn trailing line
    // is PRESERVED + trimmed (the append never durably happened) and recovery
    // proceeds on the intact prefix; corruption DEEPER than a torn tail, or a
    // missing/unreadable mirror, is unsafe → refuse (safe fallback). Committed
    // entries are NEVER truncated — the only content-bearing mutation is the
    // append-only TOP-UP on the self-certified ahead-snapshot path (D3).
    let mirror_entries = match read_mirror_under_tail_policy(mirror_path) {
        Ok(MirrorTailPolicy::Clean(e)) => e,
        Ok(MirrorTailPolicy::TornTail {
            entries,
            preserved,
            dropped_bytes,
        }) => {
            tracing::warn!(
                target: "audit_event",
                event = "audit_mirror_torn_tail_trimmed",
                mirror_path = %mirror_path.display(),
                preserved = %preserved.display(),
                dropped_bytes,
                trimmed_head_seq = entries.last().map(|e| e.seq()).unwrap_or(0),
                "ADR-0098 R1 — recovery mirror had a torn trailing line; preserved the original \
                 and trimmed to the intact prefix, proceeding on the prefix"
            );
            entries
        }
        Ok(MirrorTailPolicy::DeepCorrupt { preserved, reason }) => {
            return Ok(RecoveryOutcome::RefusedUnsafe {
                reason: format!(
                    "audit-ledger mirror at {} is corrupt beyond a torn tail ({reason}); the \
                     original was preserved to {} — refusing to auto-recover (investigate; do \
                     NOT hand-edit the mirror)",
                    mirror_path.display(),
                    preserved.display()
                ),
                retained_corrupt_db,
            })
        }
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

    // 3. ADR-0098 Gap 2a — guard COHERENCE. The latest valid snapshot is
    //    either a PREFIX of the mirror (`audit_count <= mirror_head`, the
    //    original IMPORT-snapshot + replay-mirror-delta path) or AHEAD of it
    //    (`> mirror_head`). An ahead snapshot is recovered from IFF it
    //    SELF-CERTIFIES (D4: its own hash chain verifies genesis→head AND the
    //    mirror agrees with it over the overlap `[1..mirror_head]`); the
    //    lagging mirror is then TOPPED UP to the snapshot head (append +
    //    fsync, never truncate — D3). If the ahead snapshot cannot
    //    self-certify we fall back to the newest valid snapshot
    //    `<= mirror_head`; only if THAT is also impossible do we refuse.
    //
    //    The mirror-AHEAD-of-DB P0 preserve-and-refuse is a *different*
    //    condition, handled in `audit-ledger`'s `ensure_consistent_with_db`;
    //    it is untouched by this branch.
    let primary = stage_and_validate(
        db_path,
        latest,
        mirror_path,
        tenant,
        snapshot_audit_count,
        mirror_max_seq,
        mirror_head_hash.as_deref(),
        &mirror_entries,
    )?;
    let self_certifies = matches!(primary.verdict, Verdict::Recovered(_));

    // The ahead-but-not-self-certifying fallback is the newest VALID snapshot
    // whose head does not exceed the mirror head.
    let ahead = snapshot_audit_count > mirror_max_seq;
    let fallback = if ahead && !self_certifies {
        valid_snapshots
            .iter()
            .find(|r| (r.meta.audit_count.max(0) as u64) <= mirror_max_seq)
    } else {
        None
    };

    match route_guard(
        snapshot_audit_count,
        mirror_max_seq,
        self_certifies,
        fallback.is_some(),
    ) {
        // PREFIX: the primary attempt IS the existing prefix path.
        // RECOVER-AHEAD-TOPUP: the primary self-certified and topped up the
        // mirror. Both install (or surface a refusal from) the primary.
        GuardRoute::Prefix | GuardRoute::RecoverAheadTopUp => install_or_refuse(
            primary,
            latest,
            snapshot_audit_count,
            retained_corrupt_db,
            db_path,
            mirror_path,
            tenant,
            build_row,
        ),
        // FALL BACK: the ahead snapshot did not self-certify; rebuild from the
        // newest valid snapshot `<= mirror_head` via the prefix path.
        GuardRoute::FallBackToPrefix => {
            cleanup_temp(&primary.staging);
            let fb = fallback.expect("route_guard returned FallBackToPrefix => fallback present");
            let fb_count = fb.meta.audit_count.max(0) as u64;
            tracing::warn!(
                latest_snapshot_seq = latest.meta.seq,
                latest_audit_count = snapshot_audit_count,
                mirror_head = mirror_max_seq,
                fallback_snapshot_seq = fb.meta.seq,
                fallback_audit_count = fb_count,
                "ADR-0098 Gap 2a — ahead snapshot did not self-certify; falling back to the \
                 newest valid snapshot at or below the mirror head"
            );
            let fb_attempt = stage_and_validate(
                db_path,
                fb,
                mirror_path,
                tenant,
                fb_count,
                mirror_max_seq,
                mirror_head_hash.as_deref(),
                &mirror_entries,
            )?;
            install_or_refuse(
                fb_attempt,
                fb,
                fb_count,
                retained_corrupt_db,
                db_path,
                mirror_path,
                tenant,
                build_row,
            )
        }
        // REFUSE: ahead, cannot self-certify, and no valid snapshot
        // `<= mirror_head` to fall back to. Preserve-and-surface (never guess)
        // — the chunk-3 P1 default, demoted to a last resort.
        GuardRoute::Refuse => {
            let reason = match primary.verdict {
                Verdict::Refuse(r) => format!(
                    "latest valid snapshot (audit_count={snapshot_audit_count}) is AHEAD of the \
                     mirror head (seq={mirror_max_seq}) and could not self-certify ({r}); no valid \
                     snapshot at or below the mirror head to fall back to — refusing"
                ),
                Verdict::Recovered(_) => {
                    unreachable!("route_guard returns Refuse only when the primary refused")
                }
            };
            cleanup_temp(&primary.staging);
            Ok(RecoveryOutcome::RefusedUnsafe {
                reason,
                retained_corrupt_db,
            })
        }
    }
}

/// Infix of the private recovery-staging temp (`<db>.recover-<tag>.duckdb`).
const RECOVER_INFIX: &str = ".recover-";
/// Infix of the atomic-creation temp (`<db>.creating-<tag>.duckdb`).
const CREATING_INFIX: &str = ".creating-";
/// Infix of the retained torn-DB evidence (`<db>.CORRUPT-<tag>`).
const CORRUPT_INFIX: &str = ".CORRUPT-";

struct RebuildInfo {
    /// Mirror entries replayed on top of the snapshot (prefix path; 0 ahead).
    replayed: u64,
    /// Snapshot entries appended to TOP UP a lagging mirror to the snapshot
    /// head (ahead path, ADR-0098 D3; 0 on the prefix path). Append-only.
    topped_up: u64,
    /// Rebuilt + installed head seq (`max(snapshot_head, mirror_head)`).
    max_seq: u64,
}

/// A guard-rail decision: a validated rebuild, or a refusal reason (the safe
/// fallback). Distinct from a hard mechanical `Err`.
enum Verdict {
    Recovered(RebuildInfo),
    Refuse(String),
}

/// One staged recovery attempt: the private staging DB path and the
/// guard-rail [`Verdict`] of building + validating it. The staging file is
/// the caller's to install (on `Recovered`) or clean up (otherwise).
struct Attempt {
    staging: PathBuf,
    verdict: Verdict,
}

/// The guard's three-way recovery decision (ADR-0098 Gap 2a) as a PURE
/// function of the snapshot / mirror heads, whether an ahead snapshot
/// self-certifies, and whether a `<= mirror_head` fallback snapshot exists.
/// [`recover_or_refuse`] routes on it; the table is unit-tested
/// (`tests::route_guard_*`) and faithfully extracted for the saw-off gate
/// (the crate cannot `cargo build` there — bundled DuckDB amalgamation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardRoute {
    /// `snapshot_head <= mirror_head`: the snapshot is a prefix of the mirror
    /// — the original IMPORT-snapshot + replay-mirror-delta path.
    Prefix,
    /// `snapshot_head > mirror_head` AND it self-certifies AND `mirror_head >=
    /// 1` (a real genesis anchor exists — ADR-0098 R1, finding G): recover from
    /// the ahead snapshot and top the lagging mirror up to it (append + fsync).
    RecoverAheadTopUp,
    /// `snapshot_head > mirror_head`, does NOT self-certify, but a valid
    /// snapshot `<= mirror_head` exists: fall back to it via the prefix path.
    FallBackToPrefix,
    /// `snapshot_head > mirror_head`, cannot self-certify, and no fallback
    /// snapshot `<= mirror_head`: refuse (preserve-and-surface; never guess).
    Refuse,
}

/// Pure routing for the Gap 2a guard (see [`GuardRoute`]). No I/O, so the
/// full decision table is verifiable without DuckDB.
fn route_guard(
    snapshot_head: u64,
    mirror_head: u64,
    self_certifies: bool,
    fallback_available: bool,
) -> GuardRoute {
    if snapshot_head <= mirror_head {
        GuardRoute::Prefix
    } else if self_certifies && mirror_head >= 1 {
        // ADR-0098 R1 (finding G): an AHEAD snapshot may only be recovered from
        // when there is a real genesis anchor to certify against. An EMPTY
        // mirror (`mirror_head == 0`) makes the overlap `[1..=0]` vacuously
        // satisfied, so ANY internally-valid snapshot would "self-certify" —
        // refused here so a MISSING and an EMPTY mirror behave coherently (both
        // refuse). Belt-and-suspenders with the same guard in
        // `build_and_validate`, which prevents `self_certifies` for an empty
        // mirror in the first place.
        GuardRoute::RecoverAheadTopUp
    } else if fallback_available {
        GuardRoute::FallBackToPrefix
    } else {
        GuardRoute::Refuse
    }
}

/// PURE: the first overlap seq at which the mirror and the rebuilt
/// (snapshot-sourced) chain disagree on `entry_hash`, or `None` if every
/// overlap entry agrees. `mirror` and `staging` are `(seq, entry_hash)` in
/// seq order; the overlap is the mirror's range `[1..=mirror_head]`. A
/// missing staging entry at a mirror seq counts as a disagreement. This is
/// the overlap-agreement half of the D4 self-certification bar, kept I/O-free
/// so it is verifiable without DuckDB (the saw-off gate).
fn first_overlap_disagreement(mirror: &[(u64, String)], staging: &[(u64, String)]) -> Option<u64> {
    for (seq, mirror_hash) in mirror {
        match staging.iter().find(|(s, _)| s == seq) {
            Some((_, staging_hash)) if staging_hash == mirror_hash => {}
            _ => return Some(*seq),
        }
    }
    None
}

/// PURE (ADR-0098 R1, finding G): does the mirror overlap begin at genesis
/// (seq 1)? The overlap is `(seq, entry_hash)` in seq order. An EMPTY overlap
/// is NOT anchored — that is the empty-mirror vacuity an ahead-snapshot
/// self-certification must reject (a MISSING mirror already refuses; an EMPTY
/// one must too). I/O-free, so it is verifiable without DuckDB (the saw-off
/// gate).
fn overlap_is_genesis_anchored(mirror_overlap: &[(u64, String)]) -> bool {
    matches!(mirror_overlap.first(), Some((seq, _)) if *seq == 1)
}

/// Create a fresh private staging path and build + validate `snapshot` into
/// it (prefix mode when `snapshot_audit_count <= mirror_max_seq`, ahead mode
/// — which also tops up the mirror — otherwise). The live path is untouched.
#[allow(clippy::too_many_arguments)]
fn stage_and_validate(
    db_path: &Path,
    snapshot: &SnapshotRecord,
    mirror_path: &Path,
    tenant: &str,
    snapshot_audit_count: u64,
    mirror_max_seq: u64,
    mirror_head_hash: Option<&str>,
    mirror_entries: &[MirrorEntry],
) -> Result<Attempt> {
    // Clear any orphan staging from a crashed prior recovery first (the
    // `.CORRUPT-*` evidence has a different infix and is kept).
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
        mirror_head_hash,
        mirror_entries,
    )?;
    Ok(Attempt { staging, verdict })
}

/// COMMIT a validated rebuild atomically (swap over the live path + write the
/// verified-good marker), or surface the guard-rail refusal. On refusal the
/// disposable staging is cleaned; the live inputs are untouched.
#[allow(clippy::too_many_arguments)]
fn install_or_refuse<F>(
    attempt: Attempt,
    snapshot: &SnapshotRecord,
    snapshot_audit_count: u64,
    retained_corrupt_db: Option<PathBuf>,
    db_path: &Path,
    mirror_path: &Path,
    tenant: &str,
    build_row: F,
) -> Result<RecoveryOutcome>
where
    F: FnOnce(&RecoveredMeta) -> Option<StagedAuditRow>,
{
    match attempt.verdict {
        Verdict::Recovered(info) => {
            // ADR-0098 R3 regression fix — append the caller's
            // `db.auto_recovered` audit row into the STAGING DB and top up the
            // mirror BEFORE the swap, so the installed bytes already carry the
            // row and the verified-good marker below covers the FINAL DB. This
            // replaces the old post-install append that left an unfolded WAL
            // (Ledger::open carries `disable_checkpoint_on_shutdown`) which the
            // debounced live checkpoint then skipped as a no-op, staling the
            // marker on the next plain open's WAL fold. Best-effort: a failed
            // append is logged, never fatal (the recovery is still durable).
            // The append (Ledger::open on the PRIVATE staging file) lives HERE
            // in the recovery engine — not the app layer — so no new live-path
            // opener appears in the app (ADR-0093 D5 residual-opener freeze) and
            // the app keeps ownership only of the payload bytes (layering).
            let meta = RecoveredMeta {
                source_snapshot_seq: snapshot.meta.seq,
                snapshot_audit_count,
                replayed_entries: info.replayed,
                recovered_max_seq: info.max_seq,
                retained_corrupt_db: retained_corrupt_db
                    .as_ref()
                    .map(|p| p.display().to_string()),
            };
            if let Some(row) = build_row(&meta) {
                if let Err(e) = append_staged_audit_row(&attempt.staging, mirror_path, tenant, row)
                {
                    tracing::warn!(
                        error = %e,
                        "pre-install db.auto_recovered audit append failed; installing the \
                         validated rebuild without the audit row (recovery still durable)"
                    );
                }
            }
            // Fold any WAL the pre-install append left beside the staging file
            // into its main file BEFORE `atomic_install` (which deletes the
            // staging WAL) — otherwise the appended row, living only in the WAL,
            // would be discarded by the swap. Idempotent no-op if the append
            // left nothing pending. Mirrors the fold `build_and_validate` and
            // `durable_checkpoint` already perform on their staging files.
            fold_staging_wal(&attempt.staging)?;
            atomic_install(&attempt.staging, db_path)?;
            write_marker(db_path)?;
            if info.topped_up > 0 {
                tracing::info!(
                    source_snapshot_seq = snapshot.meta.seq,
                    topped_up = info.topped_up,
                    recovered_max_seq = info.max_seq,
                    "ADR-0098 D3 — topped up the lagging mirror to the self-certified \
                     ahead-snapshot head (append-only; never truncated)"
                );
            }
            Ok(RecoveryOutcome::Recovered {
                source_snapshot_seq: snapshot.meta.seq,
                snapshot_audit_count,
                replayed_entries: info.replayed,
                recovered_max_seq: info.max_seq,
                retained_corrupt_db,
            })
        }
        Verdict::Refuse(reason) => {
            cleanup_temp(&attempt.staging);
            Ok(RecoveryOutcome::RefusedUnsafe {
                reason,
                retained_corrupt_db,
            })
        }
    }
}

/// Append a caller-provided [`StagedAuditRow`] into the PRIVATE staging DB and
/// top the mirror up to the new head (append to BOTH → staging head seq ==
/// mirror head seq; the mirror is EXTENDED, never truncated — no chain fork).
/// Best-effort: any failure is returned as a display string for the caller to
/// log; recovery still installs the validated rebuild without the row. This is
/// the ONE recovery-time opener of the staging file for the audit append — it
/// lives in `aberp-snapshot` (the sanctioned recovery layer) rather than the
/// app so no new live-path opener surfaces in the app tree (ADR-0093 D5).
fn append_staged_audit_row(
    staging: &Path,
    mirror_path: &Path,
    tenant: &str,
    row: StagedAuditRow,
) -> std::result::Result<(), String> {
    let tenant_id =
        TenantId::new(tenant.to_string()).ok_or_else(|| format!("invalid tenant id {tenant:?}"))?;
    // Ledger::open carries `disable_checkpoint_on_shutdown` (ADR-0098 R3), so
    // this append leaves the row in the staging WAL; `fold_staging_wal` folds
    // it into the staging main file before the swap.
    let mut ledger = Ledger::open(staging, tenant_id, row.binary_hash)
        .map_err(|e| format!("open staging ledger to record the recovery audit row: {e}"))?;
    ledger
        .append(row.kind, row.payload, row.actor, None)
        .map_err(|e| format!("append the recovery audit row into the staging DB: {e}"))?;
    ledger
        .sync_mirror(mirror_path)
        .map_err(|e| format!("top the mirror up to the staging head: {e}"))?;
    Ok(())
}

/// Fold any WAL left beside a freshly-built staging file into its main file so
/// the file is self-contained BEFORE [`atomic_install`] renames it over the
/// live path (and deletes the staging WAL). Opening a fresh connection and
/// issuing an explicit `CHECKPOINT` is the same self-contained-file fold
/// `build_and_validate` and `durable_checkpoint` perform; a checkpoint failure
/// here aborts the install (better than swapping in bytes the marker would not
/// cover). A no-op when no WAL is pending.
fn fold_staging_wal(staging: &Path) -> Result<()> {
    // Only re-open when there is actually a WAL to fold: the back-compat no-hook
    // path (and any hook that appended nothing) leaves the staging file
    // self-contained from `build_and_validate`, so this is a pure no-op there.
    if !wal_sibling(staging).exists() {
        return Ok(());
    }
    let conn = Connection::open(staging)?;
    conn.execute_batch("CHECKPOINT;")?;
    Ok(())
}

/// Build the staging DB from the snapshot export + mirror replay, then
/// validate it. Returns a [`Verdict`] (recovered or a refusal reason) for
/// guard-rail outcomes; a hard `Err` only for an unexpected I/O failure. The
/// staging connection is opened ONCE and reused for verification (S375 — a
/// re-open triggers the very `LoadCheckpoint`/ART replay path we recover from).
#[allow(clippy::too_many_arguments)]
fn build_and_validate(
    staging: &Path,
    snapshot_dir: &Path,
    mirror_path: &Path,
    tenant: &str,
    snapshot_audit_count: u64,
    mirror_max_seq: u64,
    mirror_head_hash: Option<&str>,
    mirror_entries: &[MirrorEntry],
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
    //    For an AHEAD snapshot (snapshot head > mirror head) the mirror has
    //    no entries beyond the snapshot, so this is a no-op (0 replayed) — the
    //    rebuild is wholly the snapshot and the mirror is TOPPED UP below.
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

    // 5. VALIDATE. The rebuild's head-of-truth is `max(snapshot_head,
    //    mirror_head)`: the mirror head on the prefix path, the snapshot head
    //    on the ahead path.
    let tenant_id = match TenantId::new(tenant.to_string()) {
        Some(t) => t,
        None => return Ok(Verdict::Refuse(format!("invalid tenant id {tenant:?}"))),
    };
    let validated_head = snapshot_audit_count.max(mirror_max_seq);
    // Reuse the already-open handle for verification (no re-open — S375).
    let ledger = Ledger::from_connection(conn, tenant_id, BinaryHash::from_bytes([0u8; 32]));
    // 5a. The hash chain verifies genesis→head (ADR-0008). For an ahead
    //     snapshot this is the FIRST self-certification gate (D4).
    let chain_len = match ledger.verify_chain() {
        Ok(n) => n,
        Err(e) => {
            return Ok(Verdict::Refuse(format!(
                "rebuilt DB hash-chain verification failed: {e}"
            )))
        }
    };
    // 5b. The rebuilt head seq reconciles with `max(snapshot, mirror)`.
    if chain_len != validated_head {
        return Ok(Verdict::Refuse(format!(
            "rebuilt DB head seq {chain_len} does not reconcile with the expected head \
             {validated_head} (snapshot head {snapshot_audit_count}, mirror head {mirror_max_seq})"
        )));
    }

    if snapshot_audit_count > mirror_max_seq {
        // ── AHEAD snapshot ───────────────────────────────────────────────
        // 5c-0. ADR-0098 R1 (finding G): REQUIRE a genesis anchor. An EMPTY
        //     mirror (`mirror_head == 0`) has no overlap, so `[1..=0]` is
        //     vacuously satisfied — that would let ANY internally-valid
        //     snapshot self-certify and install (a MISSING mirror refuses, but
        //     an EMPTY one would recover — an incoherent asymmetry). Refuse
        //     unless `mirror_head >= 1` AND the overlap is anchored at genesis
        //     (seq 1 present); this makes `self_certifies` unreachable for an
        //     empty mirror, so the belt-and-suspenders guard in `route_guard`
        //     never even fires.
        if mirror_max_seq == 0 {
            return Ok(Verdict::Refuse(
                "ahead snapshot cannot self-certify against an EMPTY mirror (mirror_head=0; no \
                 genesis anchor) — refusing to install a vacuously-certified snapshot"
                    .to_string(),
            ));
        }
        // 5c. SELF-CERTIFY gate 2 (D4): the mirror must agree with the
        //     snapshot over their overlap `[1..=mirror_head]`. Compare the
        //     mirror's recorded `entry_hash` at every overlap seq against the
        //     rebuilt (snapshot-sourced) chain's `entry_hash`. Disagreement →
        //     do NOT guess: refuse (the caller falls back to a `<= mirror_head`
        //     snapshot, else `RefusedUnsafe`).
        let staging_entries = match ledger.entries() {
            Ok(es) => es,
            Err(e) => {
                return Ok(Verdict::Refuse(format!(
                    "reading rebuilt entries for the overlap check failed: {e}"
                )))
            }
        };
        let staging_overlap: Vec<(u64, String)> = staging_entries
            .iter()
            .filter(|e| e.seq.as_u64() <= mirror_max_seq)
            .map(|e| (e.seq.as_u64(), hex::encode(e.entry_hash.as_bytes())))
            .collect();
        let mirror_overlap: Vec<(u64, String)> = mirror_entries
            .iter()
            .filter(|m| m.seq() <= mirror_max_seq)
            .map(|m| (m.seq(), m.entry_hash().to_string()))
            .collect();
        // ADR-0098 R1 (finding G): the overlap must be ANCHORED AT GENESIS
        // (seq 1). With `mirror_head >= 1` (guaranteed above) the mirror's
        // seq-1 entry is present; require it explicitly so a non-genesis
        // overlap can never vacuously certify.
        if !overlap_is_genesis_anchored(&mirror_overlap) {
            return Ok(Verdict::Refuse(
                "ahead snapshot cannot self-certify: the mirror overlap is not anchored at \
                 genesis (missing seq 1) — refusing to recover from it"
                    .to_string(),
            ));
        }
        if let Some(seq) = first_overlap_disagreement(&mirror_overlap, &staging_overlap) {
            return Ok(Verdict::Refuse(format!(
                "ahead snapshot does not self-certify: the mirror and the snapshot disagree at \
                 overlap seq {seq} (entry_hash mismatch) — refusing to recover from it"
            )));
        }

        // 5d. TOP UP the lagging mirror to the snapshot head (ADR-0098 D3):
        //     append the snapshot entries `(mirror_head .. snapshot_head]`
        //     verbatim and fsync — the same append `sync_mirror` performs — so
        //     the system reconciles to `Unchanged` with no chain fork. The
        //     mirror is EXTENDED, never truncated. `sync_mirror` re-checks the
        //     boundary overlap before appending (defence in depth).
        let topped_head = match ledger.sync_mirror(mirror_path) {
            Ok(h) => h,
            Err(e) => {
                return Ok(Verdict::Refuse(format!(
                    "topping up the mirror to the ahead-snapshot head failed ({e}) — refusing"
                )))
            }
        };
        if topped_head != chain_len {
            return Ok(Verdict::Refuse(format!(
                "mirror top-up reached head {topped_head}, expected {chain_len} — refusing"
            )));
        }

        drop(ledger);
        Ok(Verdict::Recovered(RebuildInfo {
            replayed,
            topped_up: chain_len - mirror_max_seq,
            max_seq: chain_len,
        }))
    } else {
        // ── PREFIX snapshot (validation unchanged) ───────────────────────
        // 5c. The rebuilt head `entry_hash` matches the mirror head.
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

        drop(ledger);
        Ok(Verdict::Recovered(RebuildInfo {
            replayed,
            topped_up: 0,
            max_seq: chain_len,
        }))
    }
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

    // ── ADR-0098 Gap 2a — pure guard-decision logic (no DuckDB) ──────────
    // The same logic is faithfully extracted and run under `rustc --test` on
    // the saw-off sandbox (where the crate cannot `cargo build`); here it
    // also rides the Mac `cargo test` gate.

    #[test]
    fn route_guard_prefix_when_snapshot_at_or_below_mirror_head() {
        // A snapshot that is a prefix of the mirror takes the existing path,
        // independent of self-cert / fallback (both irrelevant when behind).
        assert_eq!(route_guard(100, 106, true, true), GuardRoute::Prefix);
        assert_eq!(route_guard(100, 106, false, false), GuardRoute::Prefix);
        assert_eq!(route_guard(106, 106, false, false), GuardRoute::Prefix);
        assert_eq!(route_guard(0, 0, false, false), GuardRoute::Prefix);
    }

    #[test]
    fn route_guard_recovers_from_ahead_snapshot_that_self_certifies() {
        // The named case: snapshot head 109 > mirror head 106 and it
        // self-certifies → recover from it + top up the mirror (NOT refuse),
        // whether or not a fallback also exists.
        assert_eq!(
            route_guard(109, 106, true, false),
            GuardRoute::RecoverAheadTopUp
        );
        assert_eq!(
            route_guard(109, 106, true, true),
            GuardRoute::RecoverAheadTopUp
        );
    }

    #[test]
    fn route_guard_falls_back_when_ahead_uncertifiable_with_fallback() {
        // Ahead, cannot self-certify, but a `<= mirror_head` snapshot exists
        // → fall back to the prefix path on it (never guess from the ahead).
        assert_eq!(
            route_guard(109, 106, false, true),
            GuardRoute::FallBackToPrefix
        );
    }

    #[test]
    fn route_guard_refuses_only_when_ahead_uncertifiable_and_no_fallback() {
        // Ahead, cannot self-certify, and nothing `<= mirror_head` to fall
        // back to → the safe last resort: refuse.
        assert_eq!(route_guard(109, 106, false, false), GuardRoute::Refuse);
    }

    #[test]
    fn route_guard_refuses_ahead_against_empty_mirror() {
        // ADR-0098 R1 (finding G): an EMPTY mirror (mirror_head == 0) must NOT
        // let an ahead snapshot self-certify, even if the (vacuous) self-cert
        // flag were somehow set. With no fallback → Refuse; with a `<= 0`
        // fallback available → FallBackToPrefix. NEVER RecoverAheadTopUp — so a
        // MISSING mirror (refused upstream) and an EMPTY one behave coherently.
        assert_eq!(route_guard(109, 0, true, false), GuardRoute::Refuse);
        assert_eq!(
            route_guard(109, 0, true, true),
            GuardRoute::FallBackToPrefix
        );
        assert_ne!(
            route_guard(109, 0, true, false),
            GuardRoute::RecoverAheadTopUp
        );
    }

    #[test]
    fn overlap_agreement_detects_first_disagreeing_seq() {
        let mirror = vec![
            (1u64, "a".to_string()),
            (2, "b".to_string()),
            (3, "c".to_string()),
        ];
        // The snapshot-sourced chain may extend past the overlap; agreement
        // over [1..=mirror_head] is all that is required.
        let agree = vec![
            (1u64, "a".to_string()),
            (2, "b".to_string()),
            (3, "c".to_string()),
            (4, "d".to_string()),
        ];
        assert_eq!(first_overlap_disagreement(&mirror, &agree), None);
        // A differing entry_hash inside the overlap is reported at its seq.
        let disagree = vec![
            (1u64, "a".to_string()),
            (2, "X".to_string()),
            (3, "c".to_string()),
        ];
        assert_eq!(first_overlap_disagreement(&mirror, &disagree), Some(2));
        // A missing overlap entry counts as a disagreement.
        let missing = vec![(1u64, "a".to_string())];
        assert_eq!(first_overlap_disagreement(&mirror, &missing), Some(2));
    }

    #[test]
    fn overlap_genesis_anchor_requires_seq_1() {
        // ADR-0098 R1 (finding G): an EMPTY overlap is NOT genesis-anchored; an
        // overlap that starts at seq 1 IS; one that starts past genesis is NOT.
        assert!(!overlap_is_genesis_anchored(&[]));
        assert!(overlap_is_genesis_anchored(&[
            (1u64, "a".to_string()),
            (2, "b".to_string())
        ]));
        assert!(!overlap_is_genesis_anchored(&[(2u64, "b".to_string())]));
    }
}
