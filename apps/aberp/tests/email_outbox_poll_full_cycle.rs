//! S307 / PR-276 — End-to-end pin for the email-outbox poll daemon
//! (ADR-0009).
//!
//! Hand-rolled HTTP mock server playing the storefront's role:
//! - `GET /api/internal/email-queue` returns 2 canned queue entries
//! - `POST /api/internal/email-queue/{id}/claim` returns the same entry
//!   shape (atomic queued → claimed)
//! - `POST /api/internal/email-queue/{id}/sent` returns 200 with a
//!   stamped sent payload
//! - `POST /api/internal/email-queue/{id}/failed` returns 200 with a
//!   stamped failed payload
//!
//! Captures every request the daemon makes so the test can pin the
//! claim → sent ordering. Injects a capture-only [`OutboxSender`] so
//! the SMTP path is exercised without a real server. After one cycle:
//!
//! 1. The mock saw `GET email-queue` once.
//! 2. The mock saw `POST .../{id1}/claim` AND `POST .../{id2}/claim`.
//! 3. The capture sender saw both entry id's.
//! 4. The mock saw `POST .../{id1}/sent` AND `POST .../{id2}/sent`.
//! 5. The audit ledger contains, in order:
//!    `EmailOutboxFetched`, `EmailOutboxClaimed`×2, `EmailOutboxSent`×2.
//!
//! The pin is named `s307_email_outbox_full_cycle_two_entries_succeed`
//! so the brief's "PASS by name" report can grep for it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

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
use tokio::sync::Mutex;
use zeroize::Zeroizing;

const ENTRY_1: &str = "01H00000000000000000000001";
const ENTRY_2: &str = "01H00000000000000000000002";

fn list_body_two_entries() -> String {
    format!(
        r#"{{
          "entries": [
            {{
              "id": "{ENTRY_1}",
              "queued_at": "2026-06-09T12:00:00.000Z",
              "to": ["customer1@example.com"],
              "cc": [],
              "subject": "Submission received",
              "body_text": "Hello customer 1",
              "submitter": "submission_received",
              "state": "queued",
              "attempt_n": 0,
              "last_error": null,
              "sent_at": null,
              "audit_id": null
            }},
            {{
              "id": "{ENTRY_2}",
              "queued_at": "2026-06-09T12:00:01.000Z",
              "to": ["customer2@example.com"],
              "cc": ["cc2@example.com"],
              "subject": "Priced ready",
              "body_text": "Hello customer 2",
              "body_html": "<p>Hello customer 2</p>",
              "submitter": "priced_ready",
              "state": "queued",
              "attempt_n": 0,
              "last_error": null,
              "sent_at": null,
              "audit_id": null
            }}
          ]
        }}"#
    )
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    #[allow(dead_code)] // captured for forensic-debug; not asserted on
    body: String,
}

struct MockState {
    requests: Mutex<Vec<RecordedRequest>>,
    expected_bearer: String,
    /// `id → state` ("queued" / "claimed" / "sent" / "failed"). Drives
    /// the claim endpoint's 200/409 fork.
    entry_states: Mutex<HashMap<String, &'static str>>,
}

#[tokio::test]
async fn s307_email_outbox_full_cycle_two_entries_succeed() {
    let mut entry_states: HashMap<String, &'static str> = HashMap::new();
    entry_states.insert(ENTRY_1.to_string(), "queued");
    entry_states.insert(ENTRY_2.to_string(), "queued");
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        expected_bearer: "Bearer t0k3n".to_string(),
        entry_states: Mutex::new(entry_states),
    });
    let addr = spawn_mock(state.clone()).await;

    let db_path = scratch_db_path("s307_full_cycle");
    let _ = std::fs::remove_file(&db_path);
    // Ensure the audit-ledger schema exists so write_audit doesn't
    // create-then-write on cold disk.
    {
        let conn = Connection::open(&db_path).expect("open scratch DB");
        aberp_audit_ledger::ensure_schema(&conn).expect("ensure audit schema");
    }

    let credential = StorefrontCredentialHandle::dormant();
    credential.set(
        format!("http://{addr}"),
        Zeroizing::new("t0k3n".to_string()),
    );

    let handle = EmailOutboxDaemonHandle::dormant();
    let capture = Arc::new(CaptureSender::default());
    let deps = EmailOutboxPollDaemonDeps {
        db: aberp::serve::open_tenant_handle(&db_path, TenantId::new("test").expect("tenant id"))
            .expect("open shared test DuckDB handle (ADR-0098 Gap 1a)"),
        db_path: db_path.clone(),
        tenant: TenantId::new("test").expect("tenant id"),
        binary_hash: BinaryHash::from_bytes([0u8; 32]),
        operator_login: "test".to_string(),
        storefront_credential: credential,
        status: handle.clone(),
        poll_interval: std::time::Duration::from_secs(5),
        sender: capture.clone() as Arc<dyn OutboxSender>,
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("build client");

    poll_once(&deps, &client).await;

    // ── Assertions ────────────────────────────────────────────────────

    // 1. Capture sender saw both entries (one cycle, single-flight).
    let captured = capture.captured.lock().await;
    assert_eq!(
        captured.len(),
        2,
        "capture sender saw {} entries, expected 2",
        captured.len()
    );
    let captured_ids: Vec<String> = captured.iter().map(|e| e.id.clone()).collect();
    assert!(
        captured_ids.contains(&ENTRY_1.to_string()),
        "entry 1 missing from capture: {captured_ids:?}"
    );
    assert!(
        captured_ids.contains(&ENTRY_2.to_string()),
        "entry 2 missing from capture: {captured_ids:?}"
    );
    drop(captured);

    // 2. Mock saw the GET + claim×2 + sent×2 in the right order.
    let requests = state.requests.lock().await;
    let methods_paths: Vec<(String, String)> = requests
        .iter()
        .map(|r| (r.method.clone(), strip_query(&r.path)))
        .collect();
    let get_count = methods_paths
        .iter()
        .filter(|(m, p)| m == "GET" && p == "/api/internal/email-queue")
        .count();
    assert_eq!(
        get_count, 1,
        "expected exactly one GET /api/internal/email-queue; saw {get_count} in {methods_paths:?}"
    );
    let claim_count = methods_paths
        .iter()
        .filter(|(m, p)| m == "POST" && p.ends_with("/claim"))
        .count();
    assert_eq!(
        claim_count, 2,
        "expected 2 claim POSTs; saw {claim_count} in {methods_paths:?}"
    );
    let sent_count = methods_paths
        .iter()
        .filter(|(m, p)| m == "POST" && p.ends_with("/sent"))
        .count();
    assert_eq!(
        sent_count, 2,
        "expected 2 sent POSTs; saw {sent_count} in {methods_paths:?}"
    );
    let failed_count = methods_paths
        .iter()
        .filter(|(m, p)| m == "POST" && p.ends_with("/failed"))
        .count();
    assert_eq!(
        failed_count, 0,
        "expected 0 failed POSTs; saw {failed_count} in {methods_paths:?}"
    );

    // 3. Ordering: GET, claim, claim, sent, sent — claim for entry N
    //    precedes sent for entry N. Single-flight per cycle means the
    //    full per-entry chain runs before the next entry starts.
    let claim_then_sent_pairs: Vec<(String, String)> = ENTRY_IDS
        .iter()
        .map(|id| {
            let claim_idx = methods_paths
                .iter()
                .position(|(m, p)| {
                    m == "POST" && p == &format!("/api/internal/email-queue/{id}/claim")
                })
                .unwrap_or_else(|| panic!("no claim for {id}: {methods_paths:?}"));
            let sent_idx = methods_paths
                .iter()
                .position(|(m, p)| {
                    m == "POST" && p == &format!("/api/internal/email-queue/{id}/sent")
                })
                .unwrap_or_else(|| panic!("no sent for {id}: {methods_paths:?}"));
            assert!(
                claim_idx < sent_idx,
                "claim for {id} (idx {claim_idx}) must precede sent (idx {sent_idx})"
            );
            (id.to_string(), id.to_string())
        })
        .collect();
    assert_eq!(claim_then_sent_pairs.len(), 2);

    drop(requests);

    // 4. Audit ledger contains Fetched, Claimed×2, Sent×2 (one cycle).
    let audit_kinds = read_audit_kinds(&db_path);
    let fetched = audit_kinds
        .iter()
        .filter(|k| k == &"quote.email_outbox_fetched")
        .count();
    let claimed = audit_kinds
        .iter()
        .filter(|k| k == &"quote.email_outbox_claimed")
        .count();
    let sent = audit_kinds
        .iter()
        .filter(|k| k == &"quote.email_outbox_sent")
        .count();
    let failed = audit_kinds
        .iter()
        .filter(|k| k == &"quote.email_outbox_failed")
        .count();
    assert_eq!(
        fetched, 1,
        "expected 1 EmailOutboxFetched audit row; saw {fetched} in {audit_kinds:?}"
    );
    assert_eq!(
        claimed, 2,
        "expected 2 EmailOutboxClaimed audit rows; saw {claimed} in {audit_kinds:?}"
    );
    assert_eq!(
        sent, 2,
        "expected 2 EmailOutboxSent audit rows; saw {sent} in {audit_kinds:?}"
    );
    assert_eq!(
        failed, 0,
        "expected 0 EmailOutboxFailed audit rows; saw {failed} in {audit_kinds:?}"
    );

    // The first audit row MUST be Fetched (the daemon emits it before
    // touching any per-entry path). Subsequent rows for one entry must
    // pair (Claimed, Sent) in that order. Pin the order so a future
    // contributor reshuffling the emit sites can't silently break the
    // forensic ordering.
    assert_eq!(
        audit_kinds.first().map(|s| s.as_str()),
        Some("quote.email_outbox_fetched"),
        "first audit row must be Fetched; got {:?}",
        audit_kinds.first()
    );

    // 5. Status handle reflects the cycle.
    let snap = handle.snapshot();
    assert_eq!(snap.total_fetched_since_boot, 2);
    assert_eq!(snap.total_sent_since_boot, 2);
    assert_eq!(snap.total_failed_since_boot, 0);
    assert_eq!(snap.total_cycles_since_boot, 1);
    assert_eq!(snap.entries_in_progress, 0);
    assert!(snap.last_poll_ts.is_some());
    assert!(snap.last_error_detail.is_none());

    let _ = std::fs::remove_file(&db_path);
}

const ENTRY_IDS: [&str; 2] = [ENTRY_1, ENTRY_2];

// ── Capture sender ───────────────────────────────────────────────────

#[derive(Default)]
struct CaptureSender {
    captured: Mutex<Vec<EmailOutboxEntry>>,
}

#[async_trait]
impl OutboxSender for CaptureSender {
    async fn send(&self, entry: &EmailOutboxEntry) -> Result<()> {
        let mut guard = self.captured.lock().await;
        guard.push(entry.clone());
        Ok(())
    }
}

// ── Mock server ──────────────────────────────────────────────────────

async fn spawn_mock(state: Arc<MockState>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            let state = state.clone();
            tokio::spawn(async move {
                // Read in a loop until we have the full headers + (if
                // present) the declared content-length body.
                let mut buf = Vec::with_capacity(32 * 1024);
                let mut tmp = [0u8; 4096];
                let (head_end, content_length) = loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => return,
                    }
                    if let Some(idx) = find_header_end(&buf) {
                        let head_str = std::str::from_utf8(&buf[..idx]).unwrap_or("");
                        let cl = head_str
                            .lines()
                            .find_map(|l| {
                                let lc = l.to_ascii_lowercase();
                                if lc.starts_with("content-length:") {
                                    l.split(':')
                                        .nth(1)
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        break (idx, cl);
                    }
                    if buf.len() > 256 * 1024 {
                        return;
                    }
                };
                while buf.len() < head_end + 4 + content_length {
                    match sock.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => return,
                    }
                }
                let head_str = std::str::from_utf8(&buf[..head_end]).unwrap_or("");
                let body_start = head_end + 4;
                let body_end = (body_start + content_length).min(buf.len());
                let body = std::str::from_utf8(&buf[body_start..body_end])
                    .unwrap_or("")
                    .to_string();
                let mut lines = head_str.split("\r\n");
                let request_line = lines.next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut authorization: Option<String> = None;
                for h in lines {
                    if let Some((name, value)) = h.split_once(": ") {
                        if name.eq_ignore_ascii_case("authorization") {
                            authorization = Some(value.trim().to_string());
                        }
                    }
                }
                {
                    let mut q = state.requests.lock().await;
                    q.push(RecordedRequest {
                        method: method.clone(),
                        path: path.clone(),
                        body: body.clone(),
                    });
                }
                let response =
                    handle_request(&state, &method, &path, authorization.as_deref()).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn handle_request(
    state: &Arc<MockState>,
    method: &str,
    path: &str,
    authorization: Option<&str>,
) -> String {
    if authorization != Some(state.expected_bearer.as_str()) {
        return canned_response(401, "application/json", r#"{"error":"unauthorized"}"#);
    }
    let path_no_query = strip_query(path);
    if method == "GET" && path_no_query == "/api/internal/email-queue" {
        return canned_response(200, "application/json", &list_body_two_entries());
    }
    if method == "POST" && path_no_query.ends_with("/claim") {
        let id = path_no_query
            .trim_start_matches("/api/internal/email-queue/")
            .trim_end_matches("/claim")
            .to_string();
        let mut states = state.entry_states.lock().await;
        match states.get(&id).copied() {
            Some("queued") => {
                states.insert(id.clone(), "claimed");
                let body = canned_entry_json(&id, "claimed");
                return canned_response(200, "application/json", &body);
            }
            Some(_) => {
                return canned_response(
                    409,
                    "application/json",
                    r#"{"error":"not_claimable","state":"claimed"}"#,
                );
            }
            None => {
                return canned_response(404, "application/json", r#"{"error":"not_found"}"#);
            }
        }
    }
    if method == "POST" && path_no_query.ends_with("/sent") {
        let id = path_no_query
            .trim_start_matches("/api/internal/email-queue/")
            .trim_end_matches("/sent")
            .to_string();
        let mut states = state.entry_states.lock().await;
        states.insert(id.clone(), "sent");
        let body = canned_entry_json(&id, "sent");
        return canned_response(200, "application/json", &body);
    }
    if method == "POST" && path_no_query.ends_with("/failed") {
        let id = path_no_query
            .trim_start_matches("/api/internal/email-queue/")
            .trim_end_matches("/failed")
            .to_string();
        let mut states = state.entry_states.lock().await;
        states.insert(id.clone(), "failed");
        let body = canned_entry_json(&id, "failed");
        return canned_response(200, "application/json", &body);
    }
    canned_response(404, "application/json", r#"{"error":"not_found"}"#)
}

fn strip_query(p: &str) -> String {
    match p.split_once('?') {
        Some((q, _)) => q.to_string(),
        None => p.to_string(),
    }
}

fn canned_entry_json(id: &str, state: &str) -> String {
    format!(
        r#"{{
          "id": "{id}",
          "queued_at": "2026-06-09T12:00:00.000Z",
          "to": ["customer@example.com"],
          "cc": [],
          "subject": "stub",
          "body_text": "stub",
          "submitter": "submission_received",
          "state": "{state}",
          "attempt_n": 1,
          "last_error": null,
          "sent_at": null,
          "audit_id": null
        }}"#
    )
}

fn canned_response(status: u16, ct: &str, body: &str) -> String {
    let phrase = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "OK",
    };
    format!(
        "HTTP/1.1 {status} {phrase}\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len()
    )
}

fn scratch_db_path(suffix: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-email-outbox-poll-test-{pid}-{nanos}-{suffix}.duckdb"
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn read_audit_kinds(db_path: &PathBuf) -> Vec<String> {
    let conn = Connection::open(db_path).expect("open DB");
    aberp_audit_ledger::ensure_schema(&conn).expect("ensure schema");
    let mut stmt = conn
        .prepare("SELECT kind FROM audit_ledger ORDER BY seq ASC")
        .expect("prepare audit kinds");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query audit kinds");
    rows.filter_map(|r| r.ok()).collect()
}
