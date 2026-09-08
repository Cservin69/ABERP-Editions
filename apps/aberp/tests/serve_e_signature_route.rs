//! D-15 — the 21 CFR Part 11 e-signature ceremony, exercised end-to-end
//! through the `serve` request helper. A real `AppState` (shared
//! `aberp_db::Handle`, real in-tx audit append, the mock DigitalIdProvider)
//! signs a record; a fresh `Ledger` re-read proves the
//! `personnel.signature_applied` row is durable.
//!
//! Coverage:
//! 1. applying a signature fires `personnel.signature_applied`, sourcing the
//!    signer id + algorithm from the provider (not the request);
//! 2. an empty record kind/id is rejected (`ESignatureError` → 400) and
//!    appends nothing.

use std::path::PathBuf;
use std::sync::Arc;

use ulid::Ulid;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use aberp_digital_id::{MOCK_ALGORITHM, MOCK_OPERATOR_ID};

use aberp::e_signature::{ESignatureError, SignatureCeremonyInput};
use aberp::serve::{self, AppState};

const TEST_TENANT: &str = "serve_e_signature_route_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xE5; 32]);

fn test_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir()
            .join("aberp-e-signature")
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
                operator_login: "test-operator".to_string(),
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

fn signature_rows(db_path: &PathBuf) -> usize {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let ledger = Ledger::open(db_path, tenant, TEST_HASH).expect("open ledger");
    ledger
        .entries()
        .expect("read entries")
        .into_iter()
        .filter(|e| e.kind == EventKind::PersonnelSignatureApplied)
        .count()
}

#[test]
fn applying_a_signature_fires_the_ceremony_event() {
    let db_path = test_dir("sign").join("tenant.duckdb");
    let state = build_state(db_path.clone());

    let record = serve::apply_signature_request(
        &state,
        &SignatureCeremonyInput {
            signed_record_kind: "work_order".to_string(),
            signed_record_id: "wo_123".to_string(),
        },
    )
    .expect("signing must succeed");

    // Signer id + algorithm come from the provider, not the request.
    assert_eq!(record.operator_user_id, MOCK_OPERATOR_ID);
    assert_eq!(record.signature_algorithm, MOCK_ALGORITHM);
    assert_eq!(record.signed_record_kind, "work_order");
    assert_eq!(record.signed_record_id, "wo_123");

    drop(state);
    assert_eq!(signature_rows(&db_path), 1);
}

#[test]
fn an_empty_target_is_rejected_and_appends_nothing() {
    let db_path = test_dir("empty-target").join("tenant.duckdb");
    let state = build_state(db_path.clone());

    let err = serve::apply_signature_request(
        &state,
        &SignatureCeremonyInput {
            signed_record_kind: "   ".to_string(),
            signed_record_id: "wo_1".to_string(),
        },
    )
    .expect_err("an empty record kind must be rejected");
    assert!(
        matches!(
            err.downcast_ref::<ESignatureError>(),
            Some(ESignatureError::EmptyRecordKind)
        ),
        "expected EmptyRecordKind (→ 400), got: {err:?}"
    );

    drop(state);
    assert_eq!(signature_rows(&db_path), 0);
}
