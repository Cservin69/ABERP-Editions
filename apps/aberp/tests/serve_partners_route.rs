//! Integration tests for `/api/partners` CRUD (PR-48α / session-68).
//!
//! Six pin tests on the library-helper boundary (mirrors the WORKING
//! `serve_issue_route.rs` posture per A159 / A162 / A163):
//!
//! 1. **create happy path** — valid `PartnerInputs` rounds-trips through
//!    `create_partner_request` and returns a Partner with a
//!    server-minted `prt_<ULID>` id + Rfc3339 timestamps.
//! 2. **create validation failure** — empty `display_name` + bad
//!    `tax_number` surfaces as `PartnerRouteError::Validation` with
//!    structured per-field errors. The route handler maps this to 400
//!    with the `validation_failed` body shape.
//! 3. **list** — two creates + a list returns both partners ordered
//!    by `display_name` ASC.
//! 4. **get-by-id** — fetch the created partner; all fields round-trip.
//! 5. **update** — mutate one field; re-fetch sees the new value and a
//!    bumped `updated_at`.
//! 6. **soft-delete + 404-after-delete** — delete returns Ok; a
//!    subsequent `get_partner_request` surfaces `NotFound`; a list
//!    omits the soft-deleted row.
//!
//! All tests run against an in-process DuckDB file under a per-test
//! scratch directory; the HTTPS listener is not spun. The full HTTP
//! status-code mapping (400 / 404 / 200 / 204) is structural — axum's
//! `(Status, Json(...)).into_response()` builds the response from the
//! typed value; pinning the response bytes themselves would couple the
//! test to axum's private response shape per CLAUDE.md rule 2.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, TenantId};
use ulid::Ulid;

use aberp::nav_xml::CustomerVatStatus;
use aberp::partners::{CustomerType, PartnerInputs, PartnerKind};
use aberp::serve::{self, AppState, PartnerRouteError};

const TEST_TENANT: &str = "serve_partners_route_test";

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-partners")
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

fn minimal_valid_inputs(display: &str) -> PartnerInputs {
    PartnerInputs {
        display_name: display.to_string(),
        legal_name: format!("{} Kft.", display),
        kind: PartnerKind::Customer,
        // PR-97 / ADR-0048 — preserve pre-PR-97 implicit Domestic
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

// ──────────────────────────────────────────────────────────────────────
// Pin tests
// ──────────────────────────────────────────────────────────────────────

/// Pin #1 — create happy path. The route's library helper returns a
/// fully-populated Partner with server-minted `id` (prefixed `prt_`),
/// matching `display_name`/`legal_name`/`kind`/`tax_number`, and
/// Rfc3339 timestamps where `created_at == updated_at` and
/// `deleted_at IS None`.
#[test]
fn partners_create_happy_path_returns_populated_partner() {
    let dir = test_dir("create-happy");
    let state = build_state(dir.join("aberp.duckdb"));
    let inputs = minimal_valid_inputs("BSCE");

    let partner =
        serve::create_partner_request(&state, &inputs).expect("create happy path must succeed");

    assert!(
        partner.id.starts_with("prt_"),
        "partner id `{}` must be prefixed-ULID",
        partner.id
    );
    assert_eq!(partner.id.len(), 30, "prefixed PartnerId must be 30 chars");
    assert_eq!(partner.display_name, "BSCE");
    assert_eq!(partner.legal_name, "BSCE Kft.");
    assert_eq!(partner.kind, PartnerKind::Customer);
    assert_eq!(partner.tax_number.as_deref(), Some("12345678-1-42"));
    assert_eq!(partner.eu_vat_number.as_deref(), Some("HU12345678"));
    assert_eq!(partner.address_city.as_deref(), Some("Budapest"));
    assert_eq!(
        partner.created_at, partner.updated_at,
        "on create, created_at must equal updated_at"
    );
    assert!(
        partner.deleted_at.is_none(),
        "freshly-created partner must have NULL deleted_at"
    );

    let _keep = &dir;
}

/// Pin #2 — create validation failure. An empty `display_name` and a
/// malformed `tax_number` surface as `PartnerRouteError::Validation`
/// with structured per-field errors. The HTTP handler maps this to
/// 400 with the `validation_failed` envelope; the library boundary is
/// the load-bearing pin.
#[test]
fn partners_create_rejects_invalid_inputs_with_structured_errors() {
    let dir = test_dir("create-invalid");
    let state = build_state(dir.join("aberp.duckdb"));
    let inputs = PartnerInputs {
        display_name: "   ".to_string(),
        legal_name: "Valid Legal Kft.".to_string(),
        kind: PartnerKind::Both,
        // PR-97 / ADR-0048 — preserve pre-PR-97 implicit Domestic
        customer_vat_status: CustomerVatStatus::Domestic,
        customer_type: CustomerType::Unset,
        tax_number: Some("not-a-tax-number".to_string()),
        eu_vat_number: None,
        address_street: None,
        address_postal_code: None,
        address_city: None,
        address_country: None,
        bank_account: None,
        contact_email: None,
        contact_phone: None,
    };

    let err =
        serve::create_partner_request(&state, &inputs).expect_err("invalid inputs must reject");
    let errors = match err {
        PartnerRouteError::Validation(v) => v,
        other => panic!("expected Validation, got {other:?}"),
    };
    let flagged_fields: Vec<&str> = errors.iter().map(|e| e.field).collect();
    assert!(
        flagged_fields.contains(&"display_name"),
        "must flag display_name; got {:?}",
        flagged_fields
    );
    assert!(
        flagged_fields.contains(&"tax_number"),
        "must flag tax_number; got {:?}",
        flagged_fields
    );

    let _keep = &dir;
}

/// Pin #3 — list returns every active partner ordered by
/// `display_name` ASC. Two creates + one list call must return both
/// in alphabetical order.
#[test]
fn partners_list_returns_active_rows_ordered_by_display_name() {
    let dir = test_dir("list");
    let state = build_state(dir.join("aberp.duckdb"));

    serve::create_partner_request(&state, &minimal_valid_inputs("Zeta")).expect("create Zeta");
    serve::create_partner_request(&state, &minimal_valid_inputs("Alpha")).expect("create Alpha");

    let listed = serve::list_partners_request(&state, None).expect("list must succeed");
    assert_eq!(listed.len(), 2, "list must return both created partners");
    assert_eq!(
        listed[0].display_name, "Alpha",
        "list must order by display_name ASC"
    );
    assert_eq!(listed[1].display_name, "Zeta");

    // ?search=al filters case-insensitive prefix on display_name OR
    // legal_name. "Alpha" matches; "Zeta" does not.
    let filtered = serve::list_partners_request(&state, Some("al")).expect("search must succeed");
    assert_eq!(filtered.len(), 1, "search=al must match Alpha only");
    assert_eq!(filtered[0].display_name, "Alpha");

    let _keep = &dir;
}

/// Pin #4 — get-by-id round-trip. Every field set at create time
/// survives the SELECT path; missing optional fields stay `None`.
#[test]
fn partners_get_by_id_round_trips_every_field() {
    let dir = test_dir("get");
    let state = build_state(dir.join("aberp.duckdb"));
    let created = serve::create_partner_request(&state, &minimal_valid_inputs("Test"))
        .expect("create must succeed");

    let fetched = serve::get_partner_request(&state, &created.id).expect("get must succeed");
    assert_eq!(
        fetched, created,
        "get must return the exact Partner stored at create"
    );

    // Unknown id surfaces as NotFound (404 at the HTTP layer).
    let unknown_id = format!("prt_{}", Ulid::new());
    match serve::get_partner_request(&state, &unknown_id) {
        Err(PartnerRouteError::NotFound) => {}
        other => panic!("expected NotFound for unknown id, got {other:?}"),
    }

    let _keep = &dir;
}

/// Pin #5 — update bumps `updated_at` and persists the mutated field.
/// The original `created_at` must stay unchanged across the update
/// (only `updated_at` advances).
#[test]
fn partners_update_persists_mutated_field_and_bumps_updated_at() {
    let dir = test_dir("update");
    let state = build_state(dir.join("aberp.duckdb"));
    let created =
        serve::create_partner_request(&state, &minimal_valid_inputs("Original")).expect("create");

    // Sleep a millisecond so the formatted Rfc3339 string definitely
    // advances. Without this the test can race the same-instant case
    // and `assert_ne!` on the updated_at strings would flake.
    std::thread::sleep(std::time::Duration::from_millis(2));

    let mutated_inputs = PartnerInputs {
        display_name: "Renamed".to_string(),
        ..minimal_valid_inputs("Original")
    };
    let updated = serve::update_partner_request(
        &state,
        &created.id,
        &mutated_inputs,
        "test-operator",
        BinaryHash::from_bytes([0u8; 32]),
    )
    .expect("update must succeed");

    assert_eq!(updated.id, created.id, "id must stay stable across update");
    assert_eq!(updated.display_name, "Renamed", "mutation must persist");
    assert_eq!(
        updated.created_at, created.created_at,
        "created_at must stay stable across update"
    );
    assert_ne!(
        updated.updated_at, created.updated_at,
        "updated_at must advance"
    );

    // Update on unknown id surfaces as NotFound (404 at HTTP).
    let unknown_id = format!("prt_{}", Ulid::new());
    match serve::update_partner_request(
        &state,
        &unknown_id,
        &mutated_inputs,
        "test-operator",
        BinaryHash::from_bytes([0u8; 32]),
    ) {
        Err(PartnerRouteError::NotFound) => {}
        other => panic!("expected NotFound for unknown id, got {other:?}"),
    }

    let _keep = &dir;
}

/// Pin #6 — soft-delete + 404-after-delete. The row stays in the DB
/// (historical-invoice lookups can still resolve it), but the API
/// surface treats it as gone: get returns 404, list omits it.
#[test]
fn partners_soft_delete_makes_partner_invisible_to_api() {
    let dir = test_dir("delete");
    let state = build_state(dir.join("aberp.duckdb"));
    let created =
        serve::create_partner_request(&state, &minimal_valid_inputs("ToDelete")).expect("create");

    serve::delete_partner_request(&state, &created.id).expect("delete must succeed");

    // Get surfaces NotFound now (HTTP 404).
    match serve::get_partner_request(&state, &created.id) {
        Err(PartnerRouteError::NotFound) => {}
        other => panic!("expected NotFound after soft-delete, got {other:?}"),
    }

    // List omits the soft-deleted row.
    let listed = serve::list_partners_request(&state, None).expect("list");
    assert!(
        listed.is_empty(),
        "soft-deleted partner must not appear in list; got {:?}",
        listed
    );

    // Re-deleting the same id surfaces NotFound — defence against
    // a double-click DELETE re-issuing the request and the SPA
    // misreading a second 204 as "another partner deleted." Pinning
    // the second-call surface so an ill-considered refactor that
    // makes the soft-delete idempotent (returning Ok(()) the second
    // time) trips this test.
    match serve::delete_partner_request(&state, &created.id) {
        Err(PartnerRouteError::NotFound) => {}
        other => panic!("expected NotFound on re-delete, got {other:?}"),
    }

    let _keep = &dir;
}
