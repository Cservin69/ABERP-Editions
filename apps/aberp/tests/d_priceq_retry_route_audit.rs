//! D-PRICEQ (2026-08-22 prod head-of-line incident) — integration tests for
//! the operator Retry route (`POST /api/quote-pricing-jobs/:quote_id/retry`).
//!
//! The Retry click flips a row `Failed -> Fetched`, clears its error columns
//! and bumps `attempt_n` — a real state change that emitted NO audit event at
//! all. That mattered directly on prod: `aeb2771d` was operator-Retried out
//! of a permanent Failed state at 08:00 on the day of the incident and the
//! ledger recorded nothing, so the only evidence the wedged job had been
//! requeued by hand was the row's own mutated state. Its two sibling
//! dispositions (material-grade edit, failure delete) were already audited.
//!
//! Tests hit the `pub` library helper `retry_pricing_job_request` directly
//! (the WORKING serve-route posture per A159 — same as
//! `serve_pricing_job_material_route.rs`); the HTTP status mapping
//! (200 / 400 / 404 / 409) is structural in the handler.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use ulid::Ulid;

use aberp::quote_pricing_jobs::{self, FailureKind, JobState};
use aberp::serve::{self, AppState, RetryPricingJobRequestError};

const TEST_TENANT: &str = "d_priceq_retry_test";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-d-priceq-retry")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn build_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    AppState {
        db: aberp::serve::open_tenant_handle(&db_path, tenant.clone())
            .expect("open shared test DuckDB handle (ADR-0098 Gap 1a)"),
        db_path: Arc::new(db_path),
        tenant,
        nav_enabled: true,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
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

fn fixed_ts() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_750_000_000).unwrap()
}

/// Insert a job row and drive it to `Failed` with the prod incident's own
/// failure shape (a permanently-failed multi-solid STEP).
fn seed_failed_row(state: &AppState, tenant: &str, quote_id: &str) {
    let mut conn = state.db.write().expect("seed via shared handle");
    quote_pricing_jobs::insert_fetched_job(
        &conn,
        quote_id,
        tenant,
        "ervin@aben.ch",
        "Customer Kft.",
        "Acme Manufacturing Kft.",
        "6061-T6",
        1,
        "pump_adapter.step",
        "/tmp/pump_adapter.step",
        fixed_ts(),
    )
    .expect("insert job");
    quote_pricing_jobs::set_failed(
        &mut conn,
        quote_id,
        tenant,
        "extract",
        "STEP file contains 4 solids; expected exactly 1",
        FailureKind::Permanent,
        fixed_ts(),
    )
    .expect("fail it");
}

fn retry_entries(state: &AppState) -> Vec<aberp_audit_ledger::Entry> {
    // ADR-0098 Option 1: the retry writes its audit row INSIDE the Handle's
    // write transaction, so read it through the SAME shared instance.
    let conn = state.db.read().expect("read via shared handle");
    let ledger = Ledger::from_connection(
        conn,
        TenantId::new(TEST_TENANT.to_string()).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    );
    ledger
        .entries()
        .expect("read entries")
        .into_iter()
        .filter(|e| e.kind == EventKind::QuotePricingJobRetried)
        .collect()
}

fn read_state(state: &AppState, quote_id: &str) -> quote_pricing_jobs::PricingJobRow {
    let conn = state.db.read().expect("read via shared handle");
    quote_pricing_jobs::list_jobs(&conn, TEST_TENANT)
        .expect("list")
        .into_iter()
        .find(|r| r.quote_id == quote_id)
        .expect("row present")
}

/// Happy path — the requeue lands AND is audited, and the audit carries the
/// failure context the UPDATE just wiped off the row (after the commit this
/// entry is the only place it still exists).
///
/// Revert-proof: drop the `append_in_tx` call and the entry-count assert
/// fails; drop the `error_reason` field and the payload assert fails.
#[test]
fn retry_requeues_the_row_and_writes_one_audit_row() {
    let dir = test_dir("happy");
    let state = build_state(dir.join("aberp.duckdb"));
    let qid = "aeb2771d-0000-0000-0000-000000000000";
    seed_failed_row(&state, TEST_TENANT, qid);

    let new_n = serve::retry_pricing_job_request(&state, qid, "operator-ervin")
        .expect("retry must succeed on a Failed row");
    assert_eq!(new_n, 1, "attempt_n bumped");

    let row = read_state(&state, qid);
    assert_eq!(row.state, JobState::Fetched, "row re-enters the queue");
    assert_eq!(row.attempt_n, 1);
    assert!(
        row.error_reason.is_none(),
        "error columns cleared off the row"
    );

    let entries = retry_entries(&state);
    assert_eq!(
        entries.len(),
        1,
        "exactly one quote.pricing_job_retried row"
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&entries[0].payload).expect("decode payload");
    assert_eq!(payload["quote_id"], qid);
    assert_eq!(payload["previous_state"], "failed");
    assert_eq!(payload["attempt_n"], 1);
    assert_eq!(payload["operator_user_id"], "operator-ervin");
    assert_eq!(payload["failure_kind"], "permanent");
    assert_eq!(
        payload["error_reason"], "STEP file contains 4 solids; expected exactly 1",
        "the cleared failure context survives ONLY here"
    );
    assert_eq!(payload["error_stage"], "extract");
}

/// A row that is not `Failed` is refused (409) and NOTHING is written —
/// neither the state change nor an audit row claiming a requeue that never
/// happened. Before D-PRICEQ the UPDATE's `AND state = 'failed'` matched zero
/// rows while the route still answered 200.
#[test]
fn retry_on_a_non_failed_row_is_refused_and_writes_no_audit() {
    let dir = test_dir("not-retryable");
    let state = build_state(dir.join("aberp.duckdb"));
    let qid = "aeb2771d-0000-0000-0000-000000000001";
    seed_failed_row(&state, TEST_TENANT, qid);
    {
        let conn = state.db.write().expect("stage via shared handle");
        quote_pricing_jobs::set_state(&conn, qid, TEST_TENANT, JobState::Extracting, fixed_ts())
            .expect("force extracting");
    }

    let err = serve::retry_pricing_job_request(&state, qid, "operator-ervin")
        .expect_err("a non-Failed row must be refused");
    match err {
        RetryPricingJobRequestError::NotRetryable { state: s } => assert_eq!(s, "extracting"),
        other => panic!("expected NotRetryable, got {other:?}"),
    }

    assert_eq!(
        read_state(&state, qid).state,
        JobState::Extracting,
        "row untouched"
    );
    assert!(
        retry_entries(&state).is_empty(),
        "a refused retry must not claim a requeue in the ledger"
    );
}

/// A foreign / absent quote id is a 404, with nothing written.
#[test]
fn retry_on_an_unknown_row_is_not_found() {
    let dir = test_dir("not-found");
    let state = build_state(dir.join("aberp.duckdb"));

    let err = serve::retry_pricing_job_request(
        &state,
        "00000000-0000-0000-0000-0000000000ff",
        "operator-ervin",
    )
    .expect_err("an unknown quote id must be refused");
    assert!(
        matches!(err, RetryPricingJobRequestError::NotFound),
        "expected NotFound, got {err:?}"
    );
    assert!(retry_entries(&state).is_empty());
}
