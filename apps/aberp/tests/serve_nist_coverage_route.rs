//! D-04 (ADR-0121) — NIST SP 800-171 ledger-evidence coverage report, exercised
//! end-to-end through the `serve::build_nist_coverage_report` helper against a
//! real `AppState` (shared `aberp_db::Handle`).
//!
//! Seeds real audit events (a CUI marking → `cui.marking_applied`; a read →
//! `cui.access_event`) and asserts the derived report:
//! 1. classifies the two evidenced controls (MP 3.8.4, AC 3.1.3) as `evidenced`,
//!    a control with no mapped kind as `no_automated_evidence`, and covers all
//!    110 controls;
//! 2. never claims "compliant"/"satisfied" and carries the honesty disclaimer;
//! 3. an assessment window that predates the events excludes them.

use std::path::PathBuf;
use std::sync::Arc;

use ulid::Ulid;

use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_billing::{Currency, ProductUnit};

use aberp::cui_marking::CuiMarkingInput;
use aberp::nist_coverage::TimeWindow;
use aberp::products::{create_product, ProductInputs};
use aberp::serve::{self, AppState};

const TEST_TENANT: &str = "serve_nist_coverage_route_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xC4; 32]);
const OPERATOR: &str = "test-operator";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-nist-coverage")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// A CLEARED pilot operator (holds `cui`) so the seed marking + read succeed.
fn cleared_state(db_path: PathBuf) -> AppState {
    build_state(db_path, vec!["operator".to_string(), "cui".to_string()])
}

fn build_state(db_path: PathBuf, scopes: Vec<String>) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    AppState {
        db: aberp::serve::open_tenant_handle(&db_path, tenant.clone())
            .expect("open shared test DuckDB handle (ADR-0098 Gap 1a)"),
        db_path: Arc::new(db_path),
        tenant,
        nav_enabled: true,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(TEST_HASH),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        nav_poll_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
            aberp::serve::NAV_POLL_DAEMON_CONCURRENCY,
        )),
        boot_state: Arc::new(std::sync::RwLock::new(
            aberp::serve::ServeBootState::Ready {
                operator_login: OPERATOR.to_string(),
            },
        )),
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
        digital_id: std::sync::Arc::new(aberp_digital_id::MockProvider::with_scopes(scopes)),
    }
}

fn seed_product(state: &AppState, name: &str) -> String {
    let conn = state.db.write().expect("write guard for seed");
    create_product(
        &conn,
        TEST_TENANT,
        &ProductInputs {
            name: name.to_string(),
            unit: ProductUnit::Own("pcs".to_string()),
            currency: Currency::Huf,
            unit_price_minor: 1000,
        },
    )
    .expect("seed product")
    .id
}

/// Find the report control row whose dotted id (before the colon) equals `id`.
fn control_state<'a>(report: &'a serde_json::Value, id: &str) -> &'a str {
    report["controls"]
        .as_array()
        .expect("controls array")
        .iter()
        .find(|c| {
            c["control"]
                .as_str()
                .map(|s| s.split(':').next() == Some(id))
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("control {id} not in report"))["state"]
        .as_str()
        .expect("state string")
}

/// Seed a marking + one read, then assert the derived coverage.
#[test]
fn coverage_reports_evidenced_controls_from_ledger() {
    let db_path = test_dir("evidenced").join("tenant.duckdb");
    let state = cleared_state(db_path);
    let product_id = seed_product(&state, "Fan blade");

    // Fire cui.marking_applied (→ MP 3.8.4) and cui.access_event (→ AC 3.1.3).
    serve::apply_product_cui_marking_request(
        &state,
        OPERATOR,
        &product_id,
        &CuiMarkingInput {
            band: "cui".to_string(),
            category: Some("cti".to_string()),
            dissemination: vec!["noforn".to_string()],
        },
    )
    .expect("apply ok")
    .expect("product exists");
    serve::read_product_cui_marking_request(&state, OPERATOR, &product_id).expect("read ok");

    let report =
        serve::build_nist_coverage_report(&state, &TimeWindow::default()).expect("build report");

    // All 110 controls present; the two seeded ones evidenced.
    assert_eq!(report["summary"]["total"], 110);
    assert_eq!(
        control_state(&report, "3.8.4"),
        "evidenced",
        "MP 3.8.4 (marking)"
    );
    assert_eq!(
        control_state(&report, "3.1.3"),
        "evidenced",
        "AC 3.1.3 (access)"
    );
    // A control with a mapped kind but no event this run is mapped-not-exercised
    // (AC 3.1.2 maps to personnel.access_*, none of which fired here).
    assert_eq!(control_state(&report, "3.1.2"), "mapped_not_exercised");
    // A control with no mapped kind at all.
    assert_eq!(control_state(&report, "3.1.1"), "no_automated_evidence");

    // Honesty: never a compliance/satisfaction verdict; disclaimer present.
    let s = report.to_string();
    assert!(!s.contains("compliant"), "must not claim compliant");
    assert!(!s.contains("satisfied"), "must not claim satisfied");
    assert!(report["disclaimer"].as_str().unwrap().contains("assessor"));
    assert!(
        report["summary"]["evidenced"].as_u64().unwrap() >= 2,
        "at least MP 3.8.4 + AC 3.1.3"
    );
}

/// An assessment window that ends before the events excludes them: nothing is
/// evidenced even though the ledger holds the rows.
#[test]
fn coverage_window_excludes_out_of_range_events() {
    let db_path = test_dir("window").join("tenant.duckdb");
    let state = cleared_state(db_path);
    let product_id = seed_product(&state, "Bracket");
    serve::apply_product_cui_marking_request(
        &state,
        OPERATOR,
        &product_id,
        &CuiMarkingInput {
            band: "secret".to_string(),
            category: None,
            dissemination: vec![],
        },
    )
    .expect("apply ok")
    .expect("product exists");

    // Window ending at epoch 1ms — long before the just-written events.
    let past = TimeWindow {
        from_ms: None,
        to_ms: Some(1),
    };
    let report = serve::build_nist_coverage_report(&state, &past).expect("build report");
    assert_eq!(report["summary"]["total"], 110);
    assert_eq!(
        report["summary"]["evidenced"].as_u64().unwrap(),
        0,
        "no event falls within a window ending at epoch 1ms"
    );
    // The marking control is now mapped-but-unexercised within the window.
    assert_eq!(control_state(&report, "3.8.4"), "mapped_not_exercised");
}
