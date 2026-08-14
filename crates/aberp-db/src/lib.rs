//! `aberp-db` — ADR-0098 Gap 1a/1b: **one process-wide DuckDB access path.**
//!
//! # Why this crate exists (the 2026-06-29 17:02 re-tear)
//!
//! The Defense edition recovered cleanly at 16:58 and **re-tore at runtime
//! ~4 minutes later** under live daemon load. Root cause (ADR-0098 Gap 1a):
//! the `serve` process hosts many subsystems (pricing / quote-intake /
//! catalogue-push / email-relay / email-outbox / pdf-rerender daemons + every
//! request handler) that each call their **own** `duckdb::Connection::open`
//! on the **same** single-file tenant DB, in read-write, concurrently. DuckDB
//! single-file storage is **single-writer**; N separate `Connection::open`
//! calls are N independent `Database` instances = N checkpoint actors racing
//! one file = the `duckdb#23046` torn-metadata path, reproducibly within
//! minutes (email-relay ticks every 2 s).
//!
//! [`Handle`] is the seam the codebase *assumed* it had (`take.rs:172`'s
//! "DuckDB shares one instance per process" comment — the assumption the
//! re-tear disproved) but never built: **exactly one** `Database`, **one**
//! checkpoint actor, all runtime DB access routed through it.
//!
//! # What it guarantees
//!
//! * **1a — single instance.** The live tenant DB is opened **once** at boot.
//!   [`Handle::write`] hands out the one shared connection behind a mutex
//!   (writes are serialized — one checkpoint actor, never an interleave);
//!   [`Handle::read`] hands out a [`duckdb::Connection::try_clone`] of the
//!   **same** instance (shared buffer cache, no second OS open). Nothing else
//!   opens the live path at runtime.
//! * **1b — durable, lockstep post-commit.** After every committed write the
//!   [`WriteGuard`] runs a **lockstep** [`aberp_audit_ledger::sync_mirror`]
//!   (the mirror tracks the DB continuously — this also closes ADR-0098
//!   Gap 2b at the source) and a **debounced** validated
//!   [`aberp_snapshot::live_durable_checkpoint`] (≤ 1/min + on idle, D2). The
//!   handle **disables DuckDB's implicit checkpoint-on-close** so a runtime
//!   connection drop never folds the WAL in place (the vulnerable path); the
//!   only checkpoint is the validated logical one.
//! * **D3 — power-loss durable writes, ordered.** 1b above makes a commit
//!   *process-crash* durable (the mirror is `fsync`'d in lockstep) and
//!   *eventually* file-durable (the debounced D2 checkpoint, ≤ 1/min). Neither
//!   `fsync`s the live DB or its WAL at commit time, so for up to a checkpoint
//!   interval a write acked to the operator survives only in an un-`fsync`'d
//!   WAL — a power loss drops the business rows while the mirror keeps the
//!   audit row.
//!
//!   The [`WriteGuard`] drop now `fsync`s the main file, the WAL and the tenant
//!   directory **BEFORE** it syncs the mirror, and **skips** the mirror sync if
//!   that flush fails. That ordering is the invariant: *the data is durable
//!   before the record that points to it*, so the mirror can never be ahead of
//!   the DB at rest, in either the success or the failure case. Mirror-behind
//!   is benign and self-healing (it reconciles on the next write or at the
//!   pre-snapshot fsync); mirror-ahead is the direction that forks, because
//!   boot's `attempt_db_auto_recovery(mirror_ahead)` → `replay_mirror_delta`
//!   resurrects the delta.
//!
//!   [`Handle::durable_ack`] is the money-path *claim* of that flush's outcome:
//!   it returns the parked `Result` so the operator-facing ack propagates a
//!   failure instead of logging it. Ported from Portable PROD_v2.33.6
//!   (ADR-0110 D3); the frozen census of money-path ack sites is
//!   `tools/adr0110_durable_ack_sites.txt`, held closed in both directions by
//!   `tools/cut_gate_durable_ack.sh`.
//!
//!   The B2 reorder diverges from Portable, where `durable_ack` still does its
//!   own `fsync` after the guard drops. Portable can afford that: it has no
//!   boot mirror-replay, so a mirror-ahead tear there is inert. Defense's
//!   auto-heal makes it live, which is why the ordering had to move.
//!
//! # The single-instance coherence dividend (S335/S341)
//!
//! The pre-fix daemons *deliberately* re-opened per write (`S335`): separate
//! `Connection::open` instances do not share a buffer cache, so a persistent
//! connection would read a **stale chain head** and fork the audit `seq`. The
//! anti-fork guard was the audit-ledger's process-wide `AUDIT_APPEND_LOCK`
//! (`S341`). Collapsing onto **one** instance dissolves that hazard from the
//! other side: a `try_clone` of the shared instance *does* observe every
//! committed row (one shared cache), and [`Handle::write`] serializes writes
//! behind the writer mutex — which subsumes `AUDIT_APPEND_LOCK`'s role for
//! handle-routed writes.
//!
//! # ONE serialization domain (ADR-0105)
//!
//! The Session-B report FLAGGED that audit appends going through
//! `Ledger::append` / `append_reopen` keep their OWN lock. That flag was a real
//! defect, not a note: the writer mutex and `AUDIT_APPEND_LOCK` are DISJOINT, so
//! a writer in each domain could read the same head and both take
//! `seq = head + 1` — `Chain(OutOfOrder { .. })`, the seq-369/416/428/515
//! signature. [`Handle::with_ledger`] closes it: `Ledger`-shaped audit work now
//! runs under the writer mutex on a `try_clone` of the shared instance, so it
//! holds BOTH locks and excludes either domain. Reproduced and pinned in
//! `tests/audit_lock_domain_e2e.rs`. Cross-PROCESS writers remain outside any
//! in-process lock (S335 §3.4), backstopped by the hash chain as before.
//!
//! # No new primitive
//!
//! Per the ADR-0098 Decision, this crate invents **no** durability primitive.
//! It reuses, verbatim: [`aberp_snapshot::live_durable_checkpoint`] /
//! `durable_checkpoint` / `atomic_install` / the verified-good markers /
//! `ensure_not_prod_path`, and [`aberp_audit_ledger::sync_mirror`] /
//! [`aberp_audit_ledger::LedgerMeta`]. It only *routes* access through one
//! instance and *calls* those primitives at the post-commit point.

pub mod debounce;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId};
use duckdb::Connection;

use crate::debounce::CheckpointDebouncer;

/// Typed error surface (ADR-0021 Part A — no `anyhow` in a library crate).
/// The `apps/aberp` daemons wrap these with their own `anyhow` `.context()`.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// Construction refused because the path is (or is under) a frozen prod
    /// root — the same `ensure_not_prod_path` guard the snapshot/recover
    /// entrypoints use (ADR-0093 / ADR-0096). Mechanically prod-safe.
    #[error("aberp-db refuses a prod DB path: {0}")]
    ProdPath(String),

    /// The shared writer mutex was poisoned by a panic in another holder.
    ///
    /// Retained for API back-compat, but ADR-0098 R4 / Fable-5 finding F means
    /// [`Handle::write`] / [`Handle::read`] no longer surface this: a poisoned
    /// writer is now RECOVERED in-place (`clear_poison` + integrity re-verify)
    /// rather than bricking the whole process forever. See
    /// [`Handle::recover_from_poison`].
    #[error("aberp-db writer lock poisoned")]
    Poisoned,

    /// ADR-0098 R4 / Fable-5 finding F — a poisoning panic was recovered
    /// (`clear_poison`), but the POST-POISON integrity re-verify FAILED: on the
    /// freshly re-opened instance the audit hash-chain did not verify
    /// genesis→head. That is real corruption (not a benign prior panic), so it is
    /// surfaced HARD rather than served from a bad DB. See
    /// [`Handle::recover_from_poison`].
    #[error("aberp-db post-poison integrity re-verify failed: {0}")]
    PoisonRecoveryFailed(String),

    /// Underlying DuckDB error (open / try_clone / runtime pragma).
    #[error("duckdb: {0}")]
    Duck(#[from] duckdb::Error),

    /// ADR-0110 D3 — an `fsync` on the money-path durable-ack boundary failed.
    ///
    /// Ported from Portable PROD_v2.33.6. **Callers must propagate this, never
    /// downgrade it to a `warn!`.** The business transaction has already
    /// committed by the time [`Handle::durable_ack`] runs, so the caller is
    /// choosing between "tell the operator it failed when it may have landed"
    /// and "promise durability we did not achieve". CLAUDE.md rule 11 picks the
    /// first. The cut-gate CHECK D3-C fails any call site that swallows it.
    #[error("aberp-db durable-ack fsync failed for {path}: {source}")]
    DurableAck {
        path: PathBuf,
        source: std::io::Error,
    },

    /// ADR-0111 — the live DB file at [`Handle::db_path`] is **no longer the
    /// file this handle's connection is writing to**: something renamed a new
    /// inode over the path while the handle was open, so the shared connection
    /// is stranded on the old, unlinked one.
    ///
    /// Parked as the money path's ack outcome when the [`WriteGuard`] inode
    /// fence trips (see [`WriteGuard::drop`]). The just-committed rows went
    /// into a file the kernel will free at process exit — nothing can make them
    /// durable, so the ack must FAIL rather than certify them (ADR-0110 R3 /
    /// CLAUDE.md rule 11).
    ///
    /// In-process this is unreachable after ADR-0111: every checkpoint goes
    /// through [`Handle::checkpoint_now`], which quiesces and reopens under the
    /// lock. It stays as the belt for an OUT-OF-PROCESS swapper (a second
    /// `aberp` invocation, an operator restore, a backup tool) — the one case
    /// no in-process discipline can prevent.
    #[error(
        "aberp-db: the live DB file at {path} was swapped out from under the open handle \
         (inode changed); writes since the swap went to an orphaned file and are LOST"
    )]
    LiveFileSwapped { path: PathBuf },
}

/// Identity of the file currently at `path`: `(st_dev, st_ino)` on unix.
///
/// `atomic_install` commits by `rename(2)`, which installs a **different**
/// inode at the same path — so a change here is exactly "the live file was
/// swapped", and any fd opened before it now refers to an unlinked file.
///
/// `None` means "cannot tell": a missing file (mid-swap, or a DB not created
/// yet) and every non-unix target. The fence treats `None` as "no evidence of a
/// swap" and stays quiet — it is a belt, and a belt that fires on a stat race
/// would be worse than the hazard. On Windows there is no cheap stable
/// equivalent (`nFileIndex` needs an open handle), which is the same reason
/// `fsync_and_record_dir` no-ops there.
#[cfg(unix)]
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Tunables for a [`Handle`]. [`HandleConfig::default`] is the ADR-0098 D2
/// posture; tests dial the checkpoint off or shorten the window.
#[derive(Debug, Clone)]
pub struct HandleConfig {
    /// ADR-0098 D2 coalescing window for the post-write durable checkpoint.
    pub min_checkpoint_interval: Duration,
    /// Whether to run the debounced durable checkpoint at all. Always `true`
    /// in production; the concurrency-repro test flips it off to isolate the
    /// single-instance property from the checkpoint property.
    pub checkpoint_enabled: bool,
    /// Whether to issue `PRAGMA disable_checkpoint_on_shutdown` on each
    /// runtime connection so dropping it never folds the WAL in place (the
    /// vulnerable in-place checkpoint). Always `true` in production.
    pub disable_implicit_close_checkpoint: bool,
}

impl Default for HandleConfig {
    fn default() -> Self {
        Self {
            min_checkpoint_interval: debounce::DEFAULT_MIN_CHECKPOINT_INTERVAL,
            checkpoint_enabled: true,
            disable_implicit_close_checkpoint: true,
        }
    }
}

/// Mutable state behind the single writer mutex.
struct Inner {
    /// The one shared runtime connection. `Option` because the debounced
    /// durable checkpoint must **drop** it (so the validated checkpoint is
    /// the *only* opener while it swaps the live file) and then **reopen** on
    /// the freshly-installed inode. `None` only transiently, under the lock.
    conn: Option<Connection>,
    /// D2 cadence coordinator (pure; see [`debounce`]).
    debouncer: CheckpointDebouncer,
    /// ADR-0110 D3 B2 — the outcome of the data `fsync` the most recent
    /// [`WriteGuard`] drop performed, waiting to be claimed by
    /// [`Handle::durable_ack`].
    ///
    /// The fsync happens in the guard's `Drop`, which cannot return a `Result`,
    /// so the outcome is parked here and the money path's `durable_ack()` takes
    /// it. `None` means "no write has completed since the last claim" — see
    /// [`Handle::durable_ack`] for what that falls back to.
    last_ack: Option<Result<(), DbError>>,
    /// ADR-0111 inode fence — the `(st_dev, st_ino)` of the live file as it was
    /// when [`Inner::conn`] was opened onto it.
    ///
    /// Re-captured at EVERY open (boot, `ensure_open`, the post-checkpoint
    /// reopen) and compared in [`WriteGuard::drop`]. A mismatch means the path
    /// now names a different inode than the connection holds — the swap-orphan
    /// state. `None` when the identity could not be read (non-unix, or the file
    /// is momentarily absent); the fence then stays silent.
    fence: Option<(u64, u64)>,
}

/// The process-wide shared DuckDB handle (ADR-0098 Gap 1a). Construct once at
/// boot ([`Handle::open`]); share as `Arc<Handle>` into `AppState` and every
/// daemon spawn. **Send + Sync**: the `Connection` (which is `Send` but not
/// `Sync`) lives behind a `Mutex`, and reads are served by owned `try_clone`s.
/// Convenience alias — the shared handle is always reached as
/// `Arc<Handle>` (cloned into `AppState` and every daemon `Deps`).
pub type HandleArc = std::sync::Arc<Handle>;

pub struct Handle {
    db_path: PathBuf,
    /// `<db>.wal` — DuckDB's write-ahead log for [`Self::db_path`]. Precomputed
    /// once because [`Self::durable_ack`] needs it on every money-path ack
    /// (ADR-0110 D3, ported from Portable PROD_v2.33.6).
    wal_path: PathBuf,
    mirror_path: PathBuf,
    /// ADR-0110 D3 — the durability journal: every path this handle has
    /// actually `fsync`'d, in first-sync order, deduped.
    ///
    /// Behind its **own** mutex, never the writer's: [`Self::durable_ack`] runs
    /// AFTER the [`WriteGuard`] has dropped and deliberately does not take the
    /// writer lock (see its docs), so recording must not reach for it either.
    ///
    /// Not telemetry — it is the seam that lets a durability spec DERIVE its
    /// expected set from what the write path really did, so deleting an `fsync`
    /// deletes the path from the set and turns the spec red.
    synced: Mutex<Vec<PathBuf>>,
    /// ADR-0110 D3 B3 — the DIRECTORY half of the durability journal, kept
    /// separate so [`Handle::fsynced_paths`] keeps meaning "files carrying rows"
    /// (the power-loss spec builds its copy manifest from it).
    ///
    /// Split out because the PR #37 adversarial found the parent-directory
    /// `fsync` was pinned by NOTHING: it bypassed the journal entirely, so
    /// deleting it was invisible to both test files, all four gate checks and
    /// all nine probes. Recording it is what makes it assertable.
    synced_dirs: Mutex<Vec<PathBuf>>,
    /// Built **once** per process (S341 semantics): tenant + binary hash. The
    /// lockstep [`aberp_audit_ledger::sync_mirror`] needs it on every commit.
    meta: LedgerMeta,
    /// Plain-string tenant for [`aberp_snapshot::live_durable_checkpoint`].
    tenant: String,
    config: HandleConfig,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("db_path", &self.db_path)
            .field("mirror_path", &self.mirror_path)
            .field("tenant", &self.tenant)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Handle {
    /// Open the live tenant DB **once** and return the shared handle.
    ///
    /// * Refuses a prod path up front (`ensure_not_prod_path`), so the handle
    ///   can never become a prod opener (ADR-0098 adversarial #4).
    /// * Derives the mirror path with [`aberp_audit_ledger::mirror_path_for`]
    ///   (`<db>.audit.log`) — the same convention every other call site uses.
    /// * Builds [`LedgerMeta`] once (S341).
    ///
    /// Call **after** `provision_atomic` / `recover_or_refuse` at boot, when
    /// the live file is known-good (the recovery engine has already run).
    pub fn open(
        db_path: &Path,
        tenant: TenantId,
        config: HandleConfig,
    ) -> Result<Arc<Handle>, DbError> {
        // SAFETY: never let the shared handle become a prod opener.
        aberp_snapshot::ensure_not_prod_path(db_path)
            .map_err(|e| DbError::ProdPath(e.to_string()))?;

        let mirror_path = aberp_audit_ledger::mirror_path_for(db_path);
        // The handle's internal meta is consumed ONLY by the post-commit
        // `sync_mirror` lockstep, which reads `meta.tenant_id()` and NOTHING
        // else (verified against `audit-ledger/src/mirror.rs::sync_mirror`:
        // it appends already-hashed DB rows verbatim and never reads
        // `binary_hash`). So the binary hash — which is background-computed at
        // boot and not ready when the handle is built — is intentionally a
        // fixed placeholder here. Daemons that *create* audit rows build their
        // OWN `LedgerMeta` with the real `binary_hash` they `wait()` for; they
        // never use this meta for `append_in_tx`.
        let meta = LedgerMeta::new(tenant.clone(), BinaryHash::from_bytes([0u8; 32]));
        let conn = open_runtime_connection(db_path, &config)?;
        // Capture the coalescing window before `config` moves into the struct.
        let min_interval = config.min_checkpoint_interval;

        Ok(Arc::new(Handle {
            db_path: db_path.to_path_buf(),
            wal_path: wal_path_for(db_path),
            mirror_path,
            synced: Mutex::new(Vec::new()),
            synced_dirs: Mutex::new(Vec::new()),
            meta,
            tenant: tenant.as_str().to_string(),
            config,
            inner: Mutex::new(Inner {
                conn: Some(conn),
                debouncer: CheckpointDebouncer::new(min_interval),
                last_ack: None,
                // ADR-0111: captured AFTER the open, so it names the inode this
                // connection actually holds.
                fence: file_id(db_path),
            }),
        }))
    }

    /// Production constructor: [`HandleConfig::default`] (D2 posture).
    pub fn open_default(db_path: &Path, tenant: TenantId) -> Result<Arc<Handle>, DbError> {
        Self::open(db_path, tenant, HandleConfig::default())
    }

    /// The live DB path (for callers that still need it for log messages or
    /// to pass to a path-taking helper — *not* to open it).
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// The mirror (`<db>.audit.log`) path.
    pub fn mirror_path(&self) -> &Path {
        &self.mirror_path
    }

    /// Acquire the **serialized writer** over the shared instance. The
    /// returned [`WriteGuard`] derefs to the one `&mut Connection`; run the
    /// existing transaction body against it exactly as before. When the guard
    /// drops, the post-commit hook fires (lockstep mirror + debounced durable
    /// checkpoint). Holding the guard blocks other writers — process-wide
    /// write serialization is the intended single-writer discipline
    /// (ADR-0098 Consequences: a throughput ceiling, acceptable for a
    /// single-operator CNC-shop ERP).
    pub fn write(&self) -> Result<WriteGuard<'_>, DbError> {
        // Finding F (R4): recover a poisoned writer in-place instead of returning
        // `DbError::Poisoned` forever (which would brick every write path for the
        // whole process). See [`Self::lock_recovering`].
        let mut inner = self.lock_recovering()?;
        self.ensure_open(&mut inner)?;
        Ok(WriteGuard {
            handle: self,
            inner,
        })
    }

    /// A read connection: an owned [`duckdb::Connection::try_clone`] of the
    /// **same** instance (shared buffer cache; **not** a second OS open). The
    /// writer mutex is held only for the duration of the clone (cheap), not
    /// for the caller's query, so reads do not serialize behind each other.
    ///
    /// ADR-0098 C2 -> v0.2.5 **Option 1** (Ervin-approved): this `try_clone`
    /// is the SOLE read path. The F5 separate read-only instance is removed --
    /// a separate instance did not replay the live writer's WAL, so post-commit
    /// (WAL-only) writes were invisible to it (the avl `revoke->list` stale
    /// read); a `try_clone` of the shared instance is coherent. The DB-level
    /// write-on-read fail-loud (Gap-2b) is deferred to v0.2.6 (a compile-time
    /// read-guard); see `docs/adr-0098-v0.2.6-scope.md`.
    pub fn read(&self) -> Result<Connection, DbError> {
        // Finding F (R4): same poison-recovery as write() — a reader must not be
        // bricked by another holder's panic either.
        let mut inner = self.lock_recovering()?;
        // ADR-0111 — fence reads too (PR #41 adversarial, lower-severity gap).
        // A `try_clone` of a connection stranded on a swapped-away inode serves
        // the OLD file: after an operator restore the UI would keep reading
        // pre-restore rows, indefinitely and silently, because nothing on the
        // read path ever re-opens. Unlike the write path there is nothing to
        // lose here — no commit has happened — so this just drops the stale
        // connection and lets `ensure_open` below reopen on the live file and
        // re-arm the fence.
        if self.live_file_swapped(&inner) {
            tracing::error!(
                db = %self.db_path.display(),
                "aberp-db: LIVE DB FILE SWAPPED under the open handle (inode changed); the shared \
                 connection was serving the OLD file. Reopening before this read so it cannot \
                 return pre-swap rows (ADR-0111)"
            );
            inner.conn = None;
            inner.fence = None;
        }
        self.ensure_open(&mut inner)?;
        let clone = inner
            .conn
            .as_ref()
            .expect("ensure_open guarantees Some")
            .try_clone()?;
        Ok(clone)
    }

    /// ADR-0105 — run a `Ledger`-shaped audit operation inside the ONE shared
    /// serialization domain.
    ///
    /// # The problem this closes
    ///
    /// There are two audit-append paths and, before this method, two DISJOINT
    /// locks guarding them:
    ///
    /// | path | lock held |
    /// |---|---|
    /// | [`Handle::write`] + `append_in_tx` | this handle's writer mutex (`append_in_tx` takes none) |
    /// | `Ledger::append` / `append_signed` / `append_reopen` | audit-ledger's `AUDIT_APPEND_LOCK` |
    ///
    /// Neither excludes the other. One writer in each domain can read the same
    /// committed head and both self-assign `seq = head + 1`; the `UNIQUE(seq)`
    /// ART is gone, so both rows commit and the hash chain FORKS
    /// (`Chain(OutOfOrder { expected: 2, found: 1 })` — the seq-369/416/428/515
    /// prod signature). Reproduced by
    /// `tests/audit_lock_domain_e2e.rs::concurrent_handle_and_ledger_writers_keep_one_chain`.
    ///
    /// # Why this shape
    ///
    /// The audit-ledger SESSION api (`open_service_session`, `heartbeat`,
    /// `recover_crashed_sessions` — ADR-0087/0088) is written against
    /// `&mut Ledger`, and `Ledger` owns its `Connection`. So a caller cannot
    /// simply borrow the handle's connection. This method closes the gap
    /// without touching that api:
    ///
    /// 1. take the writer mutex (so no handle-routed writer can interleave), then
    /// 2. hand the closure a `Ledger` built from a **`try_clone` of the shared
    ///    instance** — not a fresh `Connection::open`, so there is still exactly
    ///    ONE DuckDB instance / one checkpoint actor (ADR-0098 Gap 1a) and the
    ///    chain head read inside is coherent with every committed row.
    ///
    /// Both properties are load-bearing. A clone alone would NOT be enough —
    /// the two lock domains would still interleave — which is exactly what the
    /// failing-first test pins by using `read()` (a clone) in the racing arm.
    ///
    /// `Ledger::append` inside the closure still takes `AUDIT_APPEND_LOCK`, so
    /// this path holds BOTH locks and therefore excludes writers in either
    /// domain. Lock order is always handle-mutex → `AUDIT_APPEND_LOCK`; no
    /// inversion is possible because `aberp-audit-ledger` does not depend on
    /// this crate and so can never call back in while holding its lock.
    ///
    /// The post-commit lockstep mirror sync + debounced checkpoint run as usual
    /// when the guard drops, after the ledger clone is closed.
    pub fn with_ledger<R>(
        &self,
        binary_hash: BinaryHash,
        f: impl FnOnce(&mut Ledger) -> R,
    ) -> Result<R, DbError> {
        let guard = self.write()?;
        let clone = guard.try_clone()?;
        let mut ledger = Ledger::from_connection(clone, self.meta.tenant_id().clone(), binary_hash);
        let out = f(&mut ledger);
        // Close the clone BEFORE the guard's post-commit hooks (mirror sync +
        // debounced checkpoint) run on drop.
        drop(ledger);
        drop(guard);
        Ok(out)
    }

    /// ADR-0110 D3 — the money-path **claim** that this commit is power-loss
    /// durable, not merely process-crash durable. Ported from Portable
    /// PROD_v2.33.6, with the B2 ordering fix below.
    ///
    /// # It claims; the guard flushes
    ///
    /// The `fsync` of the main file, the WAL and the tenant directory happens in
    /// [`WriteGuard::drop`], **before** that drop syncs the audit mirror. This
    /// returns the parked outcome so the money path can propagate a failure
    /// instead of logging it.
    ///
    /// It reads that way round because of the ordering invariant: *the data must
    /// be durable before the record that points to it.* Doing the `fsync` here —
    /// after the guard has already dropped and synced the mirror, which is what
    /// the first cut of this port did, and what Portable still does — makes the
    /// mirror unconditionally durable FIRST. On the happy path that only shrinks
    /// a window. On the failure path it freezes a fork: the operator is told the
    /// money write FAILED while the mirror durably says it SUCCEEDED, and
    /// Defense's boot `attempt_db_auto_recovery(mirror_ahead)` →
    /// `replay_mirror_delta` then resurrects it. Portable has no mirror replay
    /// at all (verified: zero hits for it in that tree), so the tear is inert
    /// there and live here — which is why Defense reorders and Portable did not
    /// have to.
    ///
    /// # Locking
    ///
    /// Takes the writer mutex briefly to claim the outcome, and does **no** I/O
    /// under it. Call it AFTER `drop(guard)` — calling it while holding a guard
    /// would deadlock on the non-reentrant mutex.
    ///
    /// # Errors
    ///
    /// [`DbError::DurableAck`] if the flush failed. **Propagate it — never
    /// downgrade it to a `warn!`** (cut-gate CHECK D3-C enforces this). When it
    /// fails the mirror sync was skipped, so the mirror is BEHIND the DB, which
    /// is the benign, self-healing direction.
    ///
    /// # Honest scope
    ///
    /// **On macOS this IS a device flush.** `File::sync_all` does not call
    /// `fsync(2)` on Apple targets — the stdlib routes it to
    /// `fcntl(fd, F_FULLFSYNC)` — so the bytes are pushed past the drive's own
    /// write cache rather than merely handed to the OS. Linux gets
    /// `fsync`/`fdatasync`; Windows `FlushFileBuffers` (and the directory step
    /// is skipped there — see [`Handle::fsync_and_record_dir`]).
    ///
    /// The residual bottoms out at the **drive honouring the flush**: a
    /// third-party external enclosure may lie. A tenant on external storage is
    /// outside what this can promise.
    pub fn durable_ack(&self) -> Result<(), DbError> {
        // B2: the fsync itself has already happened, in the WriteGuard's drop,
        // BEFORE that drop synced the mirror. All this does is claim the parked
        // outcome, so the money path can propagate it. Doing the fsync here
        // instead would be too late to fix the ordering — and would be a third,
        // redundant F_FULLFSYNC of bytes already flushed.
        let (claimed, swapped) = {
            let mut inner = self.lock_recovering()?;
            // ADR-0111 — read the inode fence under the SAME lock acquisition
            // that claims the outcome, so the answer cannot change between them.
            let swapped = self.live_file_swapped(&inner);
            (inner.last_ack.take(), swapped)
        };
        match claimed {
            Some(result) => result,
            // No write has completed on this handle since the last claim, so
            // there is no parked outcome. Flush directly: an ack must never
            // silently succeed on the strength of an fsync that never ran.
            // Reached by a money path whose write was a no-op, and by direct
            // callers in tests.
            //
            // ADR-0111 (PR #41 adversarial, finding 3) — but NOT over a swapped
            // file. `fsync_data_paths` opens BY PATH, so after an
            // out-of-process swap it would flush and journal the brand-new
            // inode and return `Ok(())`: an ack certifying a file that has
            // nothing to do with this handle's writes. That is the by-path
            // residual, and this arm is where it survived the guard-drop fence.
            None if swapped => Err(DbError::LiveFileSwapped {
                path: self.db_path.clone(),
            }),
            None => self.fsync_data_paths(),
        }
    }

    /// ADR-0111 — is the file at [`Self::db_path`] a DIFFERENT file from the one
    /// `inner`'s connection was opened onto?
    ///
    /// `atomic_install` (and `restore_into`) commit by `rename`, which installs
    /// a new inode at the same path, so an identity change IS the swap and the
    /// open connection is stranded on an unlinked file.
    ///
    /// Conservative on purpose: `None` on either side — a non-unix target, or
    /// the file momentarily absent mid-swap — reads as "no evidence", not as a
    /// swap. This is a belt; one that fires on a stat race would be worse than
    /// the hazard it guards.
    ///
    /// Must be called with the writer lock held (it takes `&Inner`), which is
    /// also what makes it race-free: the checkpoint that legitimately changes
    /// the inode re-arms `fence` under that same lock.
    fn live_file_swapped(&self, inner: &Inner) -> bool {
        match (inner.fence, file_id(&self.db_path)) {
            (Some(opened_on), Some(now)) => opened_on != now,
            _ => false,
        }
    }

    /// The D3 data flush: main file, then WAL (when present), then the tenant
    /// directory that names them. Main before WAL because a WAL is only
    /// meaningful on top of a durable base; both before the directory entry.
    ///
    /// Called from [`WriteGuard::drop`] BEFORE the mirror sync — see the B2
    /// ordering note on [`Handle::durable_ack`].
    fn fsync_data_paths(&self) -> Result<(), DbError> {
        self.fsync_and_record(&self.db_path)?;
        if self.wal_path.exists() {
            // TOCTOU, deliberately accepted (PR #37 adversarial F2). The D2
            // checkpoint's `atomic_install` unlinks the live WAL, so a
            // concurrent checkpoint can in principle land between this
            // `exists()` and the `File::open` inside `fsync_and_record`,
            // turning a durable write into a money-path error. The adversarial
            // raced 742k acks against 155 checkpointing commits and got ZERO
            // hits: the checkpoint holds the writer mutex across the unlink and
            // this now runs inside a guard drop that also holds it, so the two
            // are in fact serialized. Left as a comment rather than a redesign;
            // if it ever fires the error names the WAL and is not silent.
            self.fsync_and_record(&self.wal_path)?;
        }
        if let Some(parent) = self.db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            self.fsync_and_record_dir(parent)?;
        }
        Ok(())
    }

    /// `fsync` `path` and record it in the durability journal. Recording only
    /// happens on SUCCESS — the journal must mean "this is on stable storage",
    /// never "we tried".
    fn fsync_and_record(&self, path: &Path) -> Result<(), DbError> {
        fsync_path(path)?;
        // A poisoned journal mutex must not fail a write that IS now durable:
        // the journal is evidence, not the contract. Recover in place.
        let mut synced = match self.synced.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                self.synced.clear_poison();
                poisoned.into_inner()
            }
        };
        if !synced.iter().any(|p| p == path) {
            synced.push(path.to_path_buf());
        }
        Ok(())
    }

    /// `fsync` a DIRECTORY and record it in the directory journal (B3).
    ///
    /// Windows: `File::open` on a directory fails there, and a directory
    /// `fsync` has no Windows equivalent (`FlushFileBuffers` needs a file
    /// handle) — the rename durability it buys on POSIX is provided by NTFS
    /// metadata journalling instead. `aberp_snapshot::crash_safe::fsync_dir`
    /// already swallows this; matching that here, because hard-failing every
    /// money path on Windows over a no-op step would be a worse bug than the
    /// one it guards. Nothing is journalled when it is skipped: the journal
    /// must mean "this is on stable storage", never "we tried".
    fn fsync_and_record_dir(&self, dir: &Path) -> Result<(), DbError> {
        match fsync_path(dir) {
            Ok(()) => {}
            #[cfg(windows)]
            Err(_) => return Ok(()),
            #[cfg(not(windows))]
            Err(e) => return Err(e),
        }
        let mut synced = match self.synced_dirs.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                self.synced_dirs.clear_poison();
                poisoned.into_inner()
            }
        };
        if !synced.iter().any(|p| p == dir) {
            synced.push(dir.to_path_buf());
        }
        Ok(())
    }

    /// ADR-0110 D3 B3 — every DIRECTORY this handle has `fsync`'d, in first-sync
    /// order. Kept out of [`Self::fsynced_paths`] so that stays "files carrying
    /// rows" for the power-loss spec's copy manifest.
    ///
    /// Exists because the PR #37 adversarial found the parent-directory `fsync`
    /// was pinned by nothing at all — it bypassed the journal, so deleting it
    /// was invisible to both test files, all four gate checks and all nine
    /// probes.
    pub fn fsynced_dirs(&self) -> Vec<PathBuf> {
        match self.synced_dirs.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// ADR-0110 D3 — every file this handle has `fsync`'d via
    /// [`Self::durable_ack`], in first-sync order. Directories are listed
    /// separately by [`Self::fsynced_dirs`] (they are flushed but carry no
    /// rows, so the power-loss copy manifest must not include them).
    ///
    /// Read by the durability spec to build its expected set out of what the
    /// write path actually certified, so that deleting the `fsync` deletes the
    /// file from the set and turns the spec red.
    pub fn fsynced_paths(&self) -> Vec<PathBuf> {
        match self.synced.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Loop-idle hook (ADR-0098 D2 "+ one at loop-idle"). A daemon calls this
    /// when its queue drains; if the file is dirty since the last checkpoint
    /// we take one now (the cheapest moment), even inside the 1-min window.
    pub fn checkpoint_on_idle(&self) {
        if !self.config.checkpoint_enabled {
            return;
        }
        // Finding F (R4): route the idle-checkpoint lock through the SAME
        // poison-recovery path as write()/read(). The prior `let Ok(..) else
        // return` SILENTLY swallowed a poisoned mutex (dropped the checkpoint AND
        // left the poison un-audited) — exactly what finding F forbids.
        let mut inner = match self.lock_recovering() {
            Ok(inner) => inner,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    db = %self.db_path.display(),
                    "aberp-db: idle checkpoint skipped — writer poison-recovery returned a HARD error (integrity re-verify failed)"
                );
                return;
            }
        };
        if inner.debouncer.should_checkpoint_on_idle() {
            // Automatic cadence: the outcome is already logged inside, and there
            // is no caller to report it to.
            let _ = self.run_durable_checkpoint_locked(&mut inner);
        }
    }

    /// ADR-0111 — take ONE validated durable checkpoint of the live file
    /// **now**, under the writer lock, quiescing and reopening the shared
    /// connection around the swap. **The only sanctioned way for a runtime
    /// caller to checkpoint the live DB.**
    ///
    /// # Why this exists (the mirror-ahead defect)
    ///
    /// [`aberp_snapshot::durable_checkpoint`] finishes with `atomic_install`:
    /// `rename(staging → db_path)` and then it DELETES `<db>.wal`. That is
    /// sound only while the shared connection is quiesced, which is exactly
    /// what [`Self::run_durable_checkpoint_locked`] arranges.
    ///
    /// Two production callers ran that primitive on a PATH from
    /// `spawn_blocking`, holding no lock at all — the periodic snapshot daemon
    /// and the ADR-0095 §3 post-write debouncer. After their rename the shared
    /// connection still held an fd on the OLD, now-unlinked inode. Every
    /// subsequent commit landed in that orphan (freed at process exit) — yet it
    /// was fully visible to the post-commit `sync_mirror` running on that same
    /// connection, so those rows were durably appended to the JSONL mirror. The
    /// mirror ends up **AHEAD of the DB**: the one direction the design calls
    /// unrecoverable, the direction Defense's boot auto-heal RESURRECTS from,
    /// and the root behind the recurring audit-chain fork incidents. It is not
    /// a shutdown-only effect: a long session loses every row written after the
    /// first daemon checkpoint.
    ///
    /// # Deliberately UNCONDITIONAL
    ///
    /// Unlike [`Self::checkpoint_on_idle`] and the [`WriteGuard`] drop hook,
    /// this ignores BOTH the D2 coalescing window (`should_checkpoint_now`) and
    /// [`HandleConfig::checkpoint_enabled`]. Those two gate the handle's own
    /// AUTOMATIC cadence; this method is an explicit caller demand — the
    /// daemon tick, the post-write debounce, the clean-shutdown fold — and each
    /// of those callers already owns its own cadence and env kill-switch. A
    /// silently-skipped checkpoint here would put us straight back to "nothing
    /// folds the live file on a path a crash traverses" (ADR-0095 root #2).
    /// The underlying [`aberp_snapshot::live_durable_checkpoint`] is still a
    /// cheap no-op when a verified-good marker already covers the file, so
    /// calling this often is inexpensive, not a disk thrash.
    ///
    /// # NOT REENTRANT
    ///
    /// This takes the writer mutex. Calling it while a [`WriteGuard`] is alive
    /// **self-deadlocks** — `std::sync::Mutex` is not reentrant. Every call
    /// site must be outside any guard scope. (The three runtime callers are:
    /// the snapshot daemon *after* its cycle returns, the post-write debouncer
    /// on its own blocking task, and the clean-shutdown hook after the drain.)
    ///
    /// Best-effort by contract, like every other post-commit hook here:
    /// `run_durable_checkpoint_locked` logs a failure LOUD (`tracing::error!`,
    /// CLAUDE.md rule 12) and swallows it, so a checkpoint hiccup can never
    /// wedge a daemon tick or process exit — which is why this returns `()`
    /// rather than a `Result` the callers would only be able to log.
    ///
    /// Waits as long as it takes to acquire the lock. A caller that must not
    /// block forever — process exit — wants [`Self::checkpoint_now_within`].
    /// Returns `true` if the checkpoint actually succeeded — callers that
    /// announce it to an operator must not say "installed" on the error arm.
    pub fn checkpoint_now(&self) -> bool {
        // Same poison-recovery path as write()/read()/checkpoint_on_idle: a
        // poisoned mutex must never SILENTLY drop the checkpoint (finding F).
        let mut inner = match self.lock_recovering() {
            Ok(inner) => inner,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    db = %self.db_path.display(),
                    "aberp-db: explicit checkpoint skipped — writer poison-recovery returned a HARD error (integrity re-verify failed)"
                );
                return false;
            }
        };
        self.run_durable_checkpoint_locked(&mut inner)
    }

    /// [`Self::checkpoint_now`] with a **bound on the lock wait**. Returns
    /// `true` if the checkpoint ran, `false` if the writer mutex could not be
    /// acquired within `budget` (logged LOUD; the caller proceeds).
    ///
    /// # Why process exit needs this (PR #41 adversarial, finding 2)
    ///
    /// The clean-shutdown checkpoint used to reason "the drain already
    /// finished, so no `WriteGuard` is alive". That is **false**.
    /// `ShutdownCoordinator::shutdown` races each daemon's `JoinHandle` against
    /// a shared 5 s budget with `tokio::time::timeout`; on expiry it **drops**
    /// the handle. Dropping a tokio `JoinHandle` DETACHES the task, it does not
    /// abort it. So a straggler daemon is still running, can still be inside a
    /// `WriteGuard`, and an unbounded `checkpoint_now` would park on its mutex
    /// for as long as that guard lives — measured at ~1.8 s against a slow
    /// straggler, unbounded in principle. Blocking process exit on a checkpoint
    /// is the original S213 bug, and CLAUDE.md rule 12 forbids it.
    ///
    /// Giving up is the right trade here and costs little: the live file is
    /// simply left for the next boot's recovery path, exactly as it is after
    /// any unclean stop, and the ADR-0095 §3 daemon cadence has been folding it
    /// all along.
    ///
    /// Implemented as a `try_lock` poll rather than a timed lock because
    /// `std::sync::Mutex` has none. The poll interval is deliberately coarse
    /// (`POLL`): this runs once per process, at exit, and a tight spin would
    /// contend with the very guard it is waiting on.
    pub fn checkpoint_now_within(&self, budget: Duration) -> Option<bool> {
        const POLL: Duration = Duration::from_millis(25);
        let deadline = Instant::now() + budget;
        loop {
            match self.inner.try_lock() {
                Ok(mut inner) => {
                    return Some(self.run_durable_checkpoint_locked(&mut inner));
                }
                // Poisoned: fall back to the blocking path, which RECOVERS the
                // poison (finding F) rather than spinning on it forever — the
                // lock is free in this case, so there is nothing to wait for.
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Some(self.checkpoint_now());
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        tracing::error!(
                            db = %self.db_path.display(),
                            budget_ms = budget.as_millis(),
                            "aberp-db: checkpoint SKIPPED — the writer mutex was still held after \
                             the budget. A writer is still running (a shutdown drain drops straggler \
                             JoinHandles, which DETACHES them rather than aborting). Proceeding \
                             without the checkpoint: the live file falls back to the boot recovery \
                             path, exactly as after any unclean stop. Never block exit on this (S213)"
                        );
                        return None;
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }

    /// (Re)open the shared connection if it is not currently present.
    fn ensure_open(&self, inner: &mut Inner) -> Result<(), DbError> {
        if inner.conn.is_none() {
            inner.conn = Some(open_runtime_connection(&self.db_path, &self.config)?);
            // ADR-0111: the fence names the inode the CURRENT connection holds,
            // so it is re-captured with every open — otherwise the first write
            // after any legitimate reopen would trip it.
            inner.fence = file_id(&self.db_path);
        }
        Ok(())
    }

    /// Acquire the writer mutex, RECOVERING from a poisoning panic instead of
    /// surfacing [`DbError::Poisoned`] forever (ADR-0098 R4 / Fable-5 finding F).
    ///
    /// Before the shared Handle (Gap 1a) a daemon that panicked mid-write hurt
    /// only itself. The shared Handle made a panic while holding the
    /// [`WriteGuard`] poison the ONE process-wide writer mutex — bricking every
    /// write path (all ~7 daemons + every request handler) until a process
    /// restart: a NEW single point of failure the shared instance introduced.
    /// This heals it: on a poisoned lock we [`Mutex::clear_poison`], reclaim the
    /// guard via [`std::sync::PoisonError::into_inner`], and run
    /// [`Self::recover_from_poison`]. We recover ONCE (clear_poison), not on
    /// every future acquire. A benign prior panic that left the DB CONSISTENT
    /// resumes; only a FAILED integrity re-verify is a hard error.
    fn lock_recovering(&self) -> Result<MutexGuard<'_, Inner>, DbError> {
        match self.inner.lock() {
            Ok(guard) => Ok(guard),
            Err(poisoned) => {
                self.inner.clear_poison();
                let mut guard = poisoned.into_inner();
                self.recover_from_poison(&mut guard)?;
                Ok(guard)
            }
        }
    }

    /// Post-poison recovery (finding F). Reopen the shared connection FRESH and
    /// re-verify the audit hash-chain genesis→head; loud log + a durable audit
    /// row on success. Returns [`DbError::PoisonRecoveryFailed`] ONLY when the
    /// chain does not verify (real corruption — surfaced, never swallowed).
    fn recover_from_poison(&self, inner: &mut Inner) -> Result<(), DbError> {
        tracing::error!(
            db = %self.db_path.display(),
            "aberp-db: writer mutex POISONED by a panic in a prior guard holder; recovering (clear_poison + drop/reopen + post-poison integrity re-verify) per ADR-0098 R4 / Fable-5 finding F — a poisoned shared writer must NOT brick the whole process"
        );

        // (1) The panicking holder may have left the shared connection mid-
        //     transaction / indeterminate. Drop and reopen FRESH on the same live
        //     inode so recovery starts clean. A failure to reopen IS a hard error
        //     (the DB genuinely will not open) and propagates via `?`.
        inner.conn = None;
        self.ensure_open(inner)?;

        // (2) POST-POISON INTEGRITY RE-VERIFY: verify the audit hash-chain
        //     genesis→head on a try_clone of the freshly-reopened shared instance.
        //     A mere prior panic that left the DB consistent must NOT permanently
        //     brick the process; only a FAILED verify is a hard error.
        let probe = inner
            .conn
            .as_ref()
            .expect("ensure_open guarantees Some")
            .try_clone()?;
        let ledger = Ledger::from_connection(
            probe,
            self.meta.tenant_id().clone(),
            BinaryHash::from_bytes([0u8; 32]),
        );
        let head_seq = match ledger.verify_chain() {
            Ok(seq) => seq,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    db = %self.db_path.display(),
                    "aberp-db: post-poison integrity re-verify FAILED (audit chain does NOT verify genesis→head) — surfacing a HARD error; this is real corruption, not a benign prior panic"
                );
                return Err(DbError::PoisonRecoveryFailed(e.to_string()));
            }
        };

        tracing::warn!(
            db = %self.db_path.display(),
            head_seq,
            "aberp-db: poison-recovery integrity re-verify PASSED (audit chain intact genesis→head); shared writer RESUMED"
        );

        // (3) Audit the recovery (finding F: "must log+audit"). Best-effort: the
        //     mutex is already healed, so a failure to write the forensic row must
        //     not re-brick the writer.
        self.emit_poison_recovery_audit(inner, head_seq);
        Ok(())
    }

    /// Append the poison-recovery forensic audit row. Reuses
    /// [`EventKind::DbAutoRecovered`] (a system/durability event) with a
    /// SCHEMA-VALID `DbAutoRecoveredPayload`: only its free-form `trigger` string
    /// carries a new value (`writer_poison_recovered`) and the single variable is
    /// a machine `u64`, so the payload is hand-formatted (no `serde_json` dep, no
    /// decoder-shape risk). Best-effort by contract; the recovery already
    /// succeeded and was logged loudly before this is attempted.
    fn emit_poison_recovery_audit(&self, inner: &Inner, recovered_head_seq: u64) {
        let probe = match inner.conn.as_ref().map(|c| c.try_clone()) {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                tracing::error!(
                    error = %e,
                    db = %self.db_path.display(),
                    "aberp-db: poison-recovery audit row SKIPPED (try_clone failed); recovery itself succeeded and was logged"
                );
                return;
            }
            None => return,
        };
        // Injection-free: `recovered_max_seq` is the only interpolation and it is
        // a `u64`. Field set + names match `DbAutoRecoveredPayload` exactly so any
        // typed decoder round-trips it (Option -> null).
        let payload = format!(
            "{{\"trigger\":\"writer_poison_recovered\",\"source_snapshot_seq\":0,\
             \"snapshot_audit_count\":0,\"replayed_entries\":0,\
             \"recovered_max_seq\":{recovered_head_seq},\"retained_corrupt_db\":null}}"
        )
        .into_bytes();
        let session_id = format!(
            "aberp-db-poison-recovery-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let actor = Actor::from_local_cli(session_id, "system:aberp-db");
        let mut ledger = Ledger::from_connection(
            probe,
            self.meta.tenant_id().clone(),
            BinaryHash::from_bytes([0u8; 32]),
        );
        match ledger.append(EventKind::DbAutoRecovered, payload, actor, None) {
            Ok(_) => tracing::warn!(
                db = %self.db_path.display(),
                recovered_head_seq,
                "aberp-db: poison-recovery AUDITED (db.auto_recovered, trigger=writer_poison_recovered)"
            ),
            Err(e) => tracing::error!(
                error = %e,
                db = %self.db_path.display(),
                "aberp-db: poison-recovery audit-row append FAILED (non-fatal; the writer is already recovered and the recovery was logged loudly)"
            ),
        }
    }

    /// Run the validated, debounced durable checkpoint **while holding the
    /// writer lock**, quiescing the shared connection around it.
    ///
    /// This is the crux of Session B's correctness. [`durable_checkpoint`]
    /// (which [`live_durable_checkpoint`] calls) opens the live path **fresh**
    /// for its logical `EXPORT` and then `atomic_install`s a new file over it.
    /// If the shared connection stayed open during that:
    ///   1. its fresh `Connection::open` would be a **second concurrent OS
    ///      opener** — the exact tear vector we are removing; and
    ///   2. the `atomic_install` rename would **orphan** the shared
    ///      connection on the old (now-unlinked) inode.
    /// So we **drop** the shared connection first (safe: writes are serialized
    /// and the implicit close-checkpoint is disabled, so the drop folds
    /// nothing in place), let the validated checkpoint be the *only* opener,
    /// then **reopen** on the freshly-installed inode.
    ///
    /// [`durable_checkpoint`]: aberp_snapshot::durable_checkpoint
    /// [`live_durable_checkpoint`]: aberp_snapshot::live_durable_checkpoint
    fn run_durable_checkpoint_locked(&self, inner: &mut Inner) -> bool {
        // 1. Quiesce: drop the shared connection so the checkpoint is the
        //    sole opener of the live file. (No in-place fold: the runtime
        //    connection was opened with checkpoint-on-close disabled.)
        inner.conn = None;

        // 2. The validated logical checkpoint (reused verbatim). `Ok(None)`
        //    means a verified-good marker already covers the file
        //    (`checkpoint_is_current`) — a free no-op.
        //
        //    The outcome is RETURNED, not just logged: the clean-shutdown caller
        //    used to announce "durable checkpoint installed" unconditionally,
        //    including on the error arm below (PR #41 adversarial). A log line
        //    that says the live file was folded when it was not is exactly the
        //    kind of thing an operator reads during a recovery.
        let ok = match aberp_snapshot::live_durable_checkpoint(&self.db_path, &self.tenant) {
            Ok(report) => {
                inner.debouncer.record_checkpoint(Instant::now());
                if report.is_some() {
                    tracing::debug!(
                        db = %self.db_path.display(),
                        "aberp-db: durable checkpoint installed (post-commit, debounced)"
                    );
                }
                true
            }
            Err(e) => {
                // Do NOT poison the write: the business txn already committed.
                // The mirror is current (synced above) and the next due tick
                // retries. Surfaced loudly per [[trust-code-not-operator]].
                tracing::error!(
                    error = %e,
                    db = %self.db_path.display(),
                    "aberp-db: debounced durable checkpoint FAILED (post-commit); \
                     live file falls back to the periodic snapshot recovery path"
                );
                false
            }
        };

        // 3. Reopen on the (possibly newly-installed) inode. If this fails,
        //    leave `conn = None`; the next `write()`/`read()` retries via
        //    `ensure_open` and surfaces the error to that caller.
        //
        //    ADR-0111: re-arm the inode fence on the file we just installed. Our
        //    OWN checkpoint legitimately changes the live inode, so without this
        //    the very next `WriteGuard::drop` would report our own swap as a
        //    hostile one. Re-armed BEFORE the match so it is correct on both
        //    arms; if the reopen failed, `ensure_open` re-captures it on the
        //    retry that actually establishes a connection.
        inner.fence = file_id(&self.db_path);
        match open_runtime_connection(&self.db_path, &self.config) {
            Ok(c) => inner.conn = Some(c),
            Err(e) => tracing::error!(
                error = %e,
                db = %self.db_path.display(),
                "aberp-db: failed to reopen shared connection after checkpoint; \
                 next write/read will retry"
            ),
        }
        ok
    }
}

/// `<db>.wal` — DuckDB's WAL sibling for `db_path`. The extension is
/// **appended** to the whole file name (`aberp.duckdb` → `aberp.duckdb.wal`),
/// not substituted, which `Path::set_extension` would get wrong.
///
/// Re-derived here rather than reused from `aberp_snapshot::crash_safe`: that
/// helper (`wal_sibling`) is private. Defense's `aberp-db` DOES depend on
/// `aberp-snapshot` (unlike Portable's, where the same comment cites avoiding
/// the dep), so the reason here is visibility, not layering — widening the
/// snapshot crate's API for a three-line path join is the bigger change.
fn wal_path_for(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(".wal");
    PathBuf::from(name)
}

/// `fsync` a path's contents + metadata. Works for regular files and, on POSIX,
/// for directories — opening read-only and `sync_all`-ing the fd is the
/// canonical way to persist either (the same shape as
/// `aberp_snapshot::crash_safe::fsync_file`).
///
/// Unlike that helper's directory arm, a failure here is **hard**. It is
/// reached only from [`Handle::durable_ack`], where "we could not make the
/// operator's money-path write durable" is exactly the thing that must not be
/// swallowed (ADR-0110 R3 / CLAUDE.md rule 11).
fn fsync_path(path: &Path) -> Result<(), DbError> {
    let f = std::fs::File::open(path).map_err(|source| DbError::DurableAck {
        path: path.to_path_buf(),
        source,
    })?;
    f.sync_all().map_err(|source| DbError::DurableAck {
        path: path.to_path_buf(),
        source,
    })
}

/// Open one runtime connection to the live tenant DB and apply the
/// single-writer hardening pragmas.
///
/// `disable_checkpoint_on_shutdown` is what makes the quiesce-drop in
/// [`Handle::run_durable_checkpoint_locked`] safe: without it, dropping the
/// connection would trigger DuckDB's implicit close-checkpoint and fold the
/// WAL into the live file **in place** — the very `duckdb#23046` path we are
/// eliminating. With it, the only checkpoint that ever touches the live file
/// is the validated logical one.
///
/// FLAG (CI/Mac-gated): the exact pragma spelling is confirmed VALID against
/// libduckdb 1.5.3 in the e2e build. NOTE (ADR-0098 C2 / review F7): an
/// UNKNOWN pragma is NOT harmless — DuckDB errors HARD on an unrecognised
/// pragma (duckdb#10127), so a future rename/typo here makes `Handle::open`
/// fail and `serve` refuse to boot (fail-hard: loud, every write path down),
/// not silently degrade. That fail-hard posture is desired; the prior
/// "no-op-on-unknown pragma is harmless" wording was wrong and is corrected
/// here. The *behavioural* guarantee (no in-place fold on drop) is asserted by
/// the `daemon_write_killed_mid_checkpoint_is_recoverable` e2e, not the sandbox.
fn open_runtime_connection(db_path: &Path, config: &HandleConfig) -> Result<Connection, DbError> {
    let conn = Connection::open(db_path)?;
    if config.disable_implicit_close_checkpoint {
        // DuckDB folds the WAL into the live file when the last connection
        // closes; disable that so only our validated checkpoint writes the
        // live file. (ADR-0098 1b: "the handle disables DuckDB's implicit
        // checkpoint-on-close for runtime connections.")
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")?;
        // NEW-1(b) (ADR-0098 R6): `disable_checkpoint_on_shutdown` only stops the
        // fold on connection *close*. DuckDB STILL auto-folds the WAL IN PLACE
        // once it grows past `wal_autocheckpoint` (~16MB default) DURING
        // operation — empirically 6 in-place folds under sustained writes on
        // libduckdb 1.5.3, the exact duckdb#23046 path. Raise the threshold to
        // effectively infinite so the engine NEVER self-folds the live file; with
        // (a) reporting a pending WAL as not-current, the ONLY thing that ever
        // folds the live file is the validated build-aside checkpoint. Same
        // `PRAGMA ...;` idiom as above; the spelling is confirmed VALID on 1.5.3
        // (an unknown pragma errors HARD — duckdb#10127 — so a future typo makes
        // `Handle::open` fail loudly, never silently degrade).
        conn.execute_batch("PRAGMA wal_autocheckpoint='1TB';")?;
    }
    Ok(conn)
}

/// RAII writer over the shared instance. Derefs to the one `&mut Connection`.
/// On drop it runs the ADR-0098 1b post-commit hook: a **lockstep** mirror
/// append (always — the mirror tracks the DB continuously) and a **debounced**
/// durable checkpoint (D2). Both are best-effort + loudly logged: the business
/// transaction has already committed by the time the guard drops, so a hook
/// failure must not unwind it — it degrades to the periodic-snapshot recovery
/// path, exactly as before this crate existed.
pub struct WriteGuard<'h> {
    handle: &'h Handle,
    inner: MutexGuard<'h, Inner>,
}

impl WriteGuard<'_> {
    /// The shared writer connection. Run the existing transaction body
    /// (`BEGIN … COMMIT`) against this exactly as the pre-fix code ran it
    /// against its freshly-opened owned connection.
    pub fn conn(&mut self) -> &mut Connection {
        self.inner
            .conn
            .as_mut()
            .expect("write() guarantees an open connection")
    }
}

impl std::ops::Deref for WriteGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.inner
            .conn
            .as_ref()
            .expect("write() guarantees an open connection")
    }
}

impl std::ops::DerefMut for WriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.inner
            .conn
            .as_mut()
            .expect("write() guarantees an open connection")
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        let handle = self.handle;

        // ADR-0111 INODE FENCE — before anything else, prove the file we are
        // about to certify is still the file we just wrote to.
        //
        // `atomic_install` commits a checkpoint by `rename`-ing a NEW inode over
        // `db_path` and unlinking `<db>.wal`. Done under this mutex with a
        // quiesce+reopen (`checkpoint_now`) that is sound. Done by anyone else
        // while this handle is open, it strands the shared connection on the old
        // inode — and then both hooks below actively make things WORSE:
        //
        //   * `fsync_data_paths` opens BY PATH, so it would flush the brand-new
        //     inode and record it in the durability journal — certifying bytes
        //     that have nothing to do with this commit (the D3 by-path residual);
        //   * `sync_mirror` reads the ORPHANED connection, so it would durably
        //     append rows the live file does not contain — mirror ahead of DB,
        //     the direction that forks the chain on the next boot auto-heal.
        //
        // So on a mismatch we do NEITHER, park a hard failure for the money
        // path's `durable_ack` to claim, and drop the stranded connection so the
        // next `write()`/`read()` reopens on the live inode via `ensure_open`.
        // The mirror is left BEHIND — benign and self-healing, which is the
        // whole point of choosing this direction.
        //
        // In-process this is unreachable after ADR-0111 (all three checkpoint
        // sites route through `checkpoint_now`, which re-arms the fence). It is
        // the belt for an out-of-process swapper, and the loud proof that the
        // census gate has not been quietly defeated.
        if handle.live_file_swapped(&self.inner) {
            tracing::error!(
                db = %handle.db_path.display(),
                opened_on = ?self.inner.fence,
                live_now = ?file_id(&handle.db_path),
                "aberp-db: LIVE DB FILE SWAPPED under the open handle (inode changed). \
                 The commit that just finished went to an ORPHANED file and is LOST. \
                 SKIPPING the data fsync and the mirror sync so the mirror cannot end \
                 up ahead of the DB; reopening on the live file. If this fires, some \
                 writer is taking a durable checkpoint outside Handle::checkpoint_now \
                 (ADR-0111), or an in-process operator restore (POST /api/snapshots/restore) \
                 landed on the live path, or another process is rewriting the tenant DB"
            );
            self.inner.last_ack = Some(Err(DbError::LiveFileSwapped {
                path: handle.db_path.clone(),
            }));
            self.inner.conn = None;
            self.inner.fence = None;
            return;
        }

        // D3 B2 — DATA BEFORE THE RECORD THAT POINTS TO IT.
        //
        // The mirror sync below `fsync`s the mirror. Before this reorder that
        // was the FIRST durability event of the commit and `durable_ack` ran
        // strictly after the guard dropped, so the mirror was unconditionally
        // made durable before the DB and WAL were. On the happy path that only
        // shrank a window; on the ack's ERROR path it froze the ordering into a
        // fork: the operator is told the money write FAILED while the mirror
        // durably records that it SUCCEEDED, and Defense's boot auto-heal
        // (`attempt_db_auto_recovery(mirror_ahead)` → `replay_mirror_delta`)
        // then RESURRECTS it. Pre-D3 that state was unreachable.
        //
        // So the data flush moves ahead of the mirror sync, and on failure the
        // mirror sync is SKIPPED. That leaves the mirror BEHIND the DB, which
        // is the benign direction the code already tolerates by design (see the
        // warn! below: it reconciles on the next write or at the pre-snapshot
        // fsync). Mirror-ahead is the only direction that forks.
        //
        // The outcome is parked in `last_ack` for `Handle::durable_ack` to
        // claim, because `Drop` cannot return a `Result`.
        let ack = handle.fsync_data_paths();
        let data_is_durable = ack.is_ok();
        if let Err(e) = &ack {
            tracing::error!(
                error = %e,
                db = %handle.db_path.display(),
                "aberp-db: D3 data fsync FAILED (post-commit); SKIPPING the mirror \
                 sync so the mirror cannot end up ahead of the DB. The mirror stays \
                 behind and reconciles on the next successful write; a money path \
                 will surface this from durable_ack()"
            );
        }
        // Park the outcome for `durable_ack` to claim — FAIL-CLOSED.
        //
        // A money path drops its guard and then calls `durable_ack()`; it holds
        // no lock in between, so another writer's guard can drop in that window
        // and overwrite the parked outcome. Overwriting an Err with a later Ok
        // would let the money path claim someone else's success and ack a write
        // whose own flush failed. So an unclaimed Err STICKS: it is only
        // cleared by being claimed. The inverse mis-attribution (claiming
        // someone else's Err) is the safe direction — it fails an ack that may
        // have been durable, which rule 11 already prefers over the reverse.
        match (&self.inner.last_ack, &ack) {
            (Some(Err(_)), _) => { /* keep the unclaimed failure */ }
            _ => self.inner.last_ack = Some(ack),
        }

        // 1b — LOCKSTEP mirror append (closes Gap 2b at the source). Uses the
        //      shared connection + the once-built meta, so it sees exactly what
        //      the just-finished txn committed. Gated on the data flush above:
        //      a mirror row must never be durable before the row it mirrors.
        if data_is_durable {
            if let Some(conn) = self.inner.conn.as_ref() {
                if let Err(e) =
                    aberp_audit_ledger::sync_mirror(conn, &handle.meta, &handle.mirror_path)
                {
                    tracing::warn!(
                        error = %e,
                        mirror = %handle.mirror_path.display(),
                        "aberp-db: lockstep sync_mirror failed (post-commit); mirror will \
                         reconcile on the next write or at the pre-snapshot fsync"
                    );
                }
            }
        }

        // 1b — DEBOUNCED durable checkpoint (D2). Mark dirty, then fire only
        //      if the coalescing window allows. The actual checkpoint quiesces
        //      and reopens the shared connection (see the method docs).
        self.inner.debouncer.note_write();
        if handle.config.checkpoint_enabled
            && self.inner.debouncer.should_checkpoint_now(Instant::now())
        {
            // Reborrow split: `run_durable_checkpoint_locked` needs `&mut Inner`.
            let inner: &mut Inner = &mut self.inner;
            // Automatic D2 cadence: logged inside, no caller to report to.
            let _ = handle.run_durable_checkpoint_locked(inner);
        }
    }
}
