//! D-09 — DFARS 252.204-7012 cyber-incident intake, exercised end-to-end
//! through the `serve` request helper (the `serve_material_routes` /
//! `export_control_shipment_route` convention).
//!
//! Each test drives a real `AppState` (real shared `aberp_db::Handle`, real
//! in-tx audit append) against a per-test DuckDB file, then re-opens the ledger
//! with a fresh `Ledger` to prove the `incident.cyber_detected` row is durable
//! and visible (the Handle-vs-residual-opener guard from ADR-0098/0099). The
//! async handler is a thin `spawn_blocking` wrapper over `record_cyber_incident_request`,
//! so pinning the request fn pins the route's unit of work; the 201 / 400
//! status mapping is structural in `into_response`.
//!
//! Coverage:
//! 1. a valid intake records the incident, sources `operator_user_id` from the
//!    session (not the body), computes the 72h deadline, and appends exactly
//!    one `incident.cyber_detected`;
//! 2. a bad severity is rejected as `CyberIncidentError::InvalidSeverity` (→
//!    400) and appends nothing;
//! 3. an incident with neither CDI nor OCS affected omits the 72h deadline.

use std::path::PathBuf;
use std::sync::Arc;

use ulid::Ulid;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

use aberp::cyber_incident::{CyberIncidentError, CyberIncidentInput};
use aberp::serve::{self, AppState};

const TEST_TENANT: &str = "serve_cyber_incident_route_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xC9; 32]);
const OPERATOR: &str = "test-operator";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-cyber-incident")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn build_state(db_path: PathBuf) -> AppState {
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
        digital_id: std::sync::Arc::new(aberp_digital_id::MockProvider::new()),
    }
}

fn fresh_state(label: &str) -> (AppState, PathBuf) {
    let db_path = test_dir(label).join("tenant.duckdb");
    (build_state(db_path.clone()), db_path)
}

/// Re-open the ledger with a FRESH `Ledger` (not the seeding Handle) and count
/// the `incident.cyber_detected` rows — proves the append committed and is
/// visible to an independent reader.
fn incident_rows(db_path: &PathBuf) -> usize {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let ledger = Ledger::open(db_path, tenant, TEST_HASH).expect("open ledger");
    ledger
        .entries()
        .expect("read entries")
        .into_iter()
        .filter(|e| e.kind == EventKind::IncidentCyberDetected)
        .count()
}

fn cdi_incident() -> CyberIncidentInput {
    CyberIncidentInput {
        severity: "high".to_string(),
        scope_description: "Anomalous outbound traffic from CAD workstation segment".to_string(),
        detection_source: "siem".to_string(),
        detected_at_ms: Some(1_750_000_000_000),
        cdi_affected: true,
        cui_affected: true,
        ocs_affected: false,
        exfiltration_suspected: false,
        affected_systems: vec!["cad-ws-04".to_string()],
        mitigation_notes: Some("Segment isolated; credentials rotated.".to_string()),
    }
}

#[test]
fn valid_intake_records_and_fires_the_event() {
    let (state, db_path) = fresh_state("valid-intake");

    let rec = serve::record_cyber_incident_request(&state, OPERATOR, &cdi_incident())
        .expect("a well-formed intake must record");

    assert!(rec.incident_id.starts_with("inc_"));
    // operator_user_id comes from the session, not the request body.
    assert_eq!(rec.operator_user_id, OPERATOR);
    assert_eq!(rec.severity, "high");
    assert_eq!(rec.detection_source, "siem");
    // cdi_affected → the DFARS 72h clock started.
    assert_eq!(
        rec.dod_72h_report_due_at_ms,
        Some(1_750_000_000_000 + 72 * 60 * 60 * 1000)
    );

    // The event is durable + visible to an independent reader (drop the
    // writer Handle first so the fresh Ledger can open without a file-lock
    // race).
    drop(state);
    assert_eq!(incident_rows(&db_path), 1);
}

#[test]
fn bad_severity_is_rejected_and_appends_nothing() {
    let (state, db_path) = fresh_state("bad-severity");
    let mut input = cdi_incident();
    input.severity = "catastrophic".to_string();

    let err = serve::record_cyber_incident_request(&state, OPERATOR, &input)
        .expect_err("an unknown severity must be rejected");
    assert!(
        matches!(
            err.downcast_ref::<CyberIncidentError>(),
            Some(CyberIncidentError::InvalidSeverity { .. })
        ),
        "expected InvalidSeverity (→ 400), got: {err:?}"
    );

    // A rejected intake writes no ledger row.
    drop(state);
    assert_eq!(incident_rows(&db_path), 0);
}

#[test]
fn incident_without_cdi_or_ocs_omits_the_deadline() {
    let (state, db_path) = fresh_state("no-cdi-ocs");
    let mut input = cdi_incident();
    input.cdi_affected = false;
    input.ocs_affected = false;
    input.mitigation_notes = None;

    let rec = serve::record_cyber_incident_request(&state, OPERATOR, &input)
        .expect("a non-CDI/OCS incident is still recordable");
    assert_eq!(rec.dod_72h_report_due_at_ms, None);
    drop(state);
    assert_eq!(incident_rows(&db_path), 1);
}
