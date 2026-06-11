//! S350 / PR-39 (U5) — integration tests for the operator material-grade
//! override (`PATCH /api/quote-pricing-jobs/:quote_id`).
//!
//! Tests hit the `pub` library helper `amend_pricing_job_material_request`
//! directly (the WORKING serve-route posture per A159 — see
//! `serve_partners_route.rs`); the HTTP status mapping (200 / 400 / 404 /
//! 409) is structural in the handler and the 401 gate is pinned as a unit
//! test in `serve.rs`. Covered here:
//!
//! 1. **happy path** — a valid catalogue grade on a Failed row resets the
//!    row to Fetched, bumps `attempt_n`, and writes the
//!    `quote.material_grade_edited` audit row.
//! 2. **grade not in catalogue** — surfaces `NotInCatalogue` with the
//!    available count; nothing changes and no audit row lands.
//! 3. **terminal-state row** — a Posted row surfaces `NotEditable`; row
//!    unchanged.
//! 4. **wrong tenant** — a row owned by another tenant is invisible →
//!    `NotFound` (the 404, not 403, convention).
//! 5. **audit payload round-trip** — the appended payload decodes with
//!    old/new grade + previous_state + operator_user_id intact (F12).

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use ulid::Ulid;

use aberp::quote_pricing_jobs::{self, FailureKind, JobState};
use aberp::quoting_materials;
use aberp::serve::{self, AppState, MaterialEditRequestError};

const TEST_TENANT: &str = "serve_pricing_material_test";
/// A seeded catalogue grade (`quoting_materials::seed_if_empty`).
const VALID_GRADE: &str = "6061-T6";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-pricing-material")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn build_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    AppState {
        db_path: Arc::new(db_path),
        tenant,
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

/// Seed the catalogue + insert a Failed pricing-job row carrying the
/// (unknown) grade the customer submitted. `tenant` lets a test plant a
/// row under a FOREIGN tenant for the isolation case.
fn seed_failed_row(db_path: &PathBuf, tenant: &str, quote_id: &str) {
    let mut conn = duckdb::Connection::open(db_path).expect("open db");
    quoting_materials::seed_if_empty(&mut conn, tenant).expect("seed catalogue");
    quote_pricing_jobs::insert_fetched_job(
        &conn,
        quote_id,
        tenant,
        "cust@example.com",
        "Customer Kft.",
        "unknown",
        4,
        "bracket.step",
        "/tmp/bracket.step",
        fixed_ts(),
    )
    .expect("insert job");
    quote_pricing_jobs::set_failed(
        &mut conn,
        quote_id,
        tenant,
        "pricing",
        "material grade `unknown` is not in the catalogue snapshot",
        FailureKind::Permanent,
        fixed_ts(),
    )
    .expect("fail it");
}

fn read_row(db_path: &PathBuf, tenant: &str, quote_id: &str) -> quote_pricing_jobs::JobDetail {
    let conn = duckdb::Connection::open(db_path).expect("open db");
    quote_pricing_jobs::get_job_detail(&conn, quote_id, tenant)
        .expect("query")
        .expect("row present")
}

fn material_edit_entries(db_path: &PathBuf) -> Vec<aberp_audit_ledger::Entry> {
    let ledger = Ledger::open(
        db_path,
        TenantId::new(TEST_TENANT.to_string()).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
    .expect("open ledger");
    ledger
        .entries()
        .expect("read entries")
        .into_iter()
        .filter(|e| e.kind == EventKind::QuotePricingMaterialEdited)
        .collect()
}

#[test]
fn material_edit_happy_path_resets_state_and_writes_audit() {
    let dir = test_dir("happy");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    seed_failed_row(&db, TEST_TENANT, "q-happy-0000-0000-000000000000");

    let out = serve::amend_pricing_job_material_request(
        &state,
        "q-happy-0000-0000-000000000000",
        VALID_GRADE,
        "operator-ada",
    )
    .expect("edit must succeed");
    assert_eq!(out.old_grade, "unknown");
    assert_eq!(out.new_grade, VALID_GRADE);
    assert_eq!(out.previous_state, "failed");
    assert_eq!(out.new_attempt_n, 1);

    // Row reset to Fetched (re-enters pricing) + grade rewritten.
    let row = read_row(&db, TEST_TENANT, "q-happy-0000-0000-000000000000");
    assert_eq!(row.row.state, JobState::Fetched);
    assert_eq!(row.row.material_grade, VALID_GRADE);
    assert_eq!(row.row.attempt_n, 1);
    assert!(row.row.error_reason.is_none(), "error cleared");

    // Exactly one audit row landed.
    assert_eq!(
        material_edit_entries(&db).len(),
        1,
        "one quote.material_grade_edited row"
    );
}

#[test]
fn material_edit_grade_not_in_catalogue_400_no_change_no_audit() {
    let dir = test_dir("not-in-cat");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    seed_failed_row(&db, TEST_TENANT, "q-badcat-000-0000-000000000000");

    let err = serve::amend_pricing_job_material_request(
        &state,
        "q-badcat-000-0000-000000000000",
        "definitely-not-a-grade",
        "operator-ada",
    )
    .expect_err("a non-catalogue grade must be refused");
    match err {
        MaterialEditRequestError::NotInCatalogue { available_count } => {
            assert!(available_count > 0, "the seeded catalogue is non-empty");
        }
        other => panic!("expected NotInCatalogue, got {other:?}"),
    }

    // Row untouched — still Failed with the original grade.
    let row = read_row(&db, TEST_TENANT, "q-badcat-000-0000-000000000000");
    assert_eq!(row.row.state, JobState::Failed);
    assert_eq!(row.row.material_grade, "unknown");
    // No audit row.
    assert!(
        material_edit_entries(&db).is_empty(),
        "a refused edit writes no audit row"
    );
}

#[test]
fn material_edit_terminal_row_409_no_change() {
    let dir = test_dir("terminal");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    let qid = "q-posted-000-0000-000000000000";
    seed_failed_row(&db, TEST_TENANT, qid);
    // Drive the row to Posted (terminal — not editable).
    {
        let mut conn = duckdb::Connection::open(&db).expect("open db");
        // From Failed → operator retry resets to Fetched, then advance.
        quote_pricing_jobs::retry_job(&mut conn, qid, TEST_TENANT, fixed_ts()).expect("retry");
        quote_pricing_jobs::set_state(&conn, qid, TEST_TENANT, JobState::Extracting, fixed_ts())
            .expect("ex");
        quote_pricing_jobs::set_extracted(
            &mut conn,
            qid,
            TEST_TENANT,
            "blake3:x",
            "{}",
            fixed_ts(),
        )
        .expect("extract");
        quote_pricing_jobs::set_priced(&mut conn, qid, TEST_TENANT, "{}", 10.0, fixed_ts())
            .expect("price");
        quote_pricing_jobs::set_rendered(
            &mut conn,
            qid,
            TEST_TENANT,
            "/tmp/x.pdf",
            "2026-07-06",
            fixed_ts(),
        )
        .expect("render");
        quote_pricing_jobs::set_state(&conn, qid, TEST_TENANT, JobState::Posted, fixed_ts())
            .expect("post");
    }

    let err = serve::amend_pricing_job_material_request(&state, qid, VALID_GRADE, "operator-ada")
        .expect_err("a Posted row must refuse the edit");
    match err {
        MaterialEditRequestError::NotEditable { state: s } => assert_eq!(s, "posted"),
        other => panic!("expected NotEditable, got {other:?}"),
    }
    let row = read_row(&db, TEST_TENANT, qid);
    assert_eq!(row.row.state, JobState::Posted);
    assert!(material_edit_entries(&db).is_empty());
}

#[test]
fn material_edit_wrong_tenant_is_not_found() {
    let dir = test_dir("wrong-tenant");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    // The operator's own tenant has a (boot-seeded) catalogue, so the
    // grade is valid — this isolates the tenant check from the catalogue
    // check.
    {
        let mut conn = duckdb::Connection::open(&db).expect("open db");
        quoting_materials::seed_if_empty(&mut conn, TEST_TENANT).expect("seed own catalogue");
    }
    // Plant the row under a DIFFERENT tenant; the state's tenant is
    // TEST_TENANT, so the request can't see it → NotFound (404).
    seed_failed_row(&db, "some-other-tenant", "q-other-000-0000-000000000000");

    let err = serve::amend_pricing_job_material_request(
        &state,
        "q-other-000-0000-000000000000",
        VALID_GRADE,
        "operator-ada",
    )
    .expect_err("a foreign-tenant row is invisible");
    assert!(matches!(err, MaterialEditRequestError::NotFound));
    // The foreign row is untouched.
    let row = read_row(&db, "some-other-tenant", "q-other-000-0000-000000000000");
    assert_eq!(row.row.material_grade, "unknown");
    assert_eq!(row.row.state, JobState::Failed);
}

#[test]
fn material_edit_audit_payload_round_trips() {
    let dir = test_dir("audit-roundtrip");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    let qid = "q-audit-000-0000-0000-000000000000";
    seed_failed_row(&db, TEST_TENANT, qid);

    serve::amend_pricing_job_material_request(&state, qid, VALID_GRADE, "operator-bob")
        .expect("edit");

    let entries = material_edit_entries(&db);
    assert_eq!(entries.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_slice(&entries[0].payload).expect("decode payload");
    assert_eq!(payload["quote_id"], qid);
    assert_eq!(payload["tenant_id"], TEST_TENANT);
    assert_eq!(payload["old_grade"], "unknown");
    assert_eq!(payload["new_grade"], VALID_GRADE);
    assert_eq!(payload["previous_state"], "failed");
    assert_eq!(payload["operator_user_id"], "operator-bob");
    assert_eq!(payload["attempt_n"], 1);
}
