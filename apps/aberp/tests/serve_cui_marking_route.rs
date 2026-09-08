//! D-08 — CUI marking + access-event trail, exercised end-to-end through the
//! `serve` request helpers (the `serve_material_routes` /
//! `export_control_shipment_route` convention).
//!
//! A real `AppState` (shared `aberp_db::Handle`, real in-tx audit append) marks
//! a product and reads the marking; a fresh `Ledger` re-read proves the two
//! `cui.*` rows are durable + visible.
//!
//! Coverage:
//! 1. applying a marking fires `cui.marking_applied` (with the rendered banner),
//!    and each read of a marked product records a GRANT `cui.access_event`;
//! 2. applying to a missing product returns `None` (→ 404) and appends nothing;
//! 3. a bad band is rejected as `CuiMarkingError::UnknownBand` (→ 400) and
//!    appends nothing;
//! 4. reading an UNMARKED product records no access event.

use std::path::PathBuf;
use std::sync::Arc;

use ulid::Ulid;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::{Currency, ProductUnit};

use aberp::cui_marking::{CuiMarkingError, CuiMarkingInput};
use aberp::products::{create_product, ProductInputs};
use aberp::serve::{self, AppState, CuiReadOutcome};

const TEST_TENANT: &str = "serve_cui_marking_route_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xC8; 32]);
const OPERATOR: &str = "test-operator";

fn test_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir()
            .join("aberp-cui-marking")
            .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// A CLEARED pilot operator (holds `cui`) — CUI reads grant.
fn cleared_state(db_path: PathBuf) -> AppState {
    build_state(db_path, vec!["operator".to_string(), "cui".to_string()])
}

/// A scope-less operator (only `operator`) — CUI reads are denied.
fn scopeless_state(db_path: PathBuf) -> AppState {
    build_state(db_path, vec!["operator".to_string()])
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

/// Insert a product through the shared writer; return its server-minted id.
fn seed_product(state: &AppState, name: &str) -> String {
    let conn = state.db.write().expect("write guard for seed");
    let product = create_product(
        &conn,
        TEST_TENANT,
        &ProductInputs {
            name: name.to_string(),
            unit: ProductUnit::Own("pcs".to_string()),
            currency: Currency::Huf,
            unit_price_minor: 1000,
        },
    )
    .expect("seed product");
    product.id
}

/// Count ledger rows of a given kind via a fresh `Ledger` (independent reader).
fn kind_count(db_path: &PathBuf, kind: EventKind) -> usize {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let ledger = Ledger::open(db_path, tenant, TEST_HASH).expect("open ledger");
    ledger
        .entries()
        .expect("read entries")
        .into_iter()
        .filter(|e| e.kind == kind)
        .count()
}

#[test]
fn apply_marks_and_each_read_records_a_grant() {
    let db_path = test_dir("apply-read").join("tenant.duckdb");
    let state = cleared_state(db_path.clone());
    let product_id = seed_product(&state, "Fan blade");

    let applied = serve::apply_product_cui_marking_request(
        &state,
        OPERATOR,
        &product_id,
        &CuiMarkingInput {
            band: "cui".to_string(),
            category: Some("cti".to_string()),
            dissemination: vec!["noforn".to_string()],
        },
    )
    .expect("apply must succeed")
    .expect("product exists");

    assert_eq!(applied.banner_str, "CUI//SP-CTI//NOFORN");
    assert_eq!(applied.entity_id, product_id);

    // Read twice with a CLEARED operator — every read records a GRANT.
    match serve::read_product_cui_marking_request(&state, OPERATOR, &product_id).expect("read ok") {
        CuiReadOutcome::Granted(rec) => assert_eq!(rec.banner_str, "CUI//SP-CTI//NOFORN"),
        other => panic!("expected Granted, got {other:?}"),
    }
    assert!(matches!(
        serve::read_product_cui_marking_request(&state, OPERATOR, &product_id).expect("read ok"),
        CuiReadOutcome::Granted(_)
    ));

    drop(state);
    assert_eq!(kind_count(&db_path, EventKind::CuiMarkingApplied), 1);
    assert_eq!(kind_count(&db_path, EventKind::CuiAccessEvent), 2);
}

#[test]
fn apply_to_missing_product_is_none_and_appends_nothing() {
    let db_path = test_dir("missing-product").join("tenant.duckdb");
    let state = cleared_state(db_path.clone());

    let outcome = serve::apply_product_cui_marking_request(
        &state,
        OPERATOR,
        "prd_does_not_exist",
        &CuiMarkingInput {
            band: "secret".to_string(),
            category: None,
            dissemination: vec![],
        },
    )
    .expect("apply returns Ok for a missing product");
    assert!(outcome.is_none(), "missing product → None (404)");

    drop(state);
    assert_eq!(kind_count(&db_path, EventKind::CuiMarkingApplied), 0);
}

#[test]
fn bad_band_is_rejected_and_appends_nothing() {
    let db_path = test_dir("bad-band").join("tenant.duckdb");
    let state = cleared_state(db_path.clone());
    let product_id = seed_product(&state, "Bracket");

    let err = serve::apply_product_cui_marking_request(
        &state,
        OPERATOR,
        &product_id,
        &CuiMarkingInput {
            band: "mystery".to_string(),
            category: None,
            dissemination: vec![],
        },
    )
    .expect_err("an unknown band must be rejected");
    assert!(
        matches!(
            err.downcast_ref::<CuiMarkingError>(),
            Some(CuiMarkingError::UnknownBand { .. })
        ),
        "expected UnknownBand (→ 400), got: {err:?}"
    );

    drop(state);
    assert_eq!(kind_count(&db_path, EventKind::CuiMarkingApplied), 0);
}

#[test]
fn reading_an_unmarked_product_records_no_access_event() {
    let db_path = test_dir("unmarked-read").join("tenant.duckdb");
    let state = cleared_state(db_path.clone());
    let product_id = seed_product(&state, "Unmarked widget");

    let outcome =
        serve::read_product_cui_marking_request(&state, OPERATOR, &product_id).expect("read ok");
    assert!(
        matches!(outcome, CuiReadOutcome::Unmarked),
        "an unmarked product is not an access-control surface, got {outcome:?}"
    );

    drop(state);
    assert_eq!(kind_count(&db_path, EventKind::CuiAccessEvent), 0);
}

/// ADR-0117 §8b — a scope-less operator reading a CUI-marked product is DENIED,
/// the marking is withheld, and the denial is on the trail.
#[test]
fn a_scope_less_operator_is_denied_and_the_marking_is_withheld() {
    let db_path = test_dir("deny").join("tenant.duckdb");
    let product_id;
    {
        // Mark the product with a CLEARED operator, then drop that Handle so a
        // fresh (scope-less) AppState can open the same DB.
        let cleared = cleared_state(db_path.clone());
        product_id = seed_product(&cleared, "Classified widget");
        serve::apply_product_cui_marking_request(
            &cleared,
            OPERATOR,
            &product_id,
            &CuiMarkingInput {
                band: "cui".to_string(),
                category: Some("cti".to_string()),
                dissemination: vec![],
            },
        )
        .expect("apply ok")
        .expect("product exists");
    }

    let scopeless = scopeless_state(db_path.clone());
    let outcome = serve::read_product_cui_marking_request(&scopeless, OPERATOR, &product_id)
        .expect("read ok (a denial is not an error)");
    assert!(
        matches!(outcome, CuiReadOutcome::Denied),
        "a scope-less operator must be denied, got {outcome:?}"
    );

    drop(scopeless);
    // The marking was applied once; the denied read still recorded an access
    // event (every CUI access decision is on the trail).
    assert_eq!(kind_count(&db_path, EventKind::CuiMarkingApplied), 1);
    assert_eq!(kind_count(&db_path, EventKind::CuiAccessEvent), 1);
}
