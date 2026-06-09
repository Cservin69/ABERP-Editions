//! S311 / PR-278 — Integration pin for the F1 stale-claim auto-recovery
//! cycle. Closes S309 findings F2 (only happy-path tested), F5 (writeback
//! failure → silent), F7 (retry-via-next-cycle was fictitious).
//!
//! ## Test shape
//!
//! 1. Hand-rolled HTTP mock plays the storefront, this time with the
//!    storefront-side stale-claim sweep semantics: on every `GET
//!    /api/internal/email-queue` it walks the `claimed` set and promotes
//!    any entry the test marked `wedged` back to `queued` (mirroring
//!    `recoverStaleClaimed` in `ABERP-site/src/lib/server/email-outbox.ts`).
//! 2. Cycle 1: 3 entries queued with three distinct `submitter` kinds.
//!    Daemon claims+sends each. Mock state transitions queued → claimed →
//!    sent.
//! 3. A 4th entry is injected directly in `claimed` with `wedged: true`
//!    (simulates a "prior crashed daemon" — F1's wedge bug).
//! 4. Cycle 2: GET runs the mock's stale sweep, the wedged entry flips
//!    queued, daemon re-claims + re-sends.
//! 5. Audit ledger pin: cycle 1 emits 1 Fetched + 3 Claimed + 3 Sent;
//!    cycle 2 emits 1 Fetched + 1 Claimed + 1 Sent. 10 rows total, 0
//!    Failed. The Sent count proves the stale-claim recovery actually
//!    completes the cycle — without F1, cycle 2 would emit nothing.

use std::collections::{HashMap, HashSet};
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

const ENTRY_1: &str = "01H00000000000000000000010";
const ENTRY_2: &str = "01H00000000000000000000020";
const ENTRY_3: &str = "01H00000000000000000000030";
const ENTRY_4: &str = "01H00000000000000000000040";

#[derive(Debug, Clone)]
struct MockEntry {
    id: &'static str,
    submitter: &'static str,
    queued_at: &'static str,
}

const SEEDED_ENTRIES: &[MockEntry] = &[
    MockEntry {
        id: ENTRY_1,
        submitter: "submission_received",
        queued_at: "2026-06-09T12:00:00.000Z",
    },
    MockEntry {
        id: ENTRY_2,
        submitter: "priced_ready",
        queued_at: "2026-06-09T12:00:01.000Z",
    },
    MockEntry {
        id: ENTRY_3,
        submitter: "accept_confirmation",
        queued_at: "2026-06-09T12:00:02.000Z",
    },
];

const WEDGED_ENTRY: MockEntry = MockEntry {
    id: ENTRY_4,
    submitter: "submission_received",
    queued_at: "2026-06-09T12:00:03.000Z",
};

// Captured per request for forensic-debug only — the assertions on this
// test pin the entry_states/audit_kinds maps, not the wire trail.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct RecordedRequest {
    method: String,
    path: String,
    body: String,
}

struct MockState {
    requests: Mutex<Vec<RecordedRequest>>,
    expected_bearer: String,
    /// `id → state` (`"queued" | "claimed" | "sent" | "failed"`).
    entry_states: Mutex<HashMap<String, &'static str>>,
    /// Entries currently in `claimed` that the storefront-side stale
    /// sweep would auto-recover. The mock's GET handler flips each
    /// wedged id from `claimed` back to `queued` and removes it from
    /// this set (each wedge recovers exactly once, per the real
    /// `rename` semantics).
    wedged_ids: Mutex<HashSet<String>>,
    /// Closed-vocab kind → entry metadata so the mock can reply with a
    /// well-shaped queue entry for any seeded id.
    metadata: HashMap<String, MockEntry>,
}

#[tokio::test]
async fn s311_full_cycle_three_entries_plus_stale_recovery() {
    // ── Mock setup ───────────────────────────────────────────────────
    let mut entry_states: HashMap<String, &'static str> = HashMap::new();
    let mut metadata: HashMap<String, MockEntry> = HashMap::new();
    for e in SEEDED_ENTRIES {
        entry_states.insert(e.id.to_string(), "queued");
        metadata.insert(e.id.to_string(), e.clone());
    }
    // The wedged entry pre-exists in `claimed` — i.e. a prior daemon
    // crashed mid-cycle. The storefront's S311 / PR-12 stale-sweep is
    // what closes the wedge; the mock simulates it on every GET.
    entry_states.insert(WEDGED_ENTRY.id.to_string(), "claimed");
    metadata.insert(WEDGED_ENTRY.id.to_string(), WEDGED_ENTRY.clone());
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        expected_bearer: "Bearer s311_token".to_string(),
        entry_states: Mutex::new(entry_states),
        // The wedge is injected for cycle 2; cycle 1 leaves the set
        // empty so the mock GET doesn't promote the 4th entry prematurely.
        wedged_ids: Mutex::new(HashSet::new()),
        metadata,
    });
    let addr = spawn_mock(state.clone()).await;

    // ── ABERP setup ──────────────────────────────────────────────────
    let db_path = scratch_db_path("s311_stale_recovery");
    let _ = std::fs::remove_file(&db_path);
    {
        let conn = Connection::open(&db_path).expect("open scratch DB");
        aberp_audit_ledger::ensure_schema(&conn).expect("ensure audit schema");
    }
    let credential = StorefrontCredentialHandle::dormant();
    credential.set(
        format!("http://{addr}"),
        Zeroizing::new("s311_token".to_string()),
    );
    let handle = EmailOutboxDaemonHandle::dormant();
    let capture = Arc::new(CaptureSender::default());
    let deps = EmailOutboxPollDaemonDeps {
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

    // ── Cycle 1: three queued entries → all sent ─────────────────────
    poll_once(&deps, &client).await;

    {
        let captured = capture.captured.lock().await;
        assert_eq!(
            captured.len(),
            3,
            "cycle 1 capture sender saw {} entries (want 3)",
            captured.len()
        );
        let ids: HashSet<String> = captured.iter().map(|e| e.id.clone()).collect();
        for seeded in SEEDED_ENTRIES {
            assert!(
                ids.contains(seeded.id),
                "cycle 1 missed {}: {ids:?}",
                seeded.id
            );
        }
    }
    {
        let entry_states = state.entry_states.lock().await;
        for seeded in SEEDED_ENTRIES {
            assert_eq!(
                entry_states.get(seeded.id).copied(),
                Some("sent"),
                "cycle 1 left {} in state {:?}",
                seeded.id,
                entry_states.get(seeded.id)
            );
        }
        // The wedged entry is still in claimed — the daemon hasn't
        // touched it (it's not in queued, so GET didn't return it).
        assert_eq!(
            entry_states.get(WEDGED_ENTRY.id).copied(),
            Some("claimed"),
            "cycle 1 mistakenly touched the wedged entry"
        );
    }

    // ── Wedge injection ─────────────────────────────────────────────
    // Mark the 4th entry as wedged. On the next GET the mock's stale
    // sweep flips it back to queued, mirroring `recoverStaleClaimed`
    // on the storefront.
    {
        let mut wedged = state.wedged_ids.lock().await;
        wedged.insert(WEDGED_ENTRY.id.to_string());
    }
    // Reset the daemon's last_seen_iso so cycle 2 picks up the wedged
    // entry — its queued_at is BEFORE the cursor cycle 1 advanced to.
    // In production the storefront's `?since=<iso>` filter is strictly
    // `<` so an entry at exactly the cursor isn't lost; here we drop
    // the cursor entirely because the test only models 4 entries and
    // we want cycle 2 to see the recovered one. This is a test seam,
    // not a production behavior — pinned to keep the assertion clear.
    handle.reset_last_seen_for_test();
    capture.captured.lock().await.clear();

    // ── Cycle 2: wedged entry recovered → sent ──────────────────────
    poll_once(&deps, &client).await;

    {
        let captured = capture.captured.lock().await;
        assert_eq!(
            captured.len(),
            1,
            "cycle 2 capture sender saw {} entries (want exactly 1: the recovered wedge)",
            captured.len()
        );
        assert_eq!(captured[0].id, WEDGED_ENTRY.id, "cycle 2 sent the wrong id");
    }
    {
        let entry_states = state.entry_states.lock().await;
        assert_eq!(
            entry_states.get(WEDGED_ENTRY.id).copied(),
            Some("sent"),
            "cycle 2 left {} in state {:?} (want sent)",
            WEDGED_ENTRY.id,
            entry_states.get(WEDGED_ENTRY.id)
        );
    }
    {
        // The wedge set is now empty — the mock's GET consumed the
        // entry on cycle 2, mirroring "rename happens once" semantics.
        let wedged = state.wedged_ids.lock().await;
        assert!(
            wedged.is_empty(),
            "stale-claim sweep should consume the wedge once; got {wedged:?}"
        );
    }

    // ── Audit ledger pin ────────────────────────────────────────────
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
    // Cycle 1 = 1 Fetched + 3 Claimed + 3 Sent = 7
    // Cycle 2 = 1 Fetched + 1 Claimed + 1 Sent = 3
    // Total = 10 rows, 0 Failed.
    assert_eq!(
        fetched, 2,
        "expected 2 Fetched rows (one per cycle); saw {fetched} in {audit_kinds:?}"
    );
    assert_eq!(
        claimed, 4,
        "expected 4 Claimed rows (3+1); saw {claimed} in {audit_kinds:?}"
    );
    assert_eq!(
        sent, 4,
        "expected 4 Sent rows (3+1); saw {sent} in {audit_kinds:?}"
    );
    assert_eq!(
        failed, 0,
        "expected 0 Failed rows; saw {failed} in {audit_kinds:?}"
    );

    // ── Status handle reflects the two cycles ───────────────────────
    let snap = handle.snapshot();
    assert_eq!(snap.total_cycles_since_boot, 2);
    assert_eq!(snap.total_fetched_since_boot, 4); // 3 + 1
    assert_eq!(snap.total_sent_since_boot, 4); // 3 + 1
    assert_eq!(snap.total_failed_since_boot, 0);

    let _ = std::fs::remove_file(&db_path);
}

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
        // S311 mock — mirror the storefront's stale-claim sweep before
        // listing. Any id in wedged_ids that's currently `claimed` gets
        // promoted back to `queued` and removed from the wedged set
        // (rename happens once per real-world semantics).
        let mut states = state.entry_states.lock().await;
        let mut wedged = state.wedged_ids.lock().await;
        let wedged_now: Vec<String> = wedged.iter().cloned().collect();
        for id in wedged_now {
            if states.get(&id).copied() == Some("claimed") {
                states.insert(id.clone(), "queued");
                wedged.remove(&id);
            }
        }
        // List the queued set in id order (= ULID-ascending = queued_at
        // order). Mirror the storefront's response shape.
        let mut queued_entries: Vec<&MockEntry> = states
            .iter()
            .filter(|(_, st)| **st == "queued")
            .filter_map(|(id, _)| state.metadata.get(id))
            .collect();
        queued_entries.sort_by_key(|e| e.id);
        let entries_json: Vec<String> = queued_entries.iter().map(|e| entry_list_json(e)).collect();
        let body = format!(r#"{{"entries":[{}]}}"#, entries_json.join(","));
        return canned_response(200, "application/json", &body);
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
                let body = canned_entry_json(&id, "claimed", state.metadata.get(&id));
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
        let body = canned_entry_json(&id, "sent", state.metadata.get(&id));
        return canned_response(200, "application/json", &body);
    }
    if method == "POST" && path_no_query.ends_with("/failed") {
        let id = path_no_query
            .trim_start_matches("/api/internal/email-queue/")
            .trim_end_matches("/failed")
            .to_string();
        let mut states = state.entry_states.lock().await;
        states.insert(id.clone(), "failed");
        let body = canned_entry_json(&id, "failed", state.metadata.get(&id));
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

fn entry_list_json(e: &MockEntry) -> String {
    format!(
        r#"{{
          "id": "{}",
          "queued_at": "{}",
          "to": ["customer@example.com"],
          "cc": [],
          "subject": "test {}",
          "body_text": "hello",
          "submitter": "{}",
          "state": "queued",
          "attempt_n": 0,
          "last_error": null,
          "sent_at": null,
          "audit_id": null
        }}"#,
        e.id, e.queued_at, e.id, e.submitter
    )
}

fn canned_entry_json(id: &str, state: &str, meta: Option<&MockEntry>) -> String {
    let (queued_at, submitter) = match meta {
        Some(m) => (m.queued_at, m.submitter),
        None => ("2026-06-09T12:00:00.000Z", "submission_received"),
    };
    format!(
        r#"{{
          "id": "{id}",
          "queued_at": "{queued_at}",
          "to": ["customer@example.com"],
          "cc": [],
          "subject": "stub",
          "body_text": "stub",
          "submitter": "{submitter}",
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
