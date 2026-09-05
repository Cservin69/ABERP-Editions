//! D-11 — material reservation FSM + certificate operator routes, exercised
//! end-to-end through the `serve` request helpers (the same library-helper
//! boundary `serve_partners_route.rs` pins per A159 / A162 / A163).
//!
//! Each test drives a real `AppState` — a real shared `aberp_db::Handle`, a
//! real in-tx audit append — against a per-test DuckDB file. This is the
//! "handler exercised end-to-end" bar: the async axum handlers are thin
//! `spawn_blocking` wrappers over exactly these `*_request` fns, so pinning
//! the fns pins the routes' unit of work (the HTTPS listener is not spun; the
//! 201/200/404/409/400 status mapping is structural in `into_response`).
//!
//! Coverage:
//! 1. **reserve → release** round-trips the balance (reserved 0 → 30 → 0).
//! 2. **reserve → consume** draws stock down (on_hand 100 → 60, consumed 40).
//! 3. **reserve past on-hand** surfaces `InsufficientMaterial` (→ 409).
//! 4. **release an unknown reservation** surfaces `ReservationNotFound` (→ 404).
//! 5. **attach cert → list** round-trips a `MillCert`; a bad kind / bad URL
//!    surface `CertKindUnknown` / `CertUrlInvalid` (→ 400).

use std::path::PathBuf;
use std::sync::Arc;

use duckdb::params;
use ulid::Ulid;

use aberp_audit_ledger::{BinaryHash, TenantId};

use aberp::material_inventory::{
    ensure_schema as ensure_inventory_schema, MaterialCertKind, MaterialInventoryError,
    ReservationState,
};
use aberp::serve::{
    self, AppState, AttachMaterialCertBody, ReleaseReservationBody, ReserveMaterialBody,
};

const TEST_TENANT: &str = "serve_material_routes_test";
const OPERATOR: &str = "test-operator";

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-material")
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

fn fresh_state(label: &str) -> AppState {
    build_state(test_dir(label).join("tenant.duckdb"))
}

/// Seed an `inventory_balances` row with `on_hand` kg on hand, through the
/// shared writer so the reserve path (which upserts at zeros on conflict)
/// sees it.
fn seed_balance(state: &AppState, grade: &str, on_hand: f64) {
    let mut guard = state.db.write().expect("write guard for seed");
    let tx = guard.transaction().expect("seed tx");
    ensure_inventory_schema(&tx).expect("ensure inventory schema");
    tx.execute(
        "INSERT INTO inventory_balances (
            tenant_id, material_grade, on_hand_qty, reserved_qty,
            committed_qty, consumed_qty, unit_of_measure, last_updated
         ) VALUES (?1, ?2, ?3, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
        params![TEST_TENANT, grade, on_hand],
    )
    .expect("seed balance row");
    tx.commit().expect("commit seed");
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[test]
fn reserve_then_release_round_trips_the_balance() {
    let state = fresh_state("reserve-release");
    let grade = "6061-T6";
    seed_balance(&state, grade, 100.0);

    let reserved = serve::reserve_material_request(
        &state,
        OPERATOR,
        grade,
        &ReserveMaterialBody {
            qty: 30.0,
            qty_unit_kind: None,
            quote_id: Some("q-round-trip".to_string()),
        },
    )
    .expect("reserve must succeed against seeded stock");

    assert_eq!(reserved.new_state, ReservationState::Reserved);
    assert!(
        reserved.previous_state.is_none(),
        "a fresh reserve has no prior state"
    );
    assert_eq!(reserved.material_grade, grade);
    assert_eq!(reserved.qty, 30.0);
    assert_eq!(reserved.balance_after.reserved_qty, 30.0);
    assert_eq!(reserved.balance_after.on_hand_qty, 100.0);

    let released = serve::release_reservation_request(
        &state,
        OPERATOR,
        &reserved.reservation_id,
        &ReleaseReservationBody {
            reason: Some("customer cancelled".to_string()),
        },
    )
    .expect("release of a live reservation must succeed");

    assert_eq!(released.new_state, ReservationState::Released);
    assert_eq!(released.previous_state, Some(ReservationState::Reserved));
    assert_eq!(released.balance_after.reserved_qty, 0.0);
    assert_eq!(released.balance_after.on_hand_qty, 100.0);
}

#[test]
fn reserve_then_consume_draws_stock_down() {
    let state = fresh_state("reserve-consume");
    let grade = "Ti-6Al-4V";
    seed_balance(&state, grade, 100.0);

    let reserved = serve::reserve_material_request(
        &state,
        OPERATOR,
        grade,
        &ReserveMaterialBody {
            qty: 40.0,
            qty_unit_kind: Some("kg".to_string()),
            quote_id: None,
        },
    )
    .expect("reserve must succeed");
    assert_eq!(reserved.new_state, ReservationState::Reserved);

    let consumed = serve::consume_reservation_request(&state, OPERATOR, &reserved.reservation_id)
        .expect("consume of a reserved earmark must succeed");

    assert_eq!(consumed.new_state, ReservationState::Consumed);
    assert_eq!(consumed.previous_state, Some(ReservationState::Reserved));
    assert_eq!(consumed.balance_after.on_hand_qty, 60.0);
    assert_eq!(consumed.balance_after.reserved_qty, 0.0);
    assert_eq!(consumed.balance_after.consumed_qty, 40.0);
}

#[test]
fn reserve_past_on_hand_is_insufficient_material() {
    let state = fresh_state("reserve-insufficient");
    let grade = "17-4PH";
    seed_balance(&state, grade, 10.0);

    let err = serve::reserve_material_request(
        &state,
        OPERATOR,
        grade,
        &ReserveMaterialBody {
            qty: 50.0,
            qty_unit_kind: None,
            quote_id: None,
        },
    )
    .expect_err("reserving beyond on-hand must fail");

    assert!(
        matches!(
            err.downcast_ref::<MaterialInventoryError>(),
            Some(MaterialInventoryError::InsufficientMaterial { .. })
        ),
        "expected InsufficientMaterial (→ 409), got: {err:?}"
    );
}

#[test]
fn release_unknown_reservation_is_not_found() {
    let state = fresh_state("release-unknown");

    let err = serve::release_reservation_request(
        &state,
        OPERATOR,
        "rsv_does_not_exist",
        &ReleaseReservationBody::default(),
    )
    .expect_err("releasing a missing reservation must fail");

    assert!(
        matches!(
            err.downcast_ref::<MaterialInventoryError>(),
            Some(MaterialInventoryError::ReservationNotFound { .. })
        ),
        "expected ReservationNotFound (→ 404), got: {err:?}"
    );
}

#[test]
fn attach_cert_then_list_round_trips_and_rejects_bad_input() {
    let state = fresh_state("cert-attach-list");
    let grade = "15-5PH";

    let rec = serve::attach_material_cert_request(
        &state,
        OPERATOR,
        grade,
        &AttachMaterialCertBody {
            cert_kind: "mill_cert".to_string(),
            cert_url: "https://certs.example.com/15-5ph-heat-A1.pdf".to_string(),
            lot_id: Some("LOT-A1".to_string()),
        },
    )
    .expect("attach a well-formed mill cert must succeed");

    assert_eq!(rec.cert_kind, MaterialCertKind::MillCert);
    assert_eq!(rec.material_grade, grade);
    assert_eq!(rec.lot_id.as_deref(), Some("LOT-A1"));
    assert!(rec.cert_id.starts_with("mcert_"));

    let listed =
        serve::list_material_certs_request(&state, grade).expect("list certs must succeed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].cert_id, rec.cert_id);
    assert_eq!(
        listed[0].cert_url,
        "https://certs.example.com/15-5ph-heat-A1.pdf"
    );

    let bad_kind = serve::attach_material_cert_request(
        &state,
        OPERATOR,
        grade,
        &AttachMaterialCertBody {
            cert_kind: "not_a_real_kind".to_string(),
            cert_url: "https://certs.example.com/x.pdf".to_string(),
            lot_id: None,
        },
    )
    .expect_err("an unknown cert kind must be rejected");
    assert!(
        matches!(
            bad_kind.downcast_ref::<MaterialInventoryError>(),
            Some(MaterialInventoryError::CertKindUnknown { .. })
        ),
        "expected CertKindUnknown (→ 400), got: {bad_kind:?}"
    );

    let bad_url = serve::attach_material_cert_request(
        &state,
        OPERATOR,
        grade,
        &AttachMaterialCertBody {
            cert_kind: "mill_cert".to_string(),
            cert_url: "ftp://not-allowed/x.pdf".to_string(),
            lot_id: None,
        },
    )
    .expect_err("a non-http(s)/file cert URL must be rejected");
    assert!(
        matches!(
            bad_url.downcast_ref::<MaterialInventoryError>(),
            Some(MaterialInventoryError::CertUrlInvalid { .. })
        ),
        "expected CertUrlInvalid (→ 400), got: {bad_url:?}"
    );

    // The two rejects wrote nothing: the list is still exactly the one cert.
    let after = serve::list_material_certs_request(&state, grade).expect("re-list");
    assert_eq!(after.len(), 1);
}
