//! D-10 — DPAS priority-rating assignment, exercised end-to-end through the
//! `serve` request helper. A real `AppState` (shared `aberp_db::Handle`, real
//! in-tx audit append) assigns a rating to a seeded partner; a fresh `Ledger`
//! re-read proves the `supplier.dpas_priority_set` row is durable.
//!
//! Coverage:
//! 1. assigning a rating writes the column, echoes the previous value, and
//!    fires one `supplier.dpas_priority_set` per assignment;
//! 2. a malformed rating is rejected (`DpasRatingError::Invalid` → 400) and
//!    appends nothing;
//! 3. assigning to a missing partner returns `None` (→ 404) and appends nothing.

use std::path::PathBuf;
use std::sync::Arc;

use ulid::Ulid;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

use aberp::dpas_rating::DpasRatingError;
use aberp::nav_xml::CustomerVatStatus;
use aberp::partners::{CustomerType, PartnerInputs, PartnerKind};
use aberp::serve::{self, AppState};

const TEST_TENANT: &str = "serve_dpas_rating_route_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xDA; 32]);
const OPERATOR: &str = "test-operator";

fn test_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir()
            .join("aberp-dpas-rating")
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

fn supplier_inputs(display: &str) -> PartnerInputs {
    PartnerInputs {
        display_name: display.to_string(),
        legal_name: format!("{} Kft.", display),
        kind: PartnerKind::Both,
        customer_vat_status: CustomerVatStatus::Domestic,
        customer_type: CustomerType::Unset,
        tax_number: Some("12345678-1-42".to_string()),
        eu_vat_number: Some("HU12345678".to_string()),
        address_street: Some("Fő utca 1.".to_string()),
        address_postal_code: Some("1011".to_string()),
        address_city: Some("Budapest".to_string()),
        address_country: Some("Magyarország".to_string()),
        bank_account: None,
        contact_email: Some("ops@example.hu".to_string()),
        contact_phone: None,
    }
}

fn seed_partner(state: &AppState, display: &str) -> String {
    serve::create_partner_request(state, &supplier_inputs(display))
        .expect("seed partner")
        .id
}

fn dpas_rows(db_path: &PathBuf) -> usize {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let ledger = Ledger::open(db_path, tenant, TEST_HASH).expect("open ledger");
    ledger
        .entries()
        .expect("read entries")
        .into_iter()
        .filter(|e| e.kind == EventKind::SupplierDpasPrioritySet)
        .count()
}

#[test]
fn assigning_a_rating_writes_the_column_and_fires_the_event() {
    let db_path = test_dir("assign").join("tenant.duckdb");
    let state = build_state(db_path.clone());
    let partner_id = seed_partner(&state, "Acme Aerospace");

    let first = serve::set_partner_dpas_rating_request(&state, OPERATOR, &partner_id, "DO-A1")
        .expect("assign ok")
        .expect("partner exists");
    assert_eq!(first.dpas_rating, "DO-A1");
    assert_eq!(first.previous_rating, None);

    let second = serve::set_partner_dpas_rating_request(&state, OPERATOR, &partner_id, "DX-A7")
        .expect("re-assign ok")
        .expect("partner exists");
    assert_eq!(second.dpas_rating, "DX-A7");
    assert_eq!(second.previous_rating.as_deref(), Some("DO-A1"));

    drop(state);
    assert_eq!(dpas_rows(&db_path), 2);
}

#[test]
fn a_malformed_rating_is_rejected_and_appends_nothing() {
    let db_path = test_dir("bad-rating").join("tenant.duckdb");
    let state = build_state(db_path.clone());
    let partner_id = seed_partner(&state, "Beta Metals");

    let err = serve::set_partner_dpas_rating_request(&state, OPERATOR, &partner_id, "NOT-A-RATING")
        .expect_err("a malformed rating must be rejected");
    assert!(
        matches!(
            err.downcast_ref::<DpasRatingError>(),
            Some(DpasRatingError::Invalid { .. })
        ),
        "expected DpasRatingError::Invalid (→ 400), got: {err:?}"
    );

    drop(state);
    assert_eq!(dpas_rows(&db_path), 0);
}

#[test]
fn assigning_to_a_missing_partner_is_none_and_appends_nothing() {
    let db_path = test_dir("missing-partner").join("tenant.duckdb");
    let state = build_state(db_path.clone());

    let outcome =
        serve::set_partner_dpas_rating_request(&state, OPERATOR, "ptr_does_not_exist", "DO-A1")
            .expect("assign returns Ok for a missing partner");
    assert!(outcome.is_none(), "missing partner → None (404)");

    drop(state);
    assert_eq!(dpas_rows(&db_path), 0);
}
