//! Integration tests for `/api/products` CRUD (PR-91).
//!
//! Mirrors `serve_partners_route.rs` in shape — six pins on the
//! library-helper boundary, all running against an in-process DuckDB
//! file under a per-test scratch directory. The HTTPS listener is
//! NOT spun.
//!
//! Pins:
//!
//!   1. **create happy path (Nav unit)** — valid `ProductInputs`
//!      round-trip through `create_product_request`; returns a Product
//!      with server-minted `prd_<ULID>` id + Rfc3339 timestamps +
//!      `deleted_at IS None`.
//!   2. **create happy path (Own unit — `liter@15C`)** — the
//!      load-bearing OWN escape hatch survives the wire/DB round-trip;
//!      the unit comes back as `ProductUnit::Own("liter@15C")`. This
//!      is the case ADR-0046 pins as the canonical motivation for the
//!      `{Nav | Own}` shape.
//!   3. **create validation failure** — empty name + Own with an empty
//!      label + negative price surface as
//!      `ProductRouteError::Validation` with structured per-field
//!      errors (the SPA's A157 inline-error renderer consumes this).
//!   4. **list + get** — two creates, then list returns both
//!      ordered by `name` ASC; `?search=` filters case-insensitive
//!      prefix; get-by-id round-trips every field; unknown id surfaces
//!      `NotFound`.
//!   5. **update** — mutate a single field; re-fetch sees the new
//!      value and a bumped `updated_at`; unknown id surfaces
//!      `NotFound`.
//!   6. **soft-delete** — delete returns Ok; get/list omit the row;
//!      re-delete surfaces `NotFound` (defence against a double-click
//!      DELETE silently succeeding the second time).

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_billing::Currency;
use ulid::Ulid;

use aberp::products::{NavUnitOfMeasure, ProductInputs, ProductUnit};
use aberp::serve::{self, AppState, ProductRouteError};

const TEST_TENANT: &str = "serve_products_route_test";

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-products")
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

fn nav_day_inputs(name: &str) -> ProductInputs {
    ProductInputs {
        name: name.to_string(),
        unit: ProductUnit::Nav(NavUnitOfMeasure::Day),
        currency: Currency::Huf,
        unit_price_minor: 250_000,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pins
// ──────────────────────────────────────────────────────────────────────

#[test]
fn products_create_happy_path_nav_unit() {
    let dir = test_dir("create-nav");
    let state = build_state(dir.join("aberp.duckdb"));

    let p = serve::create_product_request(&state, &nav_day_inputs("Tanácsadói nap"))
        .expect("create happy path must succeed");

    assert!(
        p.id.starts_with("prd_"),
        "product id `{}` must be prefixed-ULID",
        p.id
    );
    assert_eq!(p.id.len(), 30, "prefixed ProductId must be 30 chars");
    assert_eq!(p.name, "Tanácsadói nap");
    assert_eq!(p.unit, ProductUnit::Nav(NavUnitOfMeasure::Day));
    assert_eq!(p.currency, Currency::Huf);
    assert_eq!(p.unit_price_minor, 250_000);
    assert_eq!(p.created_at, p.updated_at);
    assert!(p.deleted_at.is_none());

    let _keep = &dir;
}

#[test]
fn products_create_happy_path_own_unit_liter_at_15c() {
    // The canonical OWN case — temperature-corrected litre. NAV has
    // plain LITER but no temperature-corrected variant; the SPA's
    // dropdown surfaces "Egyéb (Own)" with a free-text label, the
    // backend persists it as ProductUnit::Own("liter@15C"), and the
    // future NAV emitter pairs it with the OWN token + the free-text
    // unitOfMeasureOwn element. See ADR-0046.
    let dir = test_dir("create-own");
    let state = build_state(dir.join("aberp.duckdb"));
    let inputs = ProductInputs {
        name: "Gázolaj".to_string(),
        unit: ProductUnit::Own("liter@15C".to_string()),
        currency: Currency::Huf,
        unit_price_minor: 650,
    };

    let p = serve::create_product_request(&state, &inputs).expect("create Own must succeed");
    assert_eq!(p.unit, ProductUnit::Own("liter@15C".to_string()));

    let fetched = serve::get_product_request(&state, &p.id).expect("get must round-trip Own unit");
    assert_eq!(fetched.unit, ProductUnit::Own("liter@15C".to_string()));

    let _keep = &dir;
}

#[test]
fn products_create_rejects_invalid_inputs_with_structured_errors() {
    let dir = test_dir("create-invalid");
    let state = build_state(dir.join("aberp.duckdb"));
    let bad = ProductInputs {
        name: "   ".to_string(),
        unit: ProductUnit::Own("   ".to_string()),
        currency: Currency::Eur,
        unit_price_minor: -1,
    };

    let err = serve::create_product_request(&state, &bad).expect_err("invalid inputs must reject");
    let errors = match err {
        ProductRouteError::Validation(v) => v,
        other => panic!("expected Validation, got {other:?}"),
    };
    let fields: Vec<&str> = errors.iter().map(|e| e.field).collect();
    assert!(fields.contains(&"name"), "must flag name; got {:?}", fields);
    assert!(
        fields.contains(&"unit"),
        "must flag empty Own label; got {:?}",
        fields
    );
    assert!(
        fields.contains(&"unit_price_minor"),
        "must flag negative price; got {:?}",
        fields
    );

    let _keep = &dir;
}

#[test]
fn products_list_orders_by_name_search_filters_prefix() {
    let dir = test_dir("list");
    let state = build_state(dir.join("aberp.duckdb"));

    serve::create_product_request(&state, &nav_day_inputs("Zeta")).expect("create Zeta");
    serve::create_product_request(&state, &nav_day_inputs("Alpha")).expect("create Alpha");

    let listed = serve::list_products_request(&state, None).expect("list must succeed");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].name, "Alpha", "list must order by name ASC");
    assert_eq!(listed[1].name, "Zeta");

    let filtered = serve::list_products_request(&state, Some("al")).expect("search must succeed");
    assert_eq!(filtered.len(), 1, "?search=al must match Alpha only");
    assert_eq!(filtered[0].name, "Alpha");

    let created = serve::create_product_request(&state, &nav_day_inputs("Beta")).expect("create");
    let fetched = serve::get_product_request(&state, &created.id).expect("get");
    assert_eq!(fetched, created);

    let unknown = format!("prd_{}", Ulid::new());
    match serve::get_product_request(&state, &unknown) {
        Err(ProductRouteError::NotFound) => {}
        other => panic!("expected NotFound for unknown id, got {other:?}"),
    }

    let _keep = &dir;
}

#[test]
fn products_update_persists_mutation_and_bumps_updated_at() {
    let dir = test_dir("update");
    let state = build_state(dir.join("aberp.duckdb"));
    let created =
        serve::create_product_request(&state, &nav_day_inputs("Original")).expect("create");

    std::thread::sleep(std::time::Duration::from_millis(2));

    let mutated = ProductInputs {
        name: "Renamed".to_string(),
        unit: ProductUnit::Nav(NavUnitOfMeasure::Hour),
        currency: Currency::Eur,
        unit_price_minor: 199,
    };
    let updated =
        serve::update_product_request(&state, &created.id, &mutated).expect("update must succeed");

    assert_eq!(updated.id, created.id);
    assert_eq!(updated.name, "Renamed");
    assert_eq!(updated.unit, ProductUnit::Nav(NavUnitOfMeasure::Hour));
    assert_eq!(updated.currency, Currency::Eur);
    assert_eq!(updated.unit_price_minor, 199);
    assert_eq!(updated.created_at, created.created_at);
    assert_ne!(updated.updated_at, created.updated_at);

    let unknown = format!("prd_{}", Ulid::new());
    match serve::update_product_request(&state, &unknown, &mutated) {
        Err(ProductRouteError::NotFound) => {}
        other => panic!("expected NotFound on unknown id, got {other:?}"),
    }

    let _keep = &dir;
}

#[test]
fn products_soft_delete_makes_product_invisible_to_api() {
    let dir = test_dir("delete");
    let state = build_state(dir.join("aberp.duckdb"));
    let created =
        serve::create_product_request(&state, &nav_day_inputs("ToDelete")).expect("create");

    serve::delete_product_request(&state, &created.id).expect("delete must succeed");

    match serve::get_product_request(&state, &created.id) {
        Err(ProductRouteError::NotFound) => {}
        other => panic!("expected NotFound after soft-delete, got {other:?}"),
    }

    let listed = serve::list_products_request(&state, None).expect("list");
    assert!(
        listed.is_empty(),
        "soft-deleted product must not appear in list; got {:?}",
        listed
    );

    // Re-delete surfaces NotFound — defence against an SPA double-click
    // DELETE silently succeeding the second time.
    match serve::delete_product_request(&state, &created.id) {
        Err(ProductRouteError::NotFound) => {}
        other => panic!("expected NotFound on re-delete, got {other:?}"),
    }

    let _keep = &dir;
}
