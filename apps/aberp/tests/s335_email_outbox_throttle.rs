//! S335 / PR-32 — regression pins for the idle-cycle audit throttle on the
//! email-outbox poll daemon.
//!
//! ## Context
//!
//! S332 (`docs/findings/s332-duckdb-art-email-outbox.md`) diagnosed the
//! live DuckDB ART crash Ervin saw as driven by the daemon emitting one
//! `EmailOutboxFetched` audit row on **every** 5s poll cycle — including
//! idle (zero-row) cycles — ~17k rows/day into the monotonic-`seq` ART,
//! the highest-frequency producer in the ledger.
//!
//! S335 throttles the idle path: a real fetch always emits, an errored
//! cycle always emits (S311 F13/F18 observability), but an idle cycle is
//! silent except for one liveness heartbeat per `HEARTBEAT_INTERVAL`
//! (5 min). These tests pin that behaviour against the daemon's real
//! `poll_once` driven by a hand-rolled storefront mock.
//!
//! The audit event-schema and wire format are UNCHANGED — only emit
//! frequency drops. No new EventKind, no schema migration.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aberp::email_outbox_poll_daemon::{
    poll_once, EmailOutboxDaemonHandle, EmailOutboxEntry, EmailOutboxPollDaemonDeps, OutboxSender,
};
use aberp::storefront_credential::StorefrontCredentialHandle;
use aberp_audit_ledger::{BinaryHash, TenantId};
use anyhow::Result;
use async_trait::async_trait;
use duckdb::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

const VALID_BEARER: &str = "t0k3n";

/// What the mock storefront returns for `GET /api/internal/email-queue`.
#[derive(Clone, Copy)]
enum QueueMode {
    /// Empty queue — the idle path.
    Empty,
    /// `n` canned queue entries — the work path.
    Entries(usize),
}

// ── Capture sender (no real SMTP) ────────────────────────────────────────

#[derive(Default)]
struct CaptureSender;

#[async_trait]
impl OutboxSender for CaptureSender {
    async fn send(&self, _entry: &EmailOutboxEntry) -> Result<()> {
        Ok(())
    }
}

// ── Mock storefront ──────────────────────────────────────────────────────

async fn spawn_mock(mode: QueueMode) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(8 * 1024);
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => return,
                    }
                    if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break idx;
                    }
                    if buf.len() > 64 * 1024 {
                        return;
                    }
                };
                let head = std::str::from_utf8(&buf[..head_end]).unwrap_or("");
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");
                let mut authorization: Option<String> = None;
                for h in lines {
                    if let Some((name, value)) = h.split_once(": ") {
                        if name.eq_ignore_ascii_case("authorization") {
                            authorization = Some(value.trim().to_string());
                        }
                    }
                }
                let response = handle(mode, method, path, authorization.as_deref());
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

fn handle(mode: QueueMode, method: &str, path: &str, authorization: Option<&str>) -> String {
    // Bearer mismatch → 401 (drives the daemon's errored-cycle path).
    if authorization != Some(format!("Bearer {VALID_BEARER}").as_str()) {
        return resp(401, r#"{"error":"unauthorized"}"#);
    }
    let path_no_query = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    if method == "GET" && path_no_query == "/api/internal/email-queue" {
        let entries = match mode {
            QueueMode::Empty => String::new(),
            QueueMode::Entries(n) => (0..n).map(canned_entry).collect::<Vec<_>>().join(","),
        };
        return resp(200, &format!(r#"{{"entries":[{entries}]}}"#));
    }
    if method == "POST" && path_no_query.ends_with("/claim") {
        // Echo a claimed entry (id parsed from the path).
        let id = path_no_query
            .trim_start_matches("/api/internal/email-queue/")
            .trim_end_matches("/claim");
        return resp(200, &canned_entry_with_id(id, "claimed"));
    }
    if method == "POST" && (path_no_query.ends_with("/sent") || path_no_query.ends_with("/failed"))
    {
        return resp(200, r#"{"ok":true}"#);
    }
    resp(404, r#"{"error":"not_found"}"#)
}

fn canned_entry(i: usize) -> String {
    canned_entry_with_id(&format!("01H0000000000000000000000{i}"), "queued")
}

fn canned_entry_with_id(id: &str, state: &str) -> String {
    format!(
        r#"{{"id":"{id}","queued_at":"2026-06-09T12:00:0{n}.000Z","to":["c@example.com"],"cc":[],"subject":"s","body_text":"b","submitter":"submission_received","state":"{state}","attempt_n":0,"last_error":null,"sent_at":null,"audit_id":null}}"#,
        n = id.chars().last().unwrap_or('0')
    )
}

fn resp(status: u16, body: &str) -> String {
    let phrase = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "OK",
    };
    format!(
        "HTTP/1.1 {status} {phrase}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len()
    )
}

// ── Harness helpers ──────────────────────────────────────────────────────

fn scratch_db_path(suffix: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!("aberp-s335-{pid}-{nanos}-{suffix}.duckdb"));
    let _ = std::fs::remove_file(&p);
    p
}

fn make_deps(db_path: &PathBuf, addr: SocketAddr, bearer: &str) -> EmailOutboxPollDaemonDeps {
    {
        let conn = Connection::open(db_path).expect("open scratch DB");
        aberp_audit_ledger::ensure_schema(&conn).expect("ensure audit schema");
    }
    let credential = StorefrontCredentialHandle::dormant();
    credential.set(format!("http://{addr}"), Zeroizing::new(bearer.to_string()));
    EmailOutboxPollDaemonDeps {
        db_path: db_path.clone(),
        tenant: TenantId::new("test").expect("tenant id"),
        binary_hash: BinaryHash::from_bytes([0u8; 32]),
        operator_login: "test".to_string(),
        storefront_credential: credential,
        status: EmailOutboxDaemonHandle::dormant(),
        poll_interval: Duration::from_secs(5),
        sender: Arc::new(CaptureSender) as Arc<dyn OutboxSender>,
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build client")
}

/// Read every `(kind, payload-as-JSON)` row in seq order.
fn read_audit_rows(db_path: &PathBuf) -> Vec<(String, serde_json::Value)> {
    let conn = Connection::open(db_path).expect("open DB");
    aberp_audit_ledger::ensure_schema(&conn).expect("ensure schema");
    let mut stmt = conn
        .prepare("SELECT kind, payload FROM audit_ledger ORDER BY seq ASC")
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .expect("query");
    rows.filter_map(|r| r.ok())
        .map(|(kind, payload)| {
            let v = serde_json::from_slice(&payload).unwrap_or(serde_json::Value::Null);
            (kind, v)
        })
        .collect()
}

fn fetched_rows(rows: &[(String, serde_json::Value)]) -> Vec<&serde_json::Value> {
    rows.iter()
        .filter(|(k, _)| k == "quote.email_outbox_fetched")
        .map(|(_, v)| v)
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────

/// Idle cycles do NOT flood the ledger: many back-to-back idle poll_once
/// cycles collapse to exactly ONE `EmailOutboxFetched` row (the first
/// liveness heartbeat). Pre-S335 this produced one row PER cycle.
#[tokio::test]
async fn s335_email_outbox_idle_cycle_does_not_emit_fetched_audit() {
    let addr = spawn_mock(QueueMode::Empty).await;
    let db_path = scratch_db_path("idle");
    let deps = make_deps(&db_path, addr, VALID_BEARER);
    let client = http_client();

    for _ in 0..10 {
        poll_once(&deps, &client).await;
    }

    let rows = read_audit_rows(&db_path);
    let fetched = fetched_rows(&rows);
    assert_eq!(
        fetched.len(),
        1,
        "10 idle cycles must collapse to 1 heartbeat row, got {} (pre-S335 would be 10)",
        fetched.len()
    );
    // The single heartbeat row is the idle shape: fetched_count 0, no error.
    assert_eq!(fetched[0]["fetched_count"], serde_json::json!(0));
    assert!(
        fetched[0]
            .get("error_class")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "heartbeat must not carry an error_class"
    );
    let _ = std::fs::remove_file(&db_path);
}

/// A non-idle cycle (real batch) emits exactly one `EmailOutboxFetched`
/// row carrying the true `fetched_count`.
#[tokio::test]
async fn s335_email_outbox_non_idle_cycle_does_emit_fetched_audit() {
    let addr = spawn_mock(QueueMode::Entries(2)).await;
    let db_path = scratch_db_path("nonidle");
    let deps = make_deps(&db_path, addr, VALID_BEARER);
    let client = http_client();

    poll_once(&deps, &client).await;

    let rows = read_audit_rows(&db_path);
    let fetched = fetched_rows(&rows);
    assert_eq!(fetched.len(), 1, "one work cycle → exactly one fetched row");
    assert_eq!(
        fetched[0]["fetched_count"],
        serde_json::json!(2),
        "fetched_count must reflect the real batch size"
    );
    let _ = std::fs::remove_file(&db_path);
}

/// An errored cycle (401) ALWAYS emits an `EmailOutboxFetched` row with
/// the `auth_failed` classification — observability is defended even
/// while idle cycles are throttled.
#[tokio::test]
async fn s335_email_outbox_errored_cycle_emits_fetched_audit_with_errored_classification() {
    let addr = spawn_mock(QueueMode::Empty).await;
    let db_path = scratch_db_path("errored");
    // Wrong bearer → mock returns 401 → daemon errored-cycle path.
    let deps = make_deps(&db_path, addr, "wrong-token");
    let client = http_client();

    poll_once(&deps, &client).await;

    let rows = read_audit_rows(&db_path);
    let fetched = fetched_rows(&rows);
    assert_eq!(fetched.len(), 1, "errored cycle must emit one fetched row");
    assert_eq!(fetched[0]["fetched_count"], serde_json::json!(0));
    assert_eq!(
        fetched[0]["error_class"],
        serde_json::json!("auth_failed"),
        "401 must classify as auth_failed"
    );
    let _ = std::fs::remove_file(&db_path);
}

/// Performance / ART-pressure delta: the throttle collapses the per-cycle
/// audit-write volume. 200 idle cycles produce exactly ONE audit write
/// (the heartbeat) instead of 200 — eliminating the every-5s monotonic-seq
/// ART churn that surfaced the prod crash. Wall-clock is dominated by the
/// 200 mock HTTP round-trips, not DuckDB, precisely because the writes are
/// gone; we assert the write COUNT (the real fix) and log the wall-clock.
#[tokio::test]
async fn s335_email_outbox_idle_cycles_collapse_audit_writes() {
    let addr = spawn_mock(QueueMode::Empty).await;
    let db_path = scratch_db_path("perf");
    let deps = make_deps(&db_path, addr, VALID_BEARER);
    let client = http_client();

    let n = 200;
    let start = Instant::now();
    for _ in 0..n {
        poll_once(&deps, &client).await;
    }
    let elapsed = start.elapsed();

    let rows = read_audit_rows(&db_path);
    let fetched = fetched_rows(&rows);
    assert_eq!(
        fetched.len(),
        1,
        "{n} idle cycles must perform exactly 1 audit write, got {} (pre-S335: {n})",
        fetched.len()
    );
    println!(
        "S335 PERF: {n} idle cycles → {} audit write(s) in {:?} (pre-S335 would be {n} writes, each O(ledger) ART checkpoint)",
        fetched.len(),
        elapsed
    );
    let _ = std::fs::remove_file(&db_path);
}
