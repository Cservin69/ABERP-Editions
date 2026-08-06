//! Durability probe for the MES ledger-writer: does every adapter event
//! reach the audit ledger, and does the hash chain survive concurrent
//! adapters? See ADR-0104.
//!
//! ## What this probe originally claimed, and what it actually showed
//!
//! These tests were written by the Trumpf adversarial review to prove
//! that `write_one`'s "logged-and-dropped" write failure lost events in
//! production. Instrumenting the code disproved that: **not one write
//! ever failed.** The loss it measured was `RecvError::Lagged` — the
//! broadcast overflowing `NoopAdapter`'s 16-slot fixture channel — which
//! ADR-0060 explicitly LICENSES. Every real adapter ships a 1024-deep
//! channel (see [`PROD_CHANNEL_CAPACITY`]), so that shape was a fixture
//! artifact and not a production one.
//!
//! The probes are therefore pinned to the production channel depth, which
//! takes the licensed overflow off the table and leaves only the writer's
//! own durability under test. At that depth they still caught three real,
//! undocumented defects, all fixed in ADR-0104:
//!
//! 1. cancellation abandoned the undrained broadcast backlog;
//! 2. a failed append was logged once and dropped, never retried;
//! 3. the writer bypassed `AUDIT_APPEND_LOCK`, so two concurrent adapters
//!    could read the same committed head and **fork the audit chain** —
//!    which is why the concurrency shapes assert `verify_chain` and not
//!    just a row count. A count alone cannot see a fork.

use std::sync::Arc;
use std::time::Duration;

use aberp_audit_ledger::{ensure_schema, BinaryHash, Ledger, TenantId};
use aberp_mes::{
    spawn_ledger_writer, Adapter, CanonicalEvent, LedgerWriterActor, LedgerWriterDeps, NoopAdapter,
};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

/// The broadcast depth every REAL adapter ships with — `barcode_scanner`,
/// `mtconnect`, `zebra`, `ur_rtde` and `trumpf` all define
/// `DEFAULT_CHANNEL_CAPACITY: usize = 1024`, per ADR-0060's "size the
/// receiver generously" mitigation.
///
/// `NoopAdapter::new` defaults to 16 instead. That is a fixture default on
/// a reference adapter which emits nothing in production, and at 16 a
/// burst overflows the channel and loses events to `RecvError::Lagged` —
/// a drop ADR-0060 explicitly LICENSES, and therefore not the defect
/// under test. These probes pin the production depth so what they measure
/// is the writer's own durability, not a fixture's channel depth.
const PROD_CHANNEL_CAPACITY: usize = 1024;

/// Generous ceiling on joining a cancelled ledger-writer. This is a
/// deadlock guard, NOT a timing assertion: the writer is expected to exit
/// as soon as its drain completes. It must comfortably exceed the
/// writer's own 30 s `DRAIN_BUDGET`, because these probes run alongside
/// every other test binary and a loaded machine slows each ~60 ms append.
const JOIN_TIMEOUT_SECS: u64 = 120;

/// Open the ONE shared handle for a probe, exactly as `serve` does at boot.
/// Every writer in a probe shares this single handle — that IS the fix under
/// test, so a probe that handed each writer its own handle would be testing
/// the wrong thing.
fn shared_handle(db_path: &std::path::Path, tenant: &str) -> aberp_db::HandleArc {
    aberp_db::Handle::open_default(db_path, TenantId::new(tenant).expect("tenant"))
        .expect("open shared handle")
}

/// Assert the hash chain has not forked. Two ledger-writers appending
/// concurrently without `AUDIT_APPEND_LOCK` can read the same committed
/// head and write a duplicate `seq`; `verify_chain` is what catches it.
fn assert_chain_intact(db_path: &std::path::Path, tenant: &str, expected: usize) {
    let ledger = Ledger::open(
        db_path,
        TenantId::new(tenant).expect("tenant"),
        BinaryHash::from_bytes([0u8; 32]),
    )
    .expect("reopen ledger for verification");
    let verified = ledger
        .verify_chain()
        .expect("audit chain must verify after concurrent adapter writes");
    assert!(
        verified as usize >= expected,
        "chain verified {verified} entries, expected at least {expected}"
    );
}

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
        db: shared_handle(&db_path, "ten_probe_contention"),
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
    tokio::time::timeout(Duration::from_secs(JOIN_TIMEOUT_SECS), writer)
        .await
        .expect("ledger-writer must finish its shutdown drain before rows are counted")
        .expect("ledger-writer task must not panic");
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

/// A coexisting read-write handle on the same file — the `serve` shape,
/// where the audit DB is also held open by the process. This one never
/// reproduced loss (the coexisting open does not make the writer's own
/// open fail); it is kept as a standing guard on that assumption.
#[tokio::test]
async fn contended_write_still_lands_or_the_event_is_silently_lost() {
    let (rows, note) = run_probe(true).await;
    assert_eq!(
        rows, 1,
        "EVENT LOSS: a coexisting read-write DuckDB handle on the audit DB \
         stopped the ledger-writer's append from landing. note={note}"
    );
}

/// PRODUCTION SHAPE: two adapters, therefore two ledger-writer tasks
/// appending to the SAME audit DB. `mes_manager` spawns one writer per
/// configured adapter, so this is what a shop with a laser and a CNC
/// running at once actually does — and it is the shape that forked the
/// hash chain before the writer moved onto `append_reopen`.
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

    let shared = shared_handle(&db_path, "ten_probe_two");
    let mk_deps = |tag: &str| LedgerWriterDeps {
        db: shared.clone(),
        tenant: TenantId::new("ten_probe_two").expect("tenant"),
        binary_hash: BinaryHash::from_bytes([0u8; 32]),
        actor: LedgerWriterActor {
            session_id: format!("{tag}-{}", Ulid::new()),
            operator_login: "probe".to_string(),
        },
    };

    let a: Arc<NoopAdapter> = Arc::new(NoopAdapter::with_capacity(
        "laser-probe",
        PROD_CHANNEL_CAPACITY,
    ));
    let b: Arc<NoopAdapter> = Arc::new(NoopAdapter::with_capacity(
        "cnc-probe",
        PROD_CHANNEL_CAPACITY,
    ));
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
    tokio::time::timeout(Duration::from_secs(JOIN_TIMEOUT_SECS), wa)
        .await
        .expect("ledger-writer must finish its shutdown drain before rows are counted")
        .expect("ledger-writer task must not panic");
    tokio::time::timeout(Duration::from_secs(JOIN_TIMEOUT_SECS), wb)
        .await
        .expect("ledger-writer must finish its shutdown drain before rows are counted")
        .expect("ledger-writer task must not panic");
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

    // Integrity, not just count: two writers appending without
    // AUDIT_APPEND_LOCK can read the same head and fork the chain.
    assert_chain_intact(&db_path, "ten_probe_two", PER_ADAPTER * 2);
    std::fs::remove_dir_all(&tempdir).ok();

    assert_eq!(
        rows as usize,
        PER_ADAPTER * 2,
        "EVENT LOSS with two adapters running at once: {rows}/{} rows \
         landed. Each ledger-writer drives its own reopen-per-write append; \
         events still buffered when the writer is cancelled must be drained, \
         and a failed append must be retried rather than dropped.",
        PER_ADAPTER * 2
    );
}

/// Higher-concurrency shape: four adapters, therefore four ledger-writer
/// tasks contending on one audit DB. A shop running a laser, a CNC, a
/// robot cell and a label printer at once. Every event must land AND the
/// chain must still verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn four_concurrent_ledger_writers_lose_no_events() {
    const PER_ADAPTER: usize = 20;
    const ADAPTERS: [&str; 4] = ["laser", "cnc", "robot", "labeller"];

    let tempdir = std::env::temp_dir().join(format!("aberp-contention-four-{}", Ulid::new()));
    std::fs::create_dir_all(&tempdir).unwrap();
    let db_path = tempdir.join("audit.duckdb");
    {
        let conn = duckdb::Connection::open(&db_path).unwrap();
        ensure_schema(&conn).unwrap();
    }

    let shared = shared_handle(&db_path, "ten_probe_four");
    let cancel = CancellationToken::new();
    let mut adapters = Vec::new();
    let mut writers = Vec::new();
    for name in ADAPTERS {
        let adapter: Arc<NoopAdapter> =
            Arc::new(NoopAdapter::with_capacity(name, PROD_CHANNEL_CAPACITY));
        adapter.start().await.unwrap();
        let deps = LedgerWriterDeps {
            db: shared.clone(),
            tenant: TenantId::new("ten_probe_four").expect("tenant"),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            actor: LedgerWriterActor {
                session_id: format!("{name}-{}", Ulid::new()),
                operator_login: "probe".to_string(),
            },
        };
        writers.push(spawn_ledger_writer(
            adapter.clone() as Arc<dyn Adapter>,
            deps,
            cancel.clone(),
        ));
        adapters.push(adapter);
    }
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Tight burst — no inter-event sleep, so the writers are guaranteed to
    // fall behind and carry a backlog into cancellation.
    for i in 0..PER_ADAPTER {
        for (adapter, name) in adapters.iter().zip(ADAPTERS) {
            let n = adapter.emit_for_test(CanonicalEvent::ScanReceived {
                scanner_id: name.into(),
                payload: format!("{name}-{i}"),
                symbology: None,
                source_addr: None,
                at_iso8601: "2026-08-05T09:00:00Z".into(),
            });
            assert!(n >= 1, "{name} writer must stay subscribed");
        }
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    cancel.cancel();
    for w in writers {
        tokio::time::timeout(Duration::from_secs(JOIN_TIMEOUT_SECS), w)
            .await
            .expect("ledger-writer must finish its shutdown drain before rows are counted")
            .expect("ledger-writer task must not panic");
    }
    for adapter in &adapters {
        adapter.stop().await.unwrap();
    }

    let conn = duckdb::Connection::open(&db_path).unwrap();
    let rows = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'mes.adapter_event'",
            [],
            |r| r.get::<_, u64>(0),
        )
        .unwrap();
    drop(conn);

    let expected = PER_ADAPTER * ADAPTERS.len();
    assert_chain_intact(&db_path, "ten_probe_four", expected);
    std::fs::remove_dir_all(&tempdir).ok();

    assert_eq!(
        rows as usize, expected,
        "EVENT LOSS with four adapters running at once: {rows}/{expected} rows landed."
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

    let adapter: Arc<NoopAdapter> = Arc::new(NoopAdapter::with_capacity(
        "burst-probe",
        PROD_CHANNEL_CAPACITY,
    ));
    let adapter_for_writer: Arc<dyn Adapter> = adapter.clone();
    adapter.start().await.unwrap();

    let deps = LedgerWriterDeps {
        db: shared_handle(&db_path, "ten_probe_burst"),
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
                    // ADR-0098 R6: an opener that closes WITHOUT this pragma
                    // can fold the shared WAL in place on drop (duckdb#23046)
                    // and destroy a just-committed row. This poller reopens
                    // every 5 ms, so without the guard the READER silently
                    // eats the writer's rows and the probe blames the writer.
                    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
                        .expect("poller pragma guard");
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
    tokio::time::timeout(Duration::from_secs(JOIN_TIMEOUT_SECS), writer)
        .await
        .expect("ledger-writer must finish its shutdown drain before rows are counted")
        .expect("ledger-writer task must not panic");
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
        "EVENT LOSS under a concurrently-polling reader: {rows}/{BURST} rows \
         landed (reader opens={opens}, reader open failures={open_failures}). \
         Every append must survive the reader's file-lock contention via \
         retry, and any backlog must be drained at cancellation."
    );
}
