//! ADR-0095 §3 — post-meaningful-write durable checkpoint (debounced).
//!
//! ADR-0095 wires the chunk-3 crash-safe primitives into the paths a crash
//! actually traverses. This module is the **post-write** leg: after a
//! regulated write (invoice issue / storno / modification commits), the live
//! DuckDB file should be folded into a fresh, verified-good checkpoint soon —
//! not only at the next clean shutdown — so the exposure window after an
//! important commit is bounded (ADR-0095 §3, adversarial review #4).
//!
//! It must never stall issuing, so the trigger is decoupled from the work:
//!
//! - [`trigger`] is a cheap, infallible signal a request handler calls on its
//!   success path; it only wakes the debouncer.
//! - [`PostWriteCheckpoint::run`] is a background loop (spawned at serve boot)
//!   that COALESCES a burst of triggers into ONE [`crate::snapshot::live_checkpoint_logged`]
//!   on a blocking thread, off the request path. That call is a no-op when a
//!   verified-good checkpoint already covers the file, so firing repeatedly is
//!   cheap and never thrashes disk.
//!
//! One tenant DB per serve process (db-per-tenant, ADR-0002), so the
//! coordinator is a process-global [`OnceLock`]: the issue/storno/modification
//! handlers can [`trigger`] without threading a handle through `AppState`, and
//! a CLI process that never installs it sees [`trigger`] as a no-op.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Env kill-switch for the post-write durable checkpoint. The periodic daemon
/// checkpoint (ADR-0095 §3, in `snapshot::run_supervised`) and the
/// clean-shutdown checkpoint are unaffected.
pub const POST_WRITE_CHECKPOINT_DISABLE_ENV: &str = "ABERP_POST_WRITE_CHECKPOINT_DISABLE";
/// Env override (seconds) for the debounce/coalesce window. Default 60
/// (ADR-0095 open question: "coalesce ≤1/min post-write").
pub const POST_WRITE_CHECKPOINT_DEBOUNCE_ENV: &str = "ABERP_POST_WRITE_CHECKPOINT_DEBOUNCE_SECS";
const DEFAULT_DEBOUNCE_SECS: u64 = 60;

/// Process-global coordinator. One tenant DB per serve process, so a single
/// global is exact (and lets the handlers trigger without an `AppState` field).
static GLOBAL: OnceLock<Arc<PostWriteCheckpoint>> = OnceLock::new();

/// Debounced post-write durable-checkpoint coordinator (ADR-0095 §3).
pub struct PostWriteCheckpoint {
    db_path: PathBuf,
    tenant: String,
    debounce: Duration,
    notify: Notify,
}

impl PostWriteCheckpoint {
    fn new(db_path: PathBuf, tenant: String, debounce: Duration) -> Arc<Self> {
        Arc::new(Self {
            db_path,
            tenant,
            debounce,
            notify: Notify::new(),
        })
    }

    /// Signal that a meaningful (regulated) write just committed. Cheap and
    /// infallible — safe to call from a request handler's success path; the
    /// actual checkpoint runs off the request path on a blocking thread.
    pub fn trigger(&self) {
        self.notify.notify_one();
    }

    /// Debounce loop: wake on a trigger, coalesce further triggers for the
    /// debounce window, then take ONE durable checkpoint of the live DB off
    /// the request path. Runs until `cancel` fires (the graceful-shutdown
    /// token). A no-op when `checkpoint_is_current`, so it never thrashes disk.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        tracing::info!(
            db = %self.db_path.display(),
            debounce_secs = self.debounce.as_secs(),
            "post-write durable-checkpoint debouncer started (ADR-0095 §3)"
        );
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.notify.notified() => {}
            }
            // Coalesce a burst of writes into a single checkpoint.
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(self.debounce) => {}
            }
            let db = self.db_path.clone();
            let tenant = self.tenant.clone();
            // DuckDB EXPORT/IMPORT is blocking — run off the async runtime.
            let outcome =
                tokio::task::spawn_blocking(move || crate::snapshot::live_checkpoint_logged(&db, &tenant))
                    .await;
            if let Err(join) = outcome {
                tracing::error!(
                    error = %join,
                    "post-write checkpoint task panicked; debouncer continues"
                );
            }
        }
    }
}

/// `true` if the post-write debouncer is disabled by env.
pub fn is_disabled() -> bool {
    std::env::var(POST_WRITE_CHECKPOINT_DISABLE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn debounce_from_env() -> Duration {
    let secs = std::env::var(POST_WRITE_CHECKPOINT_DEBOUNCE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_DEBOUNCE_SECS);
    Duration::from_secs(secs)
}

/// Install the process-global coordinator for this serve process's tenant DB
/// and return the handle so the caller can spawn its [`PostWriteCheckpoint::run`]
/// loop. Returns `None` when disabled by env or already installed (idempotent).
pub fn install(db_path: PathBuf, tenant: String) -> Option<Arc<PostWriteCheckpoint>> {
    if is_disabled() {
        tracing::info!(
            env = POST_WRITE_CHECKPOINT_DISABLE_ENV,
            "post-write durable checkpoint disabled by env (ADR-0095 §3)"
        );
        return None;
    }
    let coordinator = PostWriteCheckpoint::new(db_path, tenant, debounce_from_env());
    match GLOBAL.set(coordinator.clone()) {
        Ok(()) => Some(coordinator),
        Err(_) => None,
    }
}

/// Signal a meaningful write to the process-global coordinator, if installed.
/// A cheap no-op when never installed (CLI paths, disabled by env), so the
/// call sites in the issue/storno/modification handlers stay unconditional
/// and infallible.
pub fn trigger() {
    if let Some(c) = GLOBAL.get() {
        c.trigger();
    }
}
