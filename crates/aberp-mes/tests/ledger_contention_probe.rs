//! Probe: is the ledger-writer's "logged-and-dropped" write failure a
//! TEST artefact only, or a real production event-loss path?
//!
//! `write_one` opens a FRESH `duckdb::Connection` per event. DuckDB takes
//! a single-writer file lock. In `apps/aberp serve` the audit DB is also
//! held open by the process (the ledger writer's own source comment cites
//! ADR-0098 R6 and calls itself a "residual in-serve-process opener").
//!
//! If a coexisting read-write handle makes every `Connection::open` fail,
//! then adapter events are silently dropped in production — a `warn!` and
//! nothing else. This test holds such a handle open and checks whether the
//! event still lands.

use std::sync::Arc;
use std::time::Duration;

use aberp_mes::{
    spawn_ledger_writer, Adapter, CanonicalEvent, LedgerWriterActor, LedgerWriterDeps, NoopAdapter,
};
use aberp_audit_ledger::{ensure_schema, BinaryHash, TenantId};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

async fn run_probe(hold_open: bool) -> (u64, String) {
    let tempdir = std::env::temp_dir().join(format!("aberp-contention-{}", Ulid::new()));
    std::fs::create_dir_all(&tempdir).unwrap();
    let db_path = tempdir.join("audit.duckdb");
    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        ensure_schema(&conn).unwrap();
    }

    // The coexisting read-write handle, standing in for `serve`'s
    // long-lived audit-DB connection.
    let holder = if hold_open {
        match duckdb::Connection::open(&db_path) {
            Ok(c) => Some(c),
            Err(e) => return (0, format!("second open itself failed: {e}")),
        }
    } else {
        None
    };

    let adapter: Arc<NoopAdapter> = Arc::new(NoopAdapter::new("contention-probe"));
    let adapter_for_writer: Arc<dyn Adapter> = adapter.clone();
    adapter.start().await.unwrap();

    let deps = LedgerWriterDeps {
        db_path: db_path.clone(),
        tenant: TenantId::new("ten_probe_contention").expect("tenant"),
        binary_hash: BinaryHash::from_bytes([0u8; 32]),
        actor: LedgerWriterActor {
            session_id: Ulid::new().to_string(),
            operator_login: "probe".to_string(),
        },
    };
    let cancel = CancellationToken::new();
    let writer = spawn_ledger_writer(adapter_for_writer, deps, cancel.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let n = adapter.emit_for_test(CanonicalEvent::ScanReceived {
        scanner_id: "contention-probe".into(),
        payload: "PROBE-1".into(),
        symbology: None,
        source_addr: None,
        at_iso8601: "2026-08-05T09:00:00Z".into(),
    });
    assert!(n >= 1, "writer must be subscribed");

    // Let the writer attempt its write, then quiesce it fully.
    tokio::time::sleep(Duration::from_millis(500)).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    adapter.stop().await.unwrap();
    drop(holder);

    let conn = duckdb::Connection::open(&db_path).unwrap();
    let rows = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'mes.adapter_event'",
            [],
            |r| r.get::<_, u64>(0),
        )
        .unwrap();
    drop(conn);
    std::fs::remove_dir_all(&tempdir).ok();
    (rows, String::new())
}

/// Control: with no coexisting handle the event lands.
#[tokio::test]
async fn control_uncontended_write_lands() {
    let (rows, note) = run_probe(false).await;
    assert_eq!(rows, 1, "uncontended write should land ({note})");
}

/// The real question: with a coexisting read-write handle open on the
/// same file — the production `serve` shape — does the event still reach
/// the ledger, or is it silently dropped?
#[tokio::test]
async fn contended_write_still_lands_or_the_event_is_silently_lost() {
    let (rows, note) = run_probe(true).await;
    assert_eq!(
        rows, 1,
        "SILENT EVENT LOSS: a coexisting read-write DuckDB handle on the \
         audit DB made the ledger-writer's per-event open fail; the event \
         was logged-and-dropped, not retried. note={note}"
    );
}

/// PRODUCTION SHAPE: two adapters, therefore two ledger-writer tasks,
/// each opening its own short-lived `duckdb::Connection` per event on the
/// SAME audit DB. No test-only reader involved. `mes_manager` spawns one
/// writer per configured adapter, so this is what a shop with a laser and
/// a CNC running at once actually does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_ledger_writers_lose_no_events() {
    const PER_ADAPTER: usize = 25;

    let tempdir = std::env::temp_dir().join(format!("aberp-contention-two-{}", Ulid::new()));
    std::fs::create_dir_all(&tempdir).unwrap();
    let db_path = tempdir.join("audit.duckdb");
    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        ensure_schema(&conn).unwrap();
    }

    let mk_deps = |tag: &str| LedgerWriterDeps {
        db_path: db_path.clone(),
        tenant: TenantId::new("ten_probe_two").expect("tenant"),
        binary_hash: BinaryHash::from_bytes([0u8; 32]),
        actor: LedgerWriterActor {
            session_id: format!("{tag}-{}", Ulid::new()),
            operator_login: "probe".to_string(),
        },
    };

    let a: Arc<NoopAdapter> = Arc::new(NoopAdapter::new("laser-probe"));
    let b: Arc<NoopAdapter> = Arc::new(NoopAdapter::new("cnc-probe"));
    a.start().await.unwrap();
    b.start().await.unwrap();
    let cancel = CancellationToken::new();
    let wa = spawn_ledger_writer(a.clone() as Arc<dyn Adapter>, mk_deps("a"), cancel.clone());
    let wb = spawn_ledger_writer(b.clone() as Arc<dyn Adapter>, mk_deps("b"), cancel.clone());
    tokio::time::sleep(Duration::from_millis(150)).await;

    for i in 0..PER_ADAPTER {
        for (adapter, name) in [(&a, "laser-probe"), (&b, "cnc-probe")] {
            let n = adapter.emit_for_test(CanonicalEvent::ScanReceived {
                scanner_id: name.into(),
                payload: format!("{name}-{i}"),
                symbology: None,
                source_addr: None,
                at_iso8601: "2026-08-05T09:00:00Z".into(),
            });
            assert!(n >= 1, "{name} writer must stay subscribed");
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    tokio::time::sleep(Duration::from_millis(2000)).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(20), wa).await;
    let _ = tokio::time::timeout(Duration::from_secs(20), wb).await;
    a.stop().await.unwrap();
    b.stop().await.unwrap();

    let conn = duckdb::Connection::open(&db_path).unwrap();
    let rows = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'mes.adapter_event'",
            [],
            |r| r.get::<_, u64>(0),
        )
        .unwrap();
    drop(conn);
    std::fs::remove_dir_all(&tempdir).ok();

    assert_eq!(
        rows as usize,
        PER_ADAPTER * 2,
        "SILENT EVENT LOSS with two adapters running at once: {rows}/{} rows \
         landed. Each ledger-writer opens its own short-lived DuckDB \
         connection per event; a failed open is logged-and-dropped, never \
         retried.",
        PER_ADAPTER * 2
    );
}

/// The shape the author's e2e actually hit: a reader that repeatedly
/// OPENS a fresh connection and queries while the writer is draining a
/// burst. If any write fails it is logged-and-dropped — the row count
/// will come up short and the loss is permanent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_write_under_a_polling_reader_loses_no_events() {
    const BURST: usize = 40;

    let tempdir = std::env::temp_dir().join(format!("aberp-contention-burst-{}", Ulid::new()));
    std::fs::create_dir_all(&tempdir).unwrap();
    let db_path = tempdir.join("audit.duckdb");
    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        ensure_schema(&conn).unwrap();
    }

    let adapter: Arc<NoopAdapter> = Arc::new(NoopAdapter::new("burst-probe"));
    let adapter_for_writer: Arc<dyn Adapter> = adapter.clone();
    adapter.start().await.unwrap();

    let deps = LedgerWriterDeps {
        db_path: db_path.clone(),
        tenant: TenantId::new("ten_probe_burst").expect("tenant"),
        binary_hash: BinaryHash::from_bytes([0u8; 32]),
        actor: LedgerWriterActor {
            session_id: Ulid::new().to_string(),
            operator_login: "probe".to_string(),
        },
    };
    let cancel = CancellationToken::new();
    let writer = spawn_ledger_writer(adapter_for_writer, deps, cancel.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The concurrent poller: fresh connection per iteration, exactly what
    // the pre-existing `writer_drains_scan_event_into_audit_ledger` test
    // and the author's first e2e draft both did.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poll_path = db_path.clone();
    let poll_stop = stop.clone();
    let poller = std::thread::spawn(move || {
        let mut opens = 0u32;
        let mut open_failures = 0u32;
        while !poll_stop.load(std::sync::atomic::Ordering::SeqCst) {
            match duckdb::Connection::open(&poll_path) {
                Ok(conn) => {
                    opens += 1;
                    let _ = conn.query_row(
                        "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'mes.adapter_event'",
                        [],
                        |r| r.get::<_, u64>(0),
                    );
                }
                Err(_) => open_failures += 1,
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        (opens, open_failures)
    });

    for i in 0..BURST {
        let n = adapter.emit_for_test(CanonicalEvent::ScanReceived {
            scanner_id: "burst-probe".into(),
            payload: format!("BURST-{i}"),
            symbology: None,
            source_addr: None,
            at_iso8601: "2026-08-05T09:00:00Z".into(),
        });
        assert!(n >= 1, "writer must stay subscribed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Quiesce the writer BEFORE stopping the poller, so every write has
    // been attempted under contention.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(20), writer).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let (opens, open_failures) = poller.join().unwrap();
    adapter.stop().await.unwrap();

    let conn = duckdb::Connection::open(&db_path).unwrap();
    let rows = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'mes.adapter_event'",
            [],
            |r| r.get::<_, u64>(0),
        )
        .unwrap();
    drop(conn);
    std::fs::remove_dir_all(&tempdir).ok();

    assert_eq!(
        rows as usize, BURST,
        "SILENT EVENT LOSS under a concurrently-polling reader: {rows}/{BURST} \
         rows landed (reader opens={opens}, reader open failures={open_failures}). \
         Failed ledger writes are logged-and-dropped, never retried."
    );
}
