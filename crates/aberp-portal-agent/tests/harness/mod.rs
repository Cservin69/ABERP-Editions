//! The loopback harness: a stub ABERP, a real relay, a real agent, and
//! a virtual authenticator.
//!
//! Everything runs in one process on `127.0.0.1` with no VPS, no DNS
//! and no public certificate, but the *code under test* is the shipped
//! code: the real `aberp-portal-relay` front and broker, the real
//! agent, the real Leg-B mTLS handshake with both peers pinned, and the
//! real WebAuthn verification path.
//!
//! Three things are stand-ins, named here rather than glossed:
//!
//! 1. **ABERP itself** is a stub that speaks the contract of the four
//!    `serve.rs` read routes (bearer-gated, self-signed loopback TLS).
//!    Booting the real binary would require DuckDB, a tenant, NAV
//!    credentials and the macOS keychain. `tests/route_drift.rs` is
//!    what keeps the stub's contract tied to the real one.
//! 2. **The authenticator** is a software P-256 key rather than a
//!    Secure Enclave. It produces byte-identical ceremony material to a
//!    real platform authenticator, which is what the relying party
//!    actually verifies; what it cannot reproduce is the biometric
//!    gate, which is the OS's job and not this code's.
//! 3. **Leg A is plaintext HTTP** on loopback, because there is no
//!    wildcard certificate here. The cookie's `Secure` attribute is
//!    therefore dropped for the test only (`cookie_secure: false`),
//!    which `config::AgentConfig::from_env` makes opt-out.

// This module is compiled into every integration-test binary, and each
// one uses a different slice of it. Without this, the slice a given
// binary does not use is a `dead_code` warning — and the workspace
// gate is `clippy --all-targets -D warnings`.
#![allow(dead_code)]

pub mod authenticator;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aberp_portal_agent::alert::AlertSink;
use aberp_portal_agent::config::{AgentConfig, SecretSource, UpstreamConfig, UpstreamDiscovery};
use aberp_portal_agent::{tunnel, Agent};
use aberp_portal_core::PinnedFingerprint;
use aberp_portal_relay::{canary as relay_canary, front, Broker, Canary, Front};
use axum::extract::Path as AxumPath;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Json;
use sha2::{Digest, Sha256};

pub const UPSTREAM_BEARER: &str = "test-session-token-not-a-real-secret";
pub const INVOICE_ID: &str = "INV-2026-0001";
pub const PDF_MAGIC: &[u8] = b"%PDF-1.7 stub";
/// The decoy the harness plants. Deliberately not the compiled-in
/// default, so the tests prove the agent's value is what reaches the
/// front rather than both sides happening to agree on a constant.
pub const TRIPWIRE_PATH: &str = "/backup/site-config.old";

/// A self-signed loopback identity plus the SHA-256 its peer pins.
pub struct Identity {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint_hex: String,
}

pub fn identity(name: &str) -> Identity {
    let cert = rcgen::generate_simple_self_signed(vec![name.to_string()])
        .expect("rcgen self-signed identity");
    let der = cert.cert.der().to_vec();
    Identity {
        cert_pem: cert.cert.pem(),
        key_pem: cert.key_pair.serialize_pem(),
        fingerprint_hex: hex::encode(Sha256::digest(&der)),
    }
}

pub fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).expect("write harness file");
    p
}

// -- the stub ABERP ----------------------------------------------------

/// Counts every request the stub saw on a MUTATING route. It must stay
/// at zero: ADR-0113 §G5 says Phase 1 "structurally cannot write", and
/// the way to check that is to leave live mutating routes sitting next
/// to the read ones and prove nothing ever reaches them.
#[derive(Debug, Default)]
pub struct StubCounters {
    pub mutating_hits: AtomicU64,
    pub reads: AtomicU64,
}

pub struct StubAberp {
    pub base_url: String,
    pub fingerprint_hex: String,
    pub counters: Arc<StubCounters>,
    handle: axum_server::Handle,
}

impl StubAberp {
    /// Stop the stub — the "operator stopped `aberp serve`" event.
    pub fn stop(&self) {
        self.handle.shutdown();
    }
}

fn bearer_ok(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {UPSTREAM_BEARER}"))
}

pub async fn start_stub_aberp() -> StubAberp {
    let id = identity("localhost");
    let counters = Arc::new(StubCounters::default());
    let c = Arc::clone(&counters);

    let app = axum::Router::new()
        // `serve.rs:4271` -- the one deliberately unauthenticated route.
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({ "ok": true, "nav_xsd_version": "3.0" })) }),
        )
        .route(
            "/invoices",
            get({
                let c = Arc::clone(&c);
                move |headers: HeaderMap| {
                    let c = Arc::clone(&c);
                    async move {
                        if !bearer_ok(&headers) {
                            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!([])));
                        }
                        c.reads.fetch_add(1, Ordering::Relaxed);
                        (
                            StatusCode::OK,
                            Json(serde_json::json!([{
                                "id": INVOICE_ID,
                                "invoice_number": INVOICE_ID,
                                "state": "Issued",
                                "total": "125000"
                            }])),
                        )
                    }
                }
            }),
        )
        .route(
            "/invoices/:id",
            get({
                let c = Arc::clone(&c);
                move |headers: HeaderMap, AxumPath(id): AxumPath<String>| {
                    let c = Arc::clone(&c);
                    async move {
                        if !bearer_ok(&headers) {
                            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
                        }
                        c.reads.fetch_add(1, Ordering::Relaxed);
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({ "id": id, "state": "Issued" })),
                        )
                    }
                }
            }),
        )
        .route(
            "/invoices/:id/pdf",
            get({
                let c = Arc::clone(&c);
                move |headers: HeaderMap, AxumPath(_id): AxumPath<String>| {
                    let c = Arc::clone(&c);
                    async move {
                        if !bearer_ok(&headers) {
                            return (
                                StatusCode::UNAUTHORIZED,
                                [("content-type", "application/json")],
                                b"{}".to_vec(),
                            );
                        }
                        c.reads.fetch_add(1, Ordering::Relaxed);
                        (
                            StatusCode::OK,
                            [("content-type", "application/pdf")],
                            PDF_MAGIC.to_vec(),
                        )
                    }
                }
            }),
        )
        // Deliberately present and deliberately never reached.
        .route(
            "/invoices/issue",
            post({
                let c = Arc::clone(&c);
                move || {
                    let c = Arc::clone(&c);
                    async move {
                        c.mutating_hits.fetch_add(1, Ordering::Relaxed);
                        StatusCode::OK
                    }
                }
            }),
        )
        .route(
            "/invoices/:id/submit",
            post({
                let c = Arc::clone(&c);
                move |AxumPath(_id): AxumPath<String>| {
                    let c = Arc::clone(&c);
                    async move {
                        c.mutating_hits.fetch_add(1, Ordering::Relaxed);
                        StatusCode::OK
                    }
                }
            }),
        );

    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        id.cert_pem.clone().into_bytes(),
        id.key_pem.clone().into_bytes(),
    )
    .await
    .expect("stub ABERP TLS");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let addr = listener.local_addr().expect("addr");
    let handle = axum_server::Handle::new();
    let h = handle.clone();
    tokio::spawn(async move {
        let _ = axum_server::from_tcp_rustls(listener, tls)
            .handle(h)
            .serve(app.into_make_service())
            .await;
    });

    StubAberp {
        base_url: format!("https://127.0.0.1:{}", addr.port()),
        fingerprint_hex: id.fingerprint_hex,
        counters,
        handle,
    }
}

// -- the whole portal, wired -------------------------------------------

pub struct Portal {
    pub base: String,
    pub knock: String,
    pub agent: Arc<Agent>,
    pub stub: StubAberp,
    pub rp_id: String,
    pub origin: String,
    pub state_dir: PathBuf,
}

pub async fn start_portal(tag: &str) -> Portal {
    aberp_portal_core::pin::install_default_crypto_provider();

    let state_dir =
        std::env::temp_dir().join(format!("aberp-portal-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_dir);
    std::fs::create_dir_all(&state_dir).expect("state dir");

    let stub = start_stub_aberp().await;

    // Leg B identities: the relay pins the agent, the agent pins the
    // relay. Both directions, exactly as ADR-0113 section 2.3 specifies.
    let relay_id = identity("relay.test");
    let agent_id = identity("agent.test");

    let broker = Arc::new(Broker::new());
    let leg_b_tls = aberp_portal_core::pin::relay_server_config(
        vec![PinnedFingerprint::from_hex(&agent_id.fingerprint_hex).expect("agent pin")],
        pem_certs(&relay_id.cert_pem),
        pem_key(&relay_id.key_pem),
    )
    .expect("leg B server config");

    let agent_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind agent leg");
    let agent_addr = agent_listener.local_addr().expect("addr");
    tokio::spawn(aberp_portal_relay::broker::accept_forever(
        Arc::clone(&broker),
        agent_listener,
        Arc::new(leg_b_tls),
    ));

    // Leg A: plaintext loopback -- see the module docs. The scanner
    // trap is the real one, with its real aggregator task, so the e2e
    // proves the deployed wiring rather than a stub.
    let (canary_handle, canary_rx) = Canary::new();
    // The real aggregator, driven at a test cadence. The production
    // windows are 30 s and 60 s; waiting those out would make the
    // canary tests minutes long and would tempt someone to stub the
    // aggregator instead, which is the thing worth testing.
    tokio::spawn(relay_canary::run_aggregator_with(
        Arc::clone(&canary_handle),
        Arc::clone(&broker),
        canary_rx,
        relay_canary::AggregatorConfig {
            flush_interval: Duration::from_millis(50),
            high_coalesce_window: Duration::from_millis(100),
        },
    ));
    let app = front::router(Arc::new(Front {
        broker: Arc::clone(&broker),
        canary: canary_handle,
    }));
    let front_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind front");
    let front_addr = front_listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(
            front_listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    let rp_id = "localhost".to_string();
    let origin = format!("http://127.0.0.1:{}", front_addr.port());

    let cfg = AgentConfig {
        rp_id: rp_id.clone(),
        origin: origin.clone(),
        rp_name: "ABERP".into(),
        relay_addr: format!("127.0.0.1:{}", agent_addr.port()),
        relay_server_name: "relay.test".into(),
        relay_fingerprint: PinnedFingerprint::from_hex(&relay_id.fingerprint_hex)
            .expect("relay pin"),
        client_cert_pem: write(&state_dir, "agent-cert.pem", &agent_id.cert_pem),
        client_key: SecretSource::File(write(&state_dir, "agent-key.pem", &agent_id.key_pem)),
        state_dir: state_dir.clone(),
        upstream: UpstreamConfig {
            base_url: stub.base_url.clone(),
            tls_fingerprint: stub.fingerprint_hex.clone(),
            // Inline, never the keychain: no test touches real secrets.
            bearer: SecretSource::Inline(UPSTREAM_BEARER.to_string()),
        },
        discovery: UpstreamDiscovery::Fixed,
        cookie_secure: false,
        tripwire_path: TRIPWIRE_PATH.to_string(),
        // The file sink: no SMTP, no keychain, no real secrets. The
        // production default is the SPOC; this is the dev/test form the
        // brief asks for.
        alert_sink: AlertSink::File(state_dir.join("alerts.log")),
    };

    let agent = Agent::new(cfg).expect("agent");
    let knock = agent.knock.load_or_mint().expect("knock");

    let a = Arc::clone(&agent);
    tokio::spawn(async move { tunnel::run_forever(a).await });

    // Wait for the tunnel to publish the knock token.
    for _ in 0..400 {
        if broker.knock_matches(&knock) {
            return Portal {
                base: format!("http://127.0.0.1:{}", front_addr.port()),
                knock,
                agent,
                stub,
                rp_id,
                origin,
                state_dir,
            };
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the agent never dialled the relay");
}

impl Portal {
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}{}", self.base, self.knock, path)
    }

    /// The agent's audit log, parsed.
    pub fn audit(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.agent.audit.path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The Mac-side probe log, parsed.
    pub fn canary_log(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.agent.canary.log_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The alerts the file sink wrote.
    pub fn alerts(&self) -> String {
        std::fs::read_to_string(self.state_dir.join("alerts.log")).unwrap_or_default()
    }

    /// Wait until the canary's window has flushed through the relay,
    /// down the tunnel, and into the Mac's probe log — or give up.
    ///
    /// Polls rather than sleeping a fixed interval: the path is
    /// genuinely asynchronous by design (the response must never wait
    /// on it), so the test waits for the outcome instead of guessing a
    /// duration.
    pub async fn await_canary(&self, at_least: usize) -> Vec<serde_json::Value> {
        for _ in 0..400 {
            let log = self.canary_log();
            let samples: Vec<_> = log
                .iter()
                .filter(|v| v.get("severity").is_some())
                .cloned()
                .collect();
            if samples.len() >= at_least {
                return samples;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.canary_log()
    }

    pub fn audit_kinds(&self) -> Vec<String> {
        self.audit()
            .iter()
            .filter_map(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
            .collect()
    }
}

fn pem_certs(pem: &str) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<_, _>>()
        .expect("pem certs")
}

fn pem_key(pem: &str) -> rustls::pki_types::PrivateKeyDer<'static> {
    rustls_pemfile::private_key(&mut pem.as_bytes())
        .expect("pem key")
        .expect("a private key")
}
