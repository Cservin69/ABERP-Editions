//! Portable-line end-to-end boot smoke (S435).
//!
//! The Portable launcher pair (`run/run_portable.sh` +
//! `run/upgrade_portable.sh`) builds a DEV-profile binary and boots it as
//! `tenant=demo`. The promise that pair makes — and that this file pins —
//! is the [[customer-journey-e2e-gate]]: the NAV-off demo tenant boots
//! straight to `Ready` (NOT the first-run `NeedsSetup` wizard), with no
//! NAV credentials present, serves its routes, and the real partner-create
//! route succeeds in that tenant.
//!
//! Three pins, two always-on (keychain-free) + one env-gated full boot:
//!
//! 1. `portable_demo_health_route_answers_200_in_process` — serves the
//!    REAL route table (`serve::build_router`) over plain HTTP against a
//!    demo, `nav_enabled=false`, `Ready` `AppState`, and asserts
//!    `GET /health` → `200`, `ok:true`, `is_production_build:false` (the
//!    dev-profile the launcher builds). In-process so it carries none of
//!    the OS-keychain dependency a full `aberp serve` subprocess does, and
//!    runs in every `cargo test` gate.
//!
//! 2. `demo_tenant_create_partner_via_real_route_succeeds` — exercises the
//!    exact helper `POST /api/partners` calls (`create_partner_request`)
//!    against the demo `AppState`, proving the "create a partner in the
//!    demo" leg returns a server-minted `prt_<ULID>`. Mirrors the
//!    `serve_partners_route.rs` idiom. Always-on.
//!
//! 3. `portable_demo_subprocess_boots_ready_over_tls` — the full-fidelity
//!    e2e: spawns the compiled `aberp serve --tenant demo` binary, reads
//!    its `READY 127.0.0.1:<port> sha256:<hex> state=<token>` handshake,
//!    asserts `state=ready` (NAV-off skipped the keychain + §169 gate),
//!    and GETs `/health` over the real loopback TLS for a `200`.
//!    ENV-GATED behind `ABERP_PORTABLE_E2E=1`: the boot path reads the
//!    SPA session token from the OS keychain (always — it is the Bearer
//!    secret, independent of NAV), so this needs a real login keychain
//!    and cannot run under a synthesised `HOME`. Same env-gating posture
//!    as `serve_boot_budget_live.rs` and the other `*_live` tests.

// S435 / ADR-0093 — this entire module is the PORTABLE-line boot smoke:
// every pin asserts the Portable/DEV posture (`is_production_build:false`,
// NAV-off demo boots to `Ready`). The Defense (`--features production`)
// build inverts `is_production_build`, so these assertions are false BY
// CONSTRUCTION there — scope the whole module out of the Defense arm. The
// assertions stay correct and ALWAYS run in the Portable (default) build.
// Defense keeps its own coverage: partner-create via serve_partners_route.rs,
// /health via serve_smoke.rs, edition/boot guards via edition_db_isolation.rs.
#![cfg(not(feature = "production"))]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aberp_audit_ledger::{BinaryHash, TenantId};
use ulid::Ulid;

use aberp::nav_xml::CustomerVatStatus;
use aberp::partners::{CustomerType, PartnerInputs, PartnerKind};
use aberp::serve::{self, AppState};

// ──────────────────────────────────────────────────────────────────────
// Shared: a demo-tenant, NAV-off, Ready AppState (the Portable posture)
// ──────────────────────────────────────────────────────────────────────

fn demo_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new("demo".to_string()).expect("demo tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    AppState {
        db: aberp::serve::open_tenant_handle(&db_path, tenant.clone())
            .expect("open shared test DuckDB handle (ADR-0098 Gap 1a)"),
        db_path: Arc::new(db_path),
        tenant,
        // The whole point of the Portable line: NAV submission is off.
        nav_enabled: false,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        nav_poll_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
            aberp::serve::NAV_POLL_DAEMON_CONCURRENCY,
        )),
        boot_state: Arc::new(std::sync::RwLock::new(serve::ServeBootState::Ready {
            operator_login: serve::NAV_DISABLED_LOGIN.to_string(),
        })),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        adapter_registry: Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
        adapter_manager: Arc::new(aberp::mes_manager::AdapterManager::new(
            Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
            tokio_util::sync::CancellationToken::new(),
        )),
        adapter_health_baseline: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        restore_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        catalogue_push: aberp::catalogue_push::CataloguePushHandle::dormant(),
        email_relay_rate_limiter: std::sync::Arc::new(aberp::email_relay::RateLimiter::new()),
        pipeline_python_resolution: aberp::quote_pricing_pipeline::PythonResolutionHandle::dormant(
        ),
        storefront_credential: aberp::storefront_credential::StorefrontCredentialHandle::dormant(),
        email_outbox_daemon: aberp::email_outbox_poll_daemon::EmailOutboxDaemonHandle::dormant(),
        quote_pdf_rerender_queue: aberp::quote_pdf_rerender_queue::QuotePdfRerenderQueue::new(),
        digital_id: std::sync::Arc::new(aberp_digital_id::MockProvider::new()),
    }
}

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aberp-portable-{label}-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

// ──────────────────────────────────────────────────────────────────────
// Pin 1 — in-process /health over the REAL router → 200 (always-on)
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn portable_demo_health_route_answers_200_in_process() {
    let dir = test_dir("health");
    let state = demo_state(dir.join("aberp.duckdb"));
    let app = serve::build_router(state);

    // Serve the real route table over plain HTTP on an ephemeral loopback
    // port (no TLS, no keychain) — we are exercising the handler, not the
    // transport.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().expect("resolve bound addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("GET /health");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "/health must answer 200 on a Ready demo tenant",
    );
    let body: serde_json::Value = resp.json().await.expect("/health returns JSON");
    assert_eq!(
        body["ok"],
        serde_json::json!(true),
        "/health ok must be true"
    );
    assert_eq!(
        body["is_production_build"],
        serde_json::json!(false),
        "Portable serves a DEV-profile binary — is_production_build must be false",
    );

    server.abort();
    let _keep = &dir;
}

// ──────────────────────────────────────────────────────────────────────
// Pin 2 — create a partner via the real route helper, in the demo tenant
// ──────────────────────────────────────────────────────────────────────

/// The "create a partner in the demo" leg of the customer journey: the
/// exact helper the `POST /api/partners` handler calls must succeed and
/// mint a `prt_<ULID>` id. A demo-tenant partner uses `PrivatePerson` VAT
/// status — the NAV-off sandbox has no §169 ADÓSZÁM requirement.
#[test]
fn demo_tenant_create_partner_via_real_route_succeeds() {
    let dir = test_dir("partner");
    let state = demo_state(dir.join("aberp.duckdb"));

    let inputs = PartnerInputs {
        display_name: "Acme Manufacturing".to_string(),
        legal_name: "Acme Manufacturing Ltd".to_string(),
        kind: PartnerKind::Customer,
        customer_vat_status: CustomerVatStatus::PrivatePerson,
        customer_type: CustomerType::Unset,
        tax_number: None,
        eu_vat_number: None,
        address_street: Some("1 Industrial Way".to_string()),
        address_postal_code: Some("2000".to_string()),
        address_city: Some("Johannesburg".to_string()),
        address_country: Some("ZA".to_string()),
        bank_account: None,
        contact_email: Some("ops@acme.example".to_string()),
        contact_phone: None,
    };

    let partner = serve::create_partner_request(&state, &inputs)
        .expect("create partner in demo tenant must succeed");

    assert!(
        partner.id.starts_with("prt_"),
        "partner id `{}` must be a server-minted prefixed ULID",
        partner.id,
    );
    assert_eq!(partner.display_name, "Acme Manufacturing");

    // And it round-trips through the list route helper.
    let listed = serve::list_partners_request(&state, None).expect("list demo partners");
    assert_eq!(listed.len(), 1, "the created partner must be listable");
    assert_eq!(listed[0].id, partner.id);

    let _keep = &dir;
}

// ──────────────────────────────────────────────────────────────────────
// Pin 3 — full subprocess boot over real TLS (env-gated; needs keychain)
// ──────────────────────────────────────────────────────────────────────

/// Kill-on-drop guard so a failed assertion never strands a bound listener.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn portable_demo_subprocess_boots_ready_over_tls() {
    if std::env::var("ABERP_PORTABLE_E2E").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping portable_demo_subprocess_boots_ready_over_tls: ABERP_PORTABLE_E2E not set \
             (the boot path reads the session token from the OS keychain — needs a real login \
             keychain, so this opt-in test runs under the real HOME)"
        );
        return;
    }

    // Real HOME so the OS keychain resolves (the session token is read
    // there regardless of NAV mode). The demo tenant is the product's
    // bundled tenant; we point --db at a throwaway path so we never touch
    // real demo data.
    let db_path = test_dir("subproc").join("aberp.duckdb");
    let aberp_bin = env!("CARGO_BIN_EXE_aberp");

    let child = Command::new(aberp_bin)
        .arg("serve")
        .arg("--tenant")
        .arg("demo")
        .arg("--db")
        .arg(&db_path)
        .arg("--port")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn `aberp serve --tenant demo` subprocess");
    let mut guard = ChildGuard(child);

    let stdout = guard.0.stdout.take().expect("subprocess stdout pipe");
    let mut reader = BufReader::new(stdout);

    let started = Instant::now();
    let mut ready_line: Option<String> = None;
    let mut line_buf = String::new();
    while ready_line.is_none() {
        line_buf.clear();
        let n = reader
            .read_line(&mut line_buf)
            .expect("read subprocess stdout");
        if n == 0 {
            break; // EOF — backend died before READY
        }
        let trimmed = line_buf.trim();
        if trimmed.starts_with("READY ") {
            ready_line = Some(trimmed.to_string());
        }
        if started.elapsed() > Duration::from_secs(60) {
            break;
        }
    }

    let ready_line = ready_line
        .expect("demo tenant must emit a `READY 127.0.0.1:<port> sha256:<hex> state=ready` line");

    // The load-bearing assertion: NAV-off demo reached Ready, not the
    // NeedsSetup wizard (which would mean the keychain/§169 gate fired).
    assert!(
        ready_line.contains("state=ready"),
        "demo must boot to state=ready (NAV-off skips keychain + §169 gate); got `{ready_line}`",
    );

    let addr = ready_line
        .strip_prefix("READY ")
        .and_then(|rest| rest.split_whitespace().next())
        .expect("READY line carries a 127.0.0.1:<port> address");

    // Real loopback TLS (self-signed cert → accept invalid).
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build reqwest client");
    let resp = client
        .get(format!("https://{addr}/health"))
        .send()
        .await
        .expect("GET /health over loopback TLS");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "/health must answer 200 once the demo tenant is Ready",
    );
    let body: serde_json::Value = resp.json().await.expect("/health JSON");
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["is_production_build"], serde_json::json!(false));
    // guard drops here → subprocess killed.
}
