//! End-to-end pin: hand-rolled HTTP mock server returns canned
//! approved-quote JSON; daemon's `poll_once` ingests it, writes one
//! `quote_intake_log` row, and POSTs status writeback back to the mock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_billing::Currency;
use aberp_quote_intake::{
    audit::PollTrigger,
    service::{PollSummary, QuoteIntakeDeps},
    QuoteIntakeConfig, QuoteIntakeService,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const LIST_BODY_TWO_QUOTES: &str = r#"{
  "quotes": [
    {
      "id": "00000000-0000-0000-0000-000000000001",
      "received_at": "2026-05-30T10:00:00Z",
      "contact": {
        "name": "Ada Lovelace",
        "email": "ada@example.com",
        "company": "Babbage Engines"
      },
      "request": {
        "material_preference": "aluminum 6061",
        "quantity": 4,
        "deadline": "2026-07-01",
        "notes": "rush"
      },
      "files": [{"filename": "bracket.step", "size_bytes": 2048, "stored_at": "2026-05-30T10:00:01Z"}],
      "status": "approved",
      "consent_at": "2026-05-30T09:59:30Z"
    },
    {
      "id": "00000000-0000-0000-0000-000000000002",
      "received_at": "2026-05-30T11:00:00Z",
      "contact": {
        "name": "Grace Hopper",
        "email": "grace@example.com",
        "company": ""
      },
      "request": {
        "material_preference": "steel",
        "quantity": null,
        "deadline": null,
        "notes": ""
      },
      "files": [],
      "status": "approved",
      "consent_at": "2026-05-30T10:59:30Z"
    }
  ]
}"#;

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

struct MockState {
    requests: Mutex<Vec<RecordedRequest>>,
    expected_bearer: String,
    writeback_fail_counters: Mutex<HashMap<String, u32>>,
}

#[tokio::test]
async fn happy_path_round_trip() {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        expected_bearer: "Bearer t0k3n".to_string(),
        writeback_fail_counters: Mutex::new(HashMap::new()),
    });
    let addr = spawn_mock(state.clone(), MockBehaviour::Normal).await;

    let db_path = scratch_db_path("happy_path_round_trip");
    let service = build_service(&addr, "t0k3n", &db_path);

    let summary = service.poll_once(PollTrigger::Daemon).await;

    assert_eq!(summary.fetched, 2, "summary: {summary:?}");
    assert_eq!(summary.created, 2);
    assert_eq!(summary.skipped_duplicate, 0);
    assert_eq!(summary.writeback_failed, 0);
    assert_eq!(summary.failed.len(), 0);
    assert!(summary.error.is_none());

    let summary2 = service.poll_once(PollTrigger::Daemon).await;
    assert_eq!(summary2.fetched, 2);
    assert_eq!(summary2.created, 0);
    assert_eq!(summary2.skipped_duplicate, 2);
    assert_eq!(summary2.failed.len(), 0);

    let reqs = state.requests.lock().await.clone();
    assert!(reqs.iter().any(|r| r.path == "/api/quotes?status=approved"));
    assert!(reqs.iter().filter(|r| r.method == "POST").count() >= 2);
    for r in &reqs {
        assert_eq!(
            r.authorization.as_deref(),
            Some("Bearer t0k3n"),
            "request without bearer: {r:?}"
        );
    }
    let writebacks: Vec<_> = reqs.iter().filter(|r| r.method == "POST").collect();
    assert!(!writebacks.is_empty());
    for w in &writebacks {
        assert!(w.body.contains("\"status\":\"invoiced\""), "{:?}", w.body);
        assert!(w.body.contains("inv_"), "{:?}", w.body);
    }
}

#[tokio::test]
async fn unauthorized_aborts_cycle() {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        expected_bearer: "Bearer correct".to_string(),
        writeback_fail_counters: Mutex::new(HashMap::new()),
    });
    let addr = spawn_mock(state.clone(), MockBehaviour::Normal).await;

    let db_path = scratch_db_path("unauthorized_aborts_cycle");
    let service = build_service(&addr, "WRONG-TOKEN", &db_path);

    let summary = service.poll_once(PollTrigger::Daemon).await;
    assert_eq!(summary.fetched, 0);
    assert_eq!(summary.created, 0);
    assert!(summary.error.is_some(), "summary: {summary:?}");
    assert!(
        summary.error.as_deref().unwrap_or_default().contains("401")
            || summary
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("unauthorized"),
        "unexpected error: {:?}",
        summary.error
    );
}

#[tokio::test]
async fn writeback_failure_leaves_row_pending_and_retries_next_cycle() {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        expected_bearer: "Bearer t".to_string(),
        writeback_fail_counters: Mutex::new(HashMap::new()),
    });
    {
        let mut c = state.writeback_fail_counters.lock().await;
        c.insert("00000000-0000-0000-0000-000000000001".to_string(), 1);
        c.insert("00000000-0000-0000-0000-000000000002".to_string(), 1);
    }
    let addr = spawn_mock(state.clone(), MockBehaviour::FailFirstWriteback).await;

    let db_path = scratch_db_path("writeback_failure_then_retry");
    let service = build_service(&addr, "t", &db_path);

    let s1 = service.poll_once(PollTrigger::Daemon).await;
    assert_eq!(s1.created, 2, "cycle 1: {s1:?}");
    assert_eq!(s1.writeback_failed, 2);

    let s2 = service.poll_once(PollTrigger::Daemon).await;
    assert_eq!(s2.skipped_duplicate, 2, "cycle 2: {s2:?}");
    assert_eq!(s2.writeback_retried, 2);
    assert_eq!(s2.writeback_failed, 0);
}

fn build_service(addr: &SocketAddr, token: &str, db_path: &Path) -> QuoteIntakeService {
    let cfg = QuoteIntakeConfig {
        base_url: format!("http://{addr}"),
        bearer_token: zeroize::Zeroizing::new(token.to_string()),
        poll_interval: std::time::Duration::from_secs(60),
        enabled: true,
    };
    let deps = QuoteIntakeDeps {
        db_path: db_path.to_path_buf(),
        tenant: TenantId::new("t1".to_string()).expect("tenant"),
        binary_hash: BinaryHash::from_bytes([0u8; 32]),
        operator_login: "test".to_string(),
        default_currency: Currency::Huf,
    };
    QuoteIntakeService::new(cfg, deps).expect("service")
}

fn scratch_db_path(suffix: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    p.push(format!(
        "aberp-quote-intake-test-{pid}-{nanos}-{suffix}.duckdb"
    ));
    let _ = std::fs::remove_file(&p);
    p
}

#[derive(Debug, Clone, Copy)]
enum MockBehaviour {
    Normal,
    FailFirstWriteback,
}

async fn spawn_mock(state: Arc<MockState>, behaviour: MockBehaviour) -> SocketAddr {
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
                let mut buf = vec![0u8; 16 * 1024];
                let n = match sock.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut authorization = None;
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
                        authorization: authorization.clone(),
                        body: body.to_string(),
                    });
                }
                let response =
                    handle_request(&state, behaviour, &method, &path, authorization.as_deref())
                        .await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

async fn handle_request(
    state: &Arc<MockState>,
    behaviour: MockBehaviour,
    method: &str,
    path: &str,
    authorization: Option<&str>,
) -> String {
    if authorization != Some(state.expected_bearer.as_str()) {
        return canned_response(401, "application/json", r#"{"error":"unauthorized"}"#);
    }
    if method == "GET" && path.starts_with("/api/quotes?") {
        return canned_response(200, "application/json", LIST_BODY_TWO_QUOTES);
    }
    if method == "POST" && path.starts_with("/api/quotes/") && path.ends_with("/status") {
        let id = path
            .trim_start_matches("/api/quotes/")
            .trim_end_matches("/status")
            .to_string();
        if matches!(behaviour, MockBehaviour::FailFirstWriteback) {
            let mut counters = state.writeback_fail_counters.lock().await;
            if let Some(c) = counters.get_mut(&id) {
                if *c > 0 {
                    *c -= 1;
                    return canned_response(502, "application/json", r#"{"error":"flaky"}"#);
                }
            }
        }
        return canned_response(200, "application/json", r#"{"status":"invoiced"}"#);
    }
    canned_response(404, "application/json", r#"{"error":"not found"}"#)
}

fn canned_response(status: u16, ct: &str, body: &str) -> String {
    let phrase = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "OK",
    };
    format!(
        "HTTP/1.1 {status} {phrase}\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len()
    )
}

#[allow(dead_code)]
fn _ty_sanity(_: &PollSummary) {}
