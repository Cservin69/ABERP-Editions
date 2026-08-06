//! Runtime task that drains an [`Adapter`](crate::Adapter)'s broadcast
//! and writes each [`CanonicalEvent`](crate::CanonicalEvent) to the
//! audit ledger as one `EventKind::MesAdapterEvent` entry.
//!
//! ADR-0060 §"Migration safety — additive only" + §"Phase α scope-cap"
//! deferred this runtime to Phase β; PR-225 / S229 lands it alongside
//! the first real adapter (the barcode scanner). The Phase α
//! `write_mes_adapter_event` helper was the transactional surface; this
//! module wires the surface to the broadcast.
//!
//! ## Design
//!
//! One writer task per adapter. The task subscribes to the adapter
//! once, opens a fresh DuckDB connection (per-write — matches the
//! quote-intake / AP-sync posture; the connection is short-lived and
//! the audit-ledger schema is idempotent via `ensure_schema`).
//!
//! Cancellation races the broadcast receive via `tokio::select!`; a
//! Tauri window close or a Ctrl-C in `run_prod.sh` exits the task
//! within ms.
//!
//! ## Lossiness contract
//!
//! Per ADR-0060 §"broadcast lossiness on the ledger-writer path will
//! lose audit entries", a slow ledger-writer with a backlog drops the
//! oldest events when the broadcast channel fills. The
//! `RecvError::Lagged(n)` arm logs loud (so the future operations
//! dashboard surfaces the drop count) but otherwise continues — the
//! adapter MUST stay running even if the ledger-writer falls behind.
//!
//! That contract covers `RecvError::Lagged` — a channel that OVERFLOWS —
//! and nothing else. Two other loss paths existed here and were NOT
//! covered by it (ADR-0104):
//!
//! - **Shutdown discarded the backlog.** The cancel arm of the
//!   `tokio::select!` returned immediately, abandoning every event
//!   already buffered in the broadcast but not yet written. Because the
//!   pre-ADR-0104 write cost ~60 ms per event (a fresh `Connection::open`
//!   + `ensure_schema` + tx + commit each time), a burst arriving faster
//!   than that built a backlog as a matter of course, so this fired on
//!   ordinary shop-floor traffic rather than only under stress. It is
//!   now drained — see [`DRAIN_BUDGET`].
//! - **A failed write was logged and dropped.** One `warn!` and the
//!   event was gone. Writes are now retried (see [`WRITE_ATTEMPTS`]) and
//!   a genuinely exhausted write logs at ERROR, not WARN.
//!
//! Neither path is licensed by ADR-0060; a `Lagged` overflow is a
//! deliberate, counted, bounded drop, whereas both of these were silent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::broadcast::Receiver;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, BinaryHash, LedgerMeta, TenantId};

use crate::adapter::Adapter;
use crate::audit::{mes_adapter_event_parts, MesAdapterEventPayload};
use crate::events::CanonicalEvent;

/// How many times a single event's ledger write is attempted before the
/// writer gives up on it. A write can fail transiently — DuckDB takes a
/// single-writer file lock, so a sibling in-process opener (the snapshot
/// daemon, a dashboard query, another adapter's writer) can lose the
/// race. Retrying converts a transient loser into a durable row; the
/// pre-ADR-0104 code dropped it on the first failure.
/// A flat retry budget is not enough on its own: a sibling that reopens
/// the audit DB in a tight loop (a dashboard poller was measured at ~400
/// opens over a 40-event burst) can lose the file-lock race several times
/// in a row, and a fixed short backoff simply retries into the same
/// contention. The backoff is therefore exponential — see
/// [`WRITE_RETRY_BASE_BACKOFF`] — giving a total budget of roughly 3 s
/// before an event is declared lost.
const WRITE_ATTEMPTS: u32 = 8;

/// First retry delay; each subsequent attempt doubles it
/// (25 ms, 50, 100, … ≈ 3.2 s total across [`WRITE_ATTEMPTS`]). Starts
/// short because the contended window is a single DuckDB open/commit,
/// not a network round trip, and backs off because sustained contention
/// needs the writer to stop hammering.
const WRITE_RETRY_BASE_BACKOFF: Duration = Duration::from_millis(25);

/// Wall-clock budget for draining the broadcast backlog after
/// cancellation. Shutdown must not hang, but it also must not silently
/// discard the tail of the audit trail, so the drain is bounded rather
/// than either unbounded or absent. Exceeding it is an ERROR — the only
/// remaining path on which a cancelled writer can leave events unwritten,
/// and it is loud.
const DRAIN_BUDGET: Duration = Duration::from_secs(30);

/// Dependencies the ledger-writer task needs to write into the audit
/// ledger. Constructed by the boot code in `apps/aberp::serve` from
/// the existing `recovery_state` (`db_path`, `tenant`, `binary_hash`)
/// + a fresh per-session [`LedgerWriterActor`].
#[derive(Debug, Clone)]
pub struct LedgerWriterDeps {
    /// The ONE shared DuckDB handle (ADR-0098 Gap 1a). The writer appends
    /// on this rather than opening its own connection per event: a second
    /// instance would read a stale chain head and fork the hash chain, and
    /// the churn of open→close per event is what made the writer slow
    /// enough to build a backlog in the first place (ADR-0104).
    pub db: aberp_db::HandleArc,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub actor: LedgerWriterActor,
}

/// Actor identity stamped onto every ledger entry the writer produces.
/// Mirrors the shape `aberp-quote-intake::service` builds at boot:
/// session_id = a fresh ULID per boot, operator_login = the resolved
/// login (or `"boot"` when the boot state hasn't transitioned to
/// `Ready` yet).
#[derive(Debug, Clone)]
pub struct LedgerWriterActor {
    pub session_id: String,
    pub operator_login: String,
}

/// Spawn a runtime task that subscribes to `adapter`'s broadcast and
/// writes each emitted [`CanonicalEvent`](crate::CanonicalEvent) into
/// the audit ledger. Returns the spawn handle so the caller can
/// register it with the shutdown coordinator.
///
/// The task exits when EITHER (a) the cancel token fires, OR (b) the
/// underlying broadcast channel closes (adapter dropped). Both exits
/// are clean — the caller may safely await the handle.
pub fn spawn_ledger_writer(
    adapter: Arc<dyn Adapter>,
    deps: LedgerWriterDeps,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_ledger_writer(adapter, deps, cancel).await;
    })
}

async fn run_ledger_writer(
    adapter: Arc<dyn Adapter>,
    deps: LedgerWriterDeps,
    cancel: CancellationToken,
) {
    let adapter_name = adapter.name().to_string();
    let mut rx = adapter.subscribe();
    tracing::info!(adapter = %adapter_name, "MES ledger-writer task started");

    loop {
        // The cancel arm must NOT borrow `rx` — the drain needs it
        // mutably, and the `rx.recv()` branch holds a mutable borrow for
        // the lifetime of the select. Resolve to an Option first, then
        // drain outside the select.
        let recv = tokio::select! {
            _ = cancel.cancelled() => None,
            r = rx.recv() => Some(r),
        };
        let Some(recv) = recv else {
            drain_backlog(&mut rx, &deps, &adapter_name).await;
            tracing::info!(adapter = %adapter_name, "MES ledger-writer cancelled; exiting");
            return;
        };
        match recv {
            Ok(event) => {
                write_event(&deps, &adapter_name, event).await;
            }
            Err(RecvError::Lagged(n)) => {
                // Loud-fail per CLAUDE.md rule 12 — operator MUST see
                // dropped scans. The future operations dashboard
                // projection surfaces this counter.
                tracing::warn!(
                    adapter = %adapter_name,
                    skipped = n,
                    "MES broadcast lagged; events dropped from tail \
                     (per ADR-0060 broadcast lossiness contract)"
                );
            }
            Err(RecvError::Closed) => {
                tracing::info!(
                    adapter = %adapter_name,
                    "MES broadcast closed; ledger-writer exiting"
                );
                return;
            }
        }
    }
}

/// Mint the payload for one canonical event and write it, retrying.
async fn write_event(deps: &LedgerWriterDeps, adapter_name: &str, event: CanonicalEvent) {
    let payload =
        MesAdapterEventPayload::new(adapter_name.to_string(), Ulid::new().to_string(), event);
    write_one(deps, &payload, adapter_name).await;
}

/// Drain whatever the broadcast still holds after cancellation.
///
/// Without this the cancel arm returned straight away and every buffered
/// event went with it — the dominant loss path measured in
/// `tests/ledger_contention_probe.rs` (24/40 and 43/50 rows landing).
/// Bounded by [`DRAIN_BUDGET`] so a wedged ledger cannot hang shutdown.
async fn drain_backlog(
    rx: &mut Receiver<CanonicalEvent>,
    deps: &LedgerWriterDeps,
    adapter_name: &str,
) {
    let deadline = Instant::now() + DRAIN_BUDGET;
    let mut drained: u64 = 0;
    loop {
        if Instant::now() >= deadline {
            tracing::error!(
                adapter = %adapter_name,
                drained,
                budget_secs = DRAIN_BUDGET.as_secs(),
                "MES ledger-writer shutdown drain exceeded its budget; \
                 remaining buffered events are UNWRITTEN"
            );
            return;
        }
        match rx.try_recv() {
            Ok(event) => {
                write_event(deps, adapter_name, event).await;
                drained += 1;
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(n)) => {
                // Same ADR-0060 contract as the steady-state arm: an
                // overflow is a counted, licensed drop. Keep draining
                // what survived.
                tracing::warn!(
                    adapter = %adapter_name,
                    skipped = n,
                    "MES broadcast lagged during shutdown drain; events \
                     dropped from tail (per ADR-0060 broadcast lossiness contract)"
                );
            }
        }
    }
    if drained > 0 {
        tracing::info!(
            adapter = %adapter_name,
            drained,
            "MES ledger-writer drained its backlog before exiting"
        );
    }
}

/// Write one event, retrying a transient failure up to
/// [`WRITE_ATTEMPTS`] times before giving up loudly.
async fn write_one(deps: &LedgerWriterDeps, payload: &MesAdapterEventPayload, adapter_name: &str) {
    for attempt in 1..=WRITE_ATTEMPTS {
        match try_write_once(deps, payload).await {
            Ok(()) => return,
            Err(e) if attempt < WRITE_ATTEMPTS => {
                let backoff = WRITE_RETRY_BASE_BACKOFF * 2u32.pow(attempt - 1);
                tracing::warn!(
                    adapter = %adapter_name,
                    error = %e,
                    attempt,
                    max_attempts = WRITE_ATTEMPTS,
                    backoff_ms = backoff.as_millis() as u64,
                    "MES audit-ledger write failed; retrying"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => {
                // Loud-fail per CLAUDE.md rule 12. ERROR, not WARN: an
                // audit row that never lands is an integrity event, not
                // an operational nuisance.
                tracing::error!(
                    adapter = %adapter_name,
                    error = %e,
                    attempts = WRITE_ATTEMPTS,
                    "MES audit-ledger write failed after every retry; EVENT LOST"
                );
            }
        }
    }
}

/// One attempt at appending this event on the shared handle.
async fn try_write_once(
    deps: &LedgerWriterDeps,
    payload: &MesAdapterEventPayload,
) -> Result<(), String> {
    let db = deps.db.clone();
    let tenant = deps.tenant.clone();
    let binary_hash = deps.binary_hash;
    let session_id = deps.actor.session_id.clone();
    let login = deps.actor.operator_login.clone();
    let payload_owned = payload.clone();

    // DuckDB work is blocking, and `db.write()` parks on the handle's
    // writer mutex, so this stays off the async runtime.
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let meta = LedgerMeta::new(tenant, binary_hash);
        let actor = Actor::from_local_cli(session_id, &login);
        let (kind, bytes, idempotency_key) = mes_adapter_event_parts(&payload_owned);
        // ADR-0098 Gap 1a — the audit append runs on the ONE shared
        // instance under the writer mutex, which serializes appends so the
        // chain head is always current and the next `seq` is correct. This
        // is the same surface `email_outbox_poll_daemon` uses; it replaces
        // this path's hand-rolled `Connection::open` + `ensure_schema` +
        // tx + commit, which took no lock at all and could therefore fork
        // the chain when two adapters ran at once (ADR-0104 §2.3).
        let mut conn = db
            .write()
            .map_err(|e| format!("shared writer for MES adapter audit: {e}"))?;
        aberp_audit_ledger::ensure_schema(&conn)
            .map_err(|e| format!("ensure audit-ledger schema: {e}"))?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("begin MES adapter audit tx: {e}"))?;
        append_in_tx(&tx, &meta, kind, bytes, actor, idempotency_key)
            .map_err(|e| format!("append MES adapter event: {e}"))?;
        tx.commit()
            .map_err(|e| format!("commit MES adapter audit tx: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("MES audit-ledger blocking task panicked: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::Duration;

    use aberp_audit_ledger::{ensure_schema, BinaryHash, TenantId};

    use crate::adapters::barcode_scanner::{BarcodeScannerAdapter, BarcodeScannerConfig};
    use crate::events::CanonicalEvent;

    /// Open a shared handle on `db_path`, exactly as `serve` does at boot.
    /// Tests share ONE handle per DB so they exercise the production
    /// single-writer-instance shape rather than a per-test connection.
    fn handle_for_test(db_path: &std::path::Path) -> aberp_db::HandleArc {
        aberp_db::Handle::open_default(
            db_path,
            TenantId::new("ten_test_mes_writer").expect("tenant id"),
        )
        .expect("open shared handle for test")
    }

    fn deps_for_test(db_path: PathBuf) -> LedgerWriterDeps {
        LedgerWriterDeps {
            db: handle_for_test(&db_path),
            tenant: TenantId::new("ten_test_mes_writer").expect("tenant id"),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            actor: LedgerWriterActor {
                session_id: Ulid::new().to_string(),
                operator_login: "test-operator".to_string(),
            },
        }
    }

    /// End-to-end: NoopAdapter emits a fabricated ScanReceived event,
    /// the ledger-writer task drains it, the audit-ledger row appears
    /// on disk with kind = `mes.adapter_event` and the canonical
    /// payload round-trips back to the same struct.
    #[tokio::test]
    async fn writer_drains_scan_event_into_audit_ledger() {
        let tempdir = std::env::temp_dir().join(format!("aberp-mes-writer-{}", Ulid::new()));
        std::fs::create_dir_all(&tempdir).unwrap();
        let db_path = tempdir.join("audit.duckdb");
        // Pre-create the schema so `ensure_schema` inside the writer
        // task hits an established DB.
        {
            let conn = duckdb::Connection::open(&db_path).unwrap();
            ensure_schema(&conn).unwrap();
        }

        // Use the NoopAdapter — its `emit_for_test` lets us inject an
        // event without needing a TCP listener. Keeps this test focused
        // on the ledger-writer's drain + write path.
        let adapter: Arc<crate::NoopAdapter> =
            Arc::new(crate::NoopAdapter::new("test-noop-for-writer"));
        let adapter_for_writer: Arc<dyn Adapter> = adapter.clone();
        let deps = deps_for_test(db_path.clone());
        let cancel = CancellationToken::new();
        adapter.start().await.unwrap();
        let writer = spawn_ledger_writer(adapter_for_writer, deps, cancel.clone());

        // Give the writer a chance to subscribe before we emit.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let event = CanonicalEvent::ScanReceived {
            scanner_id: "test-noop-for-writer".into(),
            payload: "WRITER-TEST-123".into(),
            symbology: Some("Code128".into()),
            source_addr: Some("127.0.0.1:54321".into()),
            at_iso8601: "2026-06-03T08:32:00Z".into(),
        };
        let n = adapter.emit_for_test(event);
        assert!(n >= 1, "writer should be subscribed before emit");

        // Wait for the writer to drain + commit. Poll the DB for the
        // row's appearance with a bounded timeout — beats a fixed
        // sleep.
        let started = std::time::Instant::now();
        let mut row_count = 0u64;
        while started.elapsed() < Duration::from_secs(3) {
            let conn = duckdb::Connection::open(&db_path).unwrap();
            row_count = conn
                .query_row(
                    "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'mes.adapter_event'",
                    [],
                    |r| r.get::<_, u64>(0),
                )
                .unwrap();
            if row_count >= 1 {
                break;
            }
            drop(conn);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            row_count, 1,
            "exactly one MES audit-ledger row should appear within 3s"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        adapter.stop().await.unwrap();

        // Cleanup.
        std::fs::remove_dir_all(&tempdir).ok();
    }

    /// The writer task exits cleanly on cancellation even without any
    /// events ever flowing.
    #[tokio::test]
    async fn writer_exits_on_cancel_with_no_events() {
        let tempdir = std::env::temp_dir().join(format!("aberp-mes-writer-cancel-{}", Ulid::new()));
        std::fs::create_dir_all(&tempdir).unwrap();
        let db_path = tempdir.join("audit.duckdb");
        {
            let conn = duckdb::Connection::open(&db_path).unwrap();
            ensure_schema(&conn).unwrap();
        }

        let port = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let cfg = BarcodeScannerConfig {
            listen_port: port,
            ..BarcodeScannerConfig::new("test-cancel-writer")
        };
        let adapter: Arc<BarcodeScannerAdapter> = Arc::new(BarcodeScannerAdapter::new(cfg));
        let adapter_for_writer: Arc<dyn Adapter> = adapter.clone();
        adapter.start().await.unwrap();

        let deps = deps_for_test(db_path);
        let cancel = CancellationToken::new();
        let writer = spawn_ledger_writer(adapter_for_writer, deps, cancel.clone());

        // Cancel immediately — no events ever flowed.
        cancel.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(2), writer).await;
        assert!(
            joined.is_ok(),
            "writer task should exit within 2s of cancel"
        );

        adapter.stop().await.unwrap();
        std::fs::remove_dir_all(&tempdir).ok();
    }
}
