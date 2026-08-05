//! ADVERSARIAL probes against the Trumpf laser seam (PR #32).
//!
//! Written by the review pass, NOT the implementer. Each test is either
//! a FAILING proof of a defect or a characterisation pin of behaviour
//! the PR does not currently document.
//!
//! Public-API only — nothing here reaches into private items, so it
//! exercises exactly what a future backend author or an operator sees.

use std::sync::Arc;
use std::time::Duration;

use aberp_mes::{
    Adapter, AdapterHealth, CanonicalEvent, LaserSnapshot, MachineState, TrumpfAdapter,
    TrumpfAdapterConfig, TrumpfSource, WorkOrderState,
};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::sync::Mutex as AsyncMutex;

// ===================== harness =====================

fn cfg(machine_id: &str, port: u16) -> TrumpfAdapterConfig {
    TrumpfAdapterConfig {
        machine_id: machine_id.to_string(),
        friendly_name: format!("Adversarial {machine_id}"),
        host: "127.0.0.1".to_string(),
        port,
        device_name: "TruLaser".to_string(),
        poll_interval: Duration::from_millis(150),
        request_timeout: Duration::from_millis(1500),
        slow_threshold: Duration::from_millis(1200),
        max_response_bytes: 8 * 1024 * 1024,
        channel_capacity: 64,
    }
}

async fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

async fn next_event(
    rx: &mut broadcast::Receiver<CanonicalEvent>,
    within: Duration,
) -> Option<CanonicalEvent> {
    tokio::time::timeout(within, rx.recv())
        .await
        .ok()
        .and_then(|r| r.ok())
}

/// Serve `bodies` in order over HTTP/1.1, sticking on the last entry.
async fn spawn_agent(port: u16, bodies: Vec<String>) -> tokio::task::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let seq = Arc::new(AsyncMutex::new(bodies));
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(t) => t,
                Err(_) => return,
            };
            let mut acc = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        acc.extend_from_slice(&buf[..n]);
                        if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }
            let body = {
                let mut g = seq.lock().await;
                if g.len() > 1 {
                    g.remove(0)
                } else {
                    g[0].clone()
                }
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    })
}

fn streams(execution: &str, program: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MTConnectStreams xmlns="urn:mtconnect.org:MTConnectStreams:1.7">
  <Header creationTime="2026-08-05T09:00:00Z" sender="gw" instanceId="1" version="1.7.0" />
  <Streams>
    <DeviceStream name="TruLaser" uuid="trumpf-1">
      <ComponentStream component="Path" name="path" componentId="p">
        <Events>
          <Execution dataItemId="exec">{execution}</Execution>
          <Program dataItemId="prog">{program}</Program>
        </Events>
      </ComponentStream>
    </DeviceStream>
  </Streams>
</MTConnectStreams>"#
    )
}

fn wo(event: &CanonicalEvent) -> (&str, WorkOrderState, WorkOrderState) {
    match event {
        CanonicalEvent::WorkOrderStateChanged {
            work_order_id,
            previous_state,
            new_state,
            ..
        } => (work_order_id.as_str(), *previous_state, *new_state),
        other => panic!("expected WorkOrderStateChanged, got {}", other.type_tag()),
    }
}

/// Drain everything the adapter emits inside `window`.
async fn drain(
    rx: &mut broadcast::Receiver<CanonicalEvent>,
    window: Duration,
) -> Vec<CanonicalEvent> {
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + window;
    while std::time::Instant::now() < deadline {
        match next_event(rx, Duration::from_millis(200)).await {
            Some(e) => out.push(e),
            None => continue,
        }
    }
    out
}

// ===================== ATTACK 1 =====================
// The vendor `Program` string becomes `work_order_id` VERBATIM with no
// length cap, and that string is JSON-serialised into an audit-ledger
// row by `spawn_ledger_writer`.
//
// `poll_once` caps the RESPONSE at `max_response_bytes` (4 MiB in
// production) — but that is a cap on the document, not on the one field
// that escapes into durable storage. A gateway (or a machine operator
// naming a nest) can therefore push a multi-megabyte string into the
// audit ledger on every job edge.
//
// The project's own precedent is the opposite: the barcode-scanner
// adapter pins `DEFAULT_MAX_PAYLOAD_LEN = 4096` and DROPS an oversized
// payload without emitting an event
// (`crates/aberp-mes/src/adapters/barcode_scanner.rs`).
#[tokio::test]
async fn vendor_program_string_is_length_capped_before_it_becomes_a_work_order_id() {
    const SCANNER_PAYLOAD_CAP: usize = 4096; // DEFAULT_MAX_PAYLOAD_LEN

    let port = pick_free_port().await;
    let huge_nest = "N".repeat(512 * 1024);
    let agent = spawn_agent(port, vec![streams("ACTIVE", &huge_nest)]).await;

    let adapter = Arc::new(TrumpfAdapter::new(cfg("laser-huge", port)));
    let mut rx = adapter.subscribe();
    adapter.start().await.unwrap();

    let events = drain(&mut rx, Duration::from_millis(900)).await;
    let job = events
        .iter()
        .find(|e| matches!(e, CanonicalEvent::WorkOrderStateChanged { .. }));
    // The oversized field is `Program` — the machine-state signal is
    // unaffected and must still land. Only the job IDENTITY is untrusted.
    let saw_state = events
        .iter()
        .any(|e| matches!(e, CanonicalEvent::MachineStateChanged { .. }));

    adapter.stop().await.unwrap();
    agent.abort();

    assert!(
        saw_state,
        "the machine-state edge must still be emitted — only the job \
         identity is refused, not the whole poll"
    );

    // ── IMPLEMENTER'S NOTE (fix commit, not the reviewer's original) ──
    // As written this asserted `.expect("a work-order event was emitted")`
    // and then a capped length, which only admits a TRUNCATING fix. The
    // fix taken is the DROPPING one the review itself cites as the
    // project precedent: `barcode_scanner` refuses an oversized line
    // outright rather than shortening it, because a truncated identity is
    // plausible-but-wrong and can collide with a real nest. So the
    // assertion now admits EITHER remedy and still fails on the
    // pre-fix code, where a 512 KB string really did become a
    // `work_order_id` (verified by reverting the cap).
    match job {
        // Refused outright — the barcode-scanner precedent.
        None => {}
        // Or capped, if a later change prefers truncation.
        Some(job) => {
            let (id, _, _) = wo(job);
            assert!(
                id.len() <= SCANNER_PAYLOAD_CAP,
                "an unbounded vendor string ({} bytes) became a work_order_id and \
                 is written verbatim into the audit ledger; the barcode-scanner \
                 adapter caps the equivalent operator-supplied field at {} bytes \
                 and drops the event when it is exceeded",
                id.len(),
                SCANNER_PAYLOAD_CAP
            );
        }
    }
}

// ===================== ATTACK 2 =====================
// A backend that PANICS (rather than returning Err) kills the spawned
// poll task. tokio isolates the panic, so the Defense binary survives —
// that part of the implementer's claim holds. But the cached health is
// never updated again, so the Adapters page reports a STALE `Healthy`
// for a laser that is no longer being polled at all. That is the silent
// misclassification CLAUDE.md rule 12 exists to prevent, and it is the
// exact hazard the module docs cite as the reason the v2 stubs return
// `Err` instead of `todo!()` — the seam avoids one instance of it
// without defending the contract.
//
// Not reachable from the v1 MTConnect backend (no panicking path), but
// `TrumpfAdapter::with_source` is public and is the documented way a
// future OPC-UA backend lands.
#[derive(Debug)]
struct PanickingSource {
    polls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl TrumpfSource for PanickingSource {
    async fn poll(&self) -> Result<LaserSnapshot, String> {
        let n = self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            // First poll (the synchronous one in `start()`) succeeds, so
            // health latches Healthy.
            return Ok(LaserSnapshot {
                machine_state: MachineState::Running,
                ..LaserSnapshot::default()
            });
        }
        panic!("backend blew up on poll {n}");
    }

    fn backend(&self) -> &'static str {
        "panicking"
    }
}

#[tokio::test]
async fn a_panicking_backend_must_not_leave_health_latched_at_stale_healthy() {
    let adapter = Arc::new(TrumpfAdapter::with_source(
        cfg("laser-panic", 1),
        Arc::new(PanickingSource {
            polls: std::sync::atomic::AtomicUsize::new(0),
        }),
    ));
    adapter.start().await.unwrap();
    assert_eq!(
        adapter.health(),
        AdapterHealth::Healthy,
        "first poll is good"
    );

    // Several poll intervals elapse; every one of them panics.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let health = adapter.health();
    adapter.stop().await.unwrap();

    assert_ne!(
        health,
        AdapterHealth::Healthy,
        "the poll task died on a panicking backend and health stayed \
         latched at Healthy — the Adapters page shows a green laser that \
         is no longer polled at all"
    );
}

// ===================== ATTACK 3 (expected to HOLD) =====================
// XML-entity-escaped sentinel: `&#85;NAVAILABLE` decodes to
// "UNAVAILABLE". If normalisation ran BEFORE unescaping this would mint
// a phantom nest. It runs after, so this should hold.
#[tokio::test]
async fn xml_entity_escaped_unavailable_is_still_not_a_job_identity() {
    let port = pick_free_port().await;
    let agent = spawn_agent(port, vec![streams("READY", "&#85;NAVAILABLE")]).await;

    let adapter = Arc::new(TrumpfAdapter::new(cfg("laser-entity", port)));
    let mut rx = adapter.subscribe();
    adapter.start().await.unwrap();

    let events = drain(&mut rx, Duration::from_millis(700)).await;
    adapter.stop().await.unwrap();
    agent.abort();

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CanonicalEvent::WorkOrderStateChanged { .. })),
        "an entity-escaped UNAVAILABLE minted a phantom work order: {events:?}"
    );
}

// ===================== ATTACK 4 (expected to HOLD) =====================
// Hostile whitespace / mixed-case / embedded-newline sentinels.
#[tokio::test]
async fn hostile_sentinel_spellings_never_mint_a_work_order() {
    for spelling in [
        "UNAVAILABLE",
        " UNAVAILABLE ",
        "unavailable",
        "UnAvAiLaBlE",
        "\n\tUNAVAILABLE\n",
        "",
        "   ",
        "\t\n  \r",
    ] {
        let port = pick_free_port().await;
        let agent = spawn_agent(port, vec![streams("READY", spelling)]).await;
        let adapter = Arc::new(TrumpfAdapter::new(cfg("laser-sentinel", port)));
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        let events = drain(&mut rx, Duration::from_millis(500)).await;
        adapter.stop().await.unwrap();
        agent.abort();

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CanonicalEvent::WorkOrderStateChanged { .. })),
            "spelling {spelling:?} minted a phantom work order: {events:?}"
        );
    }
}

// ===================== CHARACTERISATION 5 =====================
// A Streams document carrying TWO `Program` data items (a device with a
// second Path / automation component) resolves LAST-WINS, because the
// shared parser overwrites the field on every matching leaf. For the
// CNC adapter this was inert — it never read `program`. This PR makes it
// load-bearing for work-order identity: if the trailing component
// publishes the sentinel, a genuinely running nest in the first
// component is invisible.
//
// Pinned here so the ambiguity is a deliberate, visible contract rather
// than an accident.
#[tokio::test]
async fn two_program_items_resolve_last_wins_and_a_trailing_sentinel_hides_a_real_nest() {
    let two_paths = r#"<?xml version="1.0" encoding="UTF-8"?>
<MTConnectStreams xmlns="urn:mtconnect.org:MTConnectStreams:1.7">
  <Header creationTime="2026-08-05T09:00:00Z" sender="gw" instanceId="1" version="1.7.0" />
  <Streams>
    <DeviceStream name="TruLaser" uuid="trumpf-1">
      <ComponentStream component="Path" name="cutting" componentId="p1">
        <Events>
          <Execution dataItemId="e1">ACTIVE</Execution>
          <Program dataItemId="g1">NEST_REAL.LST</Program>
        </Events>
      </ComponentStream>
      <ComponentStream component="Path" name="automation" componentId="p2">
        <Events>
          <Program dataItemId="g2">UNAVAILABLE</Program>
        </Events>
      </ComponentStream>
    </DeviceStream>
  </Streams>
</MTConnectStreams>"#
        .to_string();

    let port = pick_free_port().await;
    let agent = spawn_agent(port, vec![two_paths]).await;
    let adapter = Arc::new(TrumpfAdapter::new(cfg("laser-twopath", port)));
    let mut rx = adapter.subscribe();
    adapter.start().await.unwrap();

    let events = drain(&mut rx, Duration::from_millis(700)).await;
    adapter.stop().await.unwrap();
    agent.abort();

    let jobs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, CanonicalEvent::WorkOrderStateChanged { .. }))
        .collect();
    assert!(
        jobs.is_empty(),
        "CHARACTERISATION DRIFT: the parser is no longer last-wins across \
         multiple <Program> items. Re-read the seam's job-identity \
         contract. Got: {jobs:?}"
    );
}

// ===================== CHARACTERISATION 6 =====================
// `stop()` clears the observation, so a nest that was InProgress when
// the adapter stopped never gets a `Completed` — and if it is still
// running at restart, the ledger receives a SECOND
// `Released → InProgress` for the same nest with no completion in
// between. Downstream is inert today (nothing consumes
// `mes.adapter_event`), so this is an audit-trail artefact rather than a
// state-machine bug — but it is neither documented in the emission
// table nor pinned.
#[tokio::test]
async fn restart_with_a_still_running_nest_duplicates_the_start_with_no_completion() {
    let port = pick_free_port().await;
    let agent = spawn_agent(port, vec![streams("ACTIVE", "NEST_LONG.LST")]).await;
    let adapter = Arc::new(TrumpfAdapter::new(cfg("laser-restart-dup", port)));
    let mut rx = adapter.subscribe();

    adapter.start().await.unwrap();
    let first = drain(&mut rx, Duration::from_millis(500)).await;
    adapter.stop().await.unwrap();

    adapter.start().await.unwrap();
    let second = drain(&mut rx, Duration::from_millis(500)).await;
    adapter.stop().await.unwrap();
    agent.abort();

    let starts = |v: &Vec<CanonicalEvent>| {
        v.iter()
            .filter(|e| {
                matches!(
                    e,
                    CanonicalEvent::WorkOrderStateChanged {
                        new_state: WorkOrderState::InProgress,
                        ..
                    }
                )
            })
            .count()
    };
    let completions = |v: &Vec<CanonicalEvent>| {
        v.iter()
            .filter(|e| {
                matches!(
                    e,
                    CanonicalEvent::WorkOrderStateChanged {
                        new_state: WorkOrderState::Completed,
                        ..
                    }
                )
            })
            .count()
    };

    assert_eq!(starts(&first), 1, "first run starts the nest once");
    assert_eq!(
        completions(&first) + completions(&second),
        0,
        "CHARACTERISATION DRIFT: a completion is now emitted across a \
         stop/start — update the emission table."
    );
    assert_eq!(
        starts(&second),
        1,
        "CHARACTERISATION DRIFT: the restart no longer re-starts the \
         still-running nest — update the emission table."
    );
}
