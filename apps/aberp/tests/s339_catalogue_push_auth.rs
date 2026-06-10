//! S339 / PR-24 — catalogue-push auth contract pins.
//!
//! The storefront fronts its origin with CloudFront and enforces a
//! **dual gate** on `PUT /api/catalogue/materials`:
//!
//! 1. `hooks.server.ts` — every non-`/healthz` request must carry an
//!    `X-CloudFront-Secret` header matching `CLOUDFRONT_SHARED_SECRET`,
//!    else `403 "forbidden: missing origin signature"` (a static
//!    shared-secret compare, NOT an HMAC — verified S339 cross-repo).
//! 2. `requireAdminAuth` — `Authorization: Bearer <ABERP_SITE_ADMIN_TOKEN>`,
//!    else `401`.
//!
//! A hand-rolled mock storefront plays the origin and records every
//! request's method/path/headers so the tests can assert exactly what
//! ABERP sends. These pins guard:
//!
//! - `s339_catalogue_push_signs_request_with_origin_signature` — the
//!   `X-CloudFront-Secret` header is sent verbatim when the origin
//!   secret is provisioned.
//! - `s339_catalogue_push_omits_origin_header_when_unprovisioned` — when
//!   the secret is `None` (the common case) NO origin header is sent, so
//!   the change is additive / reversible (pre-S339 behaviour preserved).
//! - `s339_catalogue_push_uses_storefront_credential_handle_bearer` —
//!   the bearer is sourced from the shared `StorefrontCredentialHandle`
//!   (same source as the working email-outbox daemon).
//! - `s339_catalogue_push_returns_success_against_test_storefront` — a
//!   2xx from the mock yields `PushOutcome::Ok` and an `ok` audit row.
//! - `s339_catalogue_push_audit_records_http_400_when_storefront_rejects`
//!   — a 400 yields `UnexpectedStatus(400)` and an `unexpected_status`
//!   audit row (the prod symptom shape; 400 ≠ 401 ⇒ not `unauthorized`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aberp::catalogue_push::{
    CataloguePushDeps, CataloguePushHandle, CataloguePushService, PushOutcome,
};
use aberp::storefront_credential::StorefrontCredentialHandle;
use aberp_audit_ledger::{BinaryHash, EventKind, TenantId};
use duckdb::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

// ── Recorded request + mock state ────────────────────────────────────

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    /// Lower-cased header name → value, as received on the wire.
    headers: Vec<(String, String)>,
    #[allow(dead_code)] // captured for forensic-debug; not asserted on
    body: String,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        let lc = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == lc)
            .map(|(_, v)| v.as_str())
    }
}

struct MockState {
    requests: Mutex<Vec<RecordedRequest>>,
    response_status: u16,
    response_body: String,
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
                let mut headers = Vec::new();
                for h in lines {
                    if let Some((name, value)) = h.split_once(": ") {
                        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
                    }
                }
                {
                    let mut q = state.requests.lock().await;
                    q.push(RecordedRequest {
                        method,
                        path,
                        headers,
                        body,
                    });
                }
                let response = canned_response(state.response_status, &state.response_body);
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

fn canned_response(status: u16, body: &str) -> String {
    let phrase = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        _ => "OK",
    };
    format!(
        "HTTP/1.1 {status} {phrase}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len()
    )
}

// ── Harness ──────────────────────────────────────────────────────────

fn scratch_db_path(suffix: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-s339-catalogue-push-{pid}-{nanos}-{suffix}.duckdb"
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Build a service pointed at `addr` with the given bearer + optional
/// origin secret, run a single `push_once`, and return (outcome, the
/// mock's recorded PUT, the audit kinds+outcomes).
async fn run_push(
    suffix: &str,
    addr: SocketAddr,
    state: &Arc<MockState>,
    bearer: &str,
    origin_secret: Option<&str>,
) -> (PushOutcome, RecordedRequest, Vec<(String, String)>) {
    let db_path = scratch_db_path(suffix);
    {
        let conn = Connection::open(&db_path).expect("open scratch DB");
        aberp_audit_ledger::ensure_schema(&conn).expect("ensure audit schema");
    }

    let credential = StorefrontCredentialHandle::dormant();
    credential.set(format!("http://{addr}"), Zeroizing::new(bearer.to_string()));

    let handle = CataloguePushHandle::dormant();
    let deps = CataloguePushDeps {
        db_path: db_path.clone(),
        tenant: TenantId::new("test").expect("tenant id"),
        binary_hash: BinaryHash::from_bytes([0u8; 32]),
        operator_login: "test".to_string(),
        origin_secret: origin_secret.map(|s| Zeroizing::new(s.to_string())),
    };

    let service =
        CataloguePushService::new(handle.clone(), credential, deps).expect("build push service");
    let outcome = service.push_once("test").await;

    let put = {
        let reqs = state.requests.lock().await;
        reqs.iter()
            .find(|r| r.method == "PUT")
            .cloned()
            .expect("mock recorded a PUT request")
    };
    let audit = read_audit_kind_outcomes(&db_path);
    let _ = std::fs::remove_file(&db_path);
    (outcome, put, audit)
}

/// `(kind, outcome)` for every audit row, oldest first. `outcome` is the
/// `outcome` field decoded from the JSON payload (empty when absent).
fn read_audit_kind_outcomes(db_path: &PathBuf) -> Vec<(String, String)> {
    let conn = Connection::open(db_path).expect("open DB");
    aberp_audit_ledger::ensure_schema(&conn).expect("ensure schema");
    let mut stmt = conn
        .prepare("SELECT kind, payload FROM audit_ledger ORDER BY seq ASC")
        .expect("prepare audit read");
    let rows = stmt
        .query_map([], |r| {
            let kind: String = r.get(0)?;
            let payload: Vec<u8> = r.get(1)?;
            Ok((kind, payload))
        })
        .expect("query audit");
    rows.filter_map(|r| r.ok())
        .map(|(kind, payload)| {
            let outcome = serde_json::from_slice::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| v.get("outcome").and_then(|o| o.as_str()).map(String::from))
                .unwrap_or_default();
            (kind, outcome)
        })
        .collect()
}

const PUSHED_KIND: &str = "quote.material_catalogue_pushed";

// ── Tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn s339_catalogue_push_signs_request_with_origin_signature() {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        response_status: 200,
        response_body: r#"{"received_count":0}"#.to_string(),
    });
    let addr = spawn_mock(state.clone()).await;
    let (_outcome, put, _audit) = run_push(
        "origin-sig",
        addr,
        &state,
        "t0k3n",
        Some("super-origin-secret"),
    )
    .await;

    assert_eq!(
        put.header("x-cloudfront-secret"),
        Some("super-origin-secret"),
        "catalogue push must send the X-CloudFront-Secret header verbatim when provisioned; \
         headers were {:?}",
        put.headers
    );
    // And it must hit the catalogue path.
    assert_eq!(put.path, "/api/catalogue/materials");
}

#[tokio::test]
async fn s339_catalogue_push_omits_origin_header_when_unprovisioned() {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        response_status: 200,
        response_body: r#"{"received_count":0}"#.to_string(),
    });
    let addr = spawn_mock(state.clone()).await;
    let (_outcome, put, _audit) = run_push("no-origin", addr, &state, "t0k3n", None).await;

    assert!(
        put.header("x-cloudfront-secret").is_none(),
        "no origin secret provisioned ⇒ NO X-CloudFront-Secret header (pre-S339 behaviour); \
         headers were {:?}",
        put.headers
    );
}

#[tokio::test]
async fn s339_catalogue_push_uses_storefront_credential_handle_bearer() {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        response_status: 200,
        response_body: r#"{"received_count":0}"#.to_string(),
    });
    let addr = spawn_mock(state.clone()).await;
    let (_outcome, put, _audit) =
        run_push("bearer", addr, &state, "the-shared-handle-token", None).await;

    assert_eq!(
        put.header("authorization"),
        Some("Bearer the-shared-handle-token"),
        "bearer must come from the shared StorefrontCredentialHandle; headers were {:?}",
        put.headers
    );
}

#[tokio::test]
async fn s339_catalogue_push_returns_success_against_test_storefront() {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        response_status: 200,
        response_body: r#"{"received_count":0}"#.to_string(),
    });
    let addr = spawn_mock(state.clone()).await;
    let (outcome, _put, audit) =
        run_push("success", addr, &state, "t0k3n", Some("origin-secret")).await;

    assert!(
        matches!(outcome, PushOutcome::Ok { .. }),
        "2xx from the storefront must classify as Ok; got {outcome:?}"
    );
    assert_eq!(EventKind::MaterialCataloguePushed.as_str(), PUSHED_KIND);
    let pushed: Vec<&(String, String)> = audit.iter().filter(|(k, _)| k == PUSHED_KIND).collect();
    assert_eq!(
        pushed.len(),
        1,
        "exactly one MaterialCataloguePushed audit row; saw {audit:?}"
    );
    assert_eq!(
        pushed[0].1, "ok",
        "success cycle must record outcome=ok; saw {audit:?}"
    );
}

#[tokio::test]
async fn s339_catalogue_push_audit_records_http_400_when_storefront_rejects() {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        response_status: 400,
        response_body: r#"{"error":"materials[0]: grade is required"}"#.to_string(),
    });
    let addr = spawn_mock(state.clone()).await;
    let (outcome, _put, audit) = run_push("reject400", addr, &state, "t0k3n", None).await;

    assert_eq!(
        outcome,
        PushOutcome::UnexpectedStatus(400),
        "a 400 must classify as UnexpectedStatus(400) — NOT Unauthorized (that is reserved \
         for 401); got {outcome:?}"
    );
    let pushed: Vec<&(String, String)> = audit.iter().filter(|(k, _)| k == PUSHED_KIND).collect();
    assert_eq!(
        pushed.len(),
        1,
        "the failure path must still write an audit row; saw {audit:?}"
    );
    assert_eq!(
        pushed[0].1, "unexpected_status",
        "a 400 rejection must record outcome=unexpected_status (the prod symptom shape); \
         saw {audit:?}"
    );
}
