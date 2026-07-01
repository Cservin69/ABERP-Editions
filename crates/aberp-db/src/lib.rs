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
//! handle-routed writes. (Audit appends that still go through
//! `append_reopen` keep their own lock; see the FLAG in the Session-B report.)
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
use std::time::{Duration, Instant};

use aberp_audit_ledger::LedgerMeta;
use aberp_audit_ledger::{BinaryHash, TenantId};
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
    #[error("aberp-db writer lock poisoned")]
    Poisoned,

    /// Underlying DuckDB error (open / try_clone / runtime pragma).
    #[error("duckdb: {0}")]
    Duck(#[from] duckdb::Error),
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
    /// ADR-0098 C2 (review F5 / Gap-2b): when `true`, [`Handle::read`] hands
    /// out a SEPARATE read-only connection (`AccessMode::ReadOnly`) instead of
    /// a read-write `try_clone`, so a write mis-routed through `read()` fails
    /// LOUDLY (DuckDB rejects it) instead of silently committing to the shared
    /// instance and bypassing the post-commit durability hook. `true` in
    /// production. FLAGGED (CI/Mac-gated): a same-process read-only open must
    /// coexist with the Handle's read-write instance on DuckDB 1.5.3; flip to
    /// `false` to fall back to the `try_clone` behaviour if the e2e shows the
    /// read-only open is rejected.
    pub read_returns_readonly: bool,
}

impl Default for HandleConfig {
    fn default() -> Self {
        Self {
            min_checkpoint_interval: debounce::DEFAULT_MIN_CHECKPOINT_INTERVAL,
            checkpoint_enabled: true,
            disable_implicit_close_checkpoint: true,
            read_returns_readonly: true,
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
    mirror_path: PathBuf,
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
            mirror_path,
            meta,
            tenant: tenant.as_str().to_string(),
            config,
            inner: Mutex::new(Inner {
                conn: Some(conn),
                debouncer: CheckpointDebouncer::new(min_interval),
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
        let mut inner = self.inner.lock().map_err(|_| DbError::Poisoned)?;
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
    pub fn read(&self) -> Result<Connection, DbError> {
        if self.config.read_returns_readonly {
            // ADR-0098 C2 (review F5 / Gap-2b): hand out a READ-ONLY connection
            // so a write mis-routed through `read()` FAILS LOUDLY (DuckDB
            // rejects the write) instead of silently committing to the shared
            // instance and bypassing the post-commit hook (lockstep mirror +
            // debounced durable checkpoint). The single-WRITER invariant is
            // intact: the only writable instance is still the Handle's guarded
            // connection, and a read-only handle never checkpoints, so it is
            // NOT a duckdb#23046 tear vector.
            //
            // FLAG (CI/Mac-gated — the top conservative call of C2): this
            // replaces the shared-instance `try_clone` ("one buffer cache, no
            // second OS open") with a separate read-only OS open. The e2e MUST
            // assert (1) DuckDB 1.5.3 ALLOWS a same-process read-only open
            // concurrent with the Handle's read-write instance (else set
            // `read_returns_readonly=false`), and (2) a read-only open observes
            // COMMITTED writes (so the post-commit `verify_chain` reads on the
            // migrated paths see the just-committed append). Writes still flow
            // through `write()` with full coherence.
            return open_runtime_connection_readonly(&self.db_path);
        }
        let mut inner = self.inner.lock().map_err(|_| DbError::Poisoned)?;
        self.ensure_open(&mut inner)?;
        let clone = inner
            .conn
            .as_ref()
            .expect("ensure_open guarantees Some")
            .try_clone()?;
        Ok(clone)
    }

    /// Loop-idle hook (ADR-0098 D2 "+ one at loop-idle"). A daemon calls this
    /// when its queue drains; if the file is dirty since the last checkpoint
    /// we take one now (the cheapest moment), even inside the 1-min window.
    pub fn checkpoint_on_idle(&self) {
        if !self.config.checkpoint_enabled {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            tracing::error!("aberp-db: writer lock poisoned at idle checkpoint");
            return;
        };
        if inner.debouncer.should_checkpoint_on_idle() {
            self.run_durable_checkpoint_locked(&mut inner);
        }
    }

    /// (Re)open the shared connection if it is not currently present.
    fn ensure_open(&self, inner: &mut Inner) -> Result<(), DbError> {
        if inner.conn.is_none() {
            inner.conn = Some(open_runtime_connection(&self.db_path, &self.config)?);
        }
        Ok(())
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
    fn run_durable_checkpoint_locked(&self, inner: &mut Inner) {
        // 1. Quiesce: drop the shared connection so the checkpoint is the
        //    sole opener of the live file. (No in-place fold: the runtime
        //    connection was opened with checkpoint-on-close disabled.)
        inner.conn = None;

        // 2. The validated logical checkpoint (reused verbatim). `Ok(None)`
        //    means a verified-good marker already covers the file
        //    (`checkpoint_is_current`) — a free no-op.
        match aberp_snapshot::live_durable_checkpoint(&self.db_path, &self.tenant) {
            Ok(report) => {
                inner.debouncer.record_checkpoint(Instant::now());
                if report.is_some() {
                    tracing::debug!(
                        db = %self.db_path.display(),
                        "aberp-db: durable checkpoint installed (post-commit, debounced)"
                    );
                }
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
            }
        }

        // 3. Reopen on the (possibly newly-installed) inode. If this fails,
        //    leave `conn = None`; the next `write()`/`read()` retries via
        //    `ensure_open` and surfaces the error to that caller.
        match open_runtime_connection(&self.db_path, &self.config) {
            Ok(c) => inner.conn = Some(c),
            Err(e) => tracing::error!(
                error = %e,
                db = %self.db_path.display(),
                "aberp-db: failed to reopen shared connection after checkpoint; \
                 next write/read will retry"
            ),
        }
    }
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
    }
    Ok(conn)
}

/// Open a SEPARATE **read-only** connection to the live tenant DB (ADR-0098 C2,
/// review F5 / Gap-2b). [`Handle::read`] returns this when
/// `read_returns_readonly` is set: a write issued through it is REJECTED by
/// DuckDB (`AccessMode::ReadOnly`) rather than silently committing to the
/// shared instance and bypassing the post-commit hook.
///
/// Read-only => never checkpoints => NOT a `duckdb#23046` tear vector; the
/// single-WRITER invariant (the Handle's guarded connection is the only
/// writable instance) is preserved. No `disable_checkpoint_on_shutdown` pragma
/// is needed — a read-only connection cannot fold the WAL on close.
///
/// FLAG (CI/Mac-gated): `duckdb::AccessMode` / `Config::access_mode` /
/// `open_with_flags` are otherwise unused in this tree; the exact API + the
/// same-process RO-with-RW coexistence are asserted by the e2e, not the sandbox.
fn open_runtime_connection_readonly(db_path: &Path) -> Result<Connection, DbError> {
    let config = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
    let conn = Connection::open_with_flags(db_path, config)?;
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

        // 1b — LOCKSTEP mirror append (always; cheap; closes Gap 2b at the
        //      source). Uses the shared connection + the once-built meta, so
        //      it sees exactly what the just-finished txn committed.
        if let Some(conn) = self.inner.conn.as_ref() {
            if let Err(e) = aberp_audit_ledger::sync_mirror(conn, &handle.meta, &handle.mirror_path)
            {
                tracing::warn!(
                    error = %e,
                    mirror = %handle.mirror_path.display(),
                    "aberp-db: lockstep sync_mirror failed (post-commit); mirror will \
                     reconcile on the next write or at the pre-snapshot fsync"
                );
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
            handle.run_durable_checkpoint_locked(inner);
        }
    }
}
