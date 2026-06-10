//! Integration tests for the S231 / PR-227 / ADR-0061 inventory routes.
//!
//! Mirrors `serve_products_route.rs` in shape — exercises the
//! library-helper boundary against an in-process DuckDB file under a
//! per-test scratch directory. The HTTPS listener is NOT spun.
//!
//! Pins:
//!   1. **happy path** — POST a manual Adjustment, then GET the
//!      ledger; the response carries one row with the operator's
//!      attribution + reason + qty_delta. The product's
//!      `ProductWithInventory` payload reads the cache back.
//!   2. **reason-sign matrix** — POST a Receipt with negative qty
//!      surfaces 400 BadInput (the route layer maps
//!      `InventoryError::WrongSignForReason`).
//!   3. **upstream-only reasons refused at the route** — POST a
//!      `bom_consumption` is refused at the SPA boundary; only
//!      Receipt / Adjustment / Scrap pass through the manual form.
//!   4. **low-stock virtual view** — products below `min_stock` appear
//!      in `list_low_stock_products_request`.
//!   5. **idempotency** — repeat POST with the same key surfaces 409.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_billing::Currency;
use ulid::Ulid;

use aberp::products::{NavUnitOfMeasure, ProductInputs, ProductUnit};
use aberp::serve::{self, AppState, CreateStockMovementInputs, StockMovementRouteError};

const TEST_TENANT: &str = "serve_stock_movements_route_test";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-stock-movements")
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

fn ensure_inventory_schema(db_path: &std::path::Path) {
    let conn = duckdb::Connection::open(db_path).expect("open tenant DuckDB");
    // products::ensure_schema runs implicitly when create_product
    // lands its first INSERT; the inventory migration ALTER TABLEs
    // products, so it must run AFTER products exists.
    aberp::products::ensure_schema(&conn).expect("ensure products schema");
    aberp_inventory::ensure_schema(&conn).expect("ensure inventory schema");
    aberp_audit_ledger::ensure_schema(&conn).expect("ensure audit-ledger schema");
}

fn nav_pieces(name: &str) -> ProductInputs {
    ProductInputs {
        name: name.to_string(),
        unit: ProductUnit::Nav(NavUnitOfMeasure::Piece),
        currency: Currency::Huf,
        unit_price_minor: 0,
    }
}

fn create_with_min_stock(state: &AppState, name: &str, min_stock: &str) -> String {
    let p = serve::create_product_request(state, &nav_pieces(name)).expect("create product");
    let conn = duckdb::Connection::open(&*state.db_path).unwrap();
    conn.execute(
        "UPDATE products SET min_stock = ? WHERE id = ? AND tenant_id = ?;",
        duckdb::params![min_stock, &p.id, TEST_TENANT],
    )
    .unwrap();
    p.id
}

// ──────────────────────────────────────────────────────────────────────
// Pin 1 — happy path
// ──────────────────────────────────────────────────────────────────────

#[test]
fn post_then_list_stock_movements_round_trips() {
    let dir = test_dir("happy");
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    ensure_inventory_schema(&db_path);
    let pid = create_with_min_stock(&state, "Widget", "5");

    serve::create_stock_movement_request(
        &state,
        &pid,
        "test-operator",
        &CreateStockMovementInputs {
            qty_delta: "10".to_string(),
            reason: "receipt".to_string(),
            idempotency_key: "idem-receipt".to_string(),
            notes: Some("first GRN".to_string()),
        },
    )
    .expect("Receipt of 10 must succeed");

    let listed =
        serve::list_stock_movements_request(&state, &pid, 50, 0).expect("list must succeed");
    assert_eq!(listed.len(), 1);
    // Compare via Decimal value, not string — the DB serialises
    // DECIMAL(18,6) with trailing zeros ("10.000000") while the SPA
    // wire shape carries the parsed value.
    use rust_decimal::Decimal;
    use std::str::FromStr;
    assert_eq!(listed[0].qty_delta, Decimal::from_str("10").unwrap());
    assert_eq!(listed[0].operator, "test-operator");
    assert_eq!(listed[0].notes.as_deref(), Some("first GRN"));

    // ProductWithInventory shows the cached SUM + is_low_stock = false
    // (10 > min_stock 5).
    let pw = serve::get_product_with_inventory_request(&state, &pid).unwrap();
    assert_eq!(pw.stock_qty, Decimal::from_str("10").unwrap());
    assert_eq!(pw.min_stock, Decimal::from_str("5").unwrap());
    assert!(!pw.is_low_stock);
}

// ──────────────────────────────────────────────────────────────────────
// Pin 2 — reason-sign matrix at the boundary
// ──────────────────────────────────────────────────────────────────────

#[test]
fn post_refuses_wrong_sign_per_reason() {
    let dir = test_dir("wrong-sign");
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    ensure_inventory_schema(&db_path);
    let pid = create_with_min_stock(&state, "BadSign", "0");

    let err = serve::create_stock_movement_request(
        &state,
        &pid,
        "test-operator",
        &CreateStockMovementInputs {
            qty_delta: "-5".to_string(),
            reason: "receipt".to_string(),
            idempotency_key: "i-1".to_string(),
            notes: None,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, StockMovementRouteError::BadInput(_)),
        "Receipt with negative qty must surface BadInput, got {err:?}"
    );

    // Same on the wire-form side: malformed decimal is also BadInput
    // (not Other) — the route layer parses the wire shape first.
    let err = serve::create_stock_movement_request(
        &state,
        &pid,
        "test-operator",
        &CreateStockMovementInputs {
            qty_delta: "not-a-number".to_string(),
            reason: "adjustment".to_string(),
            idempotency_key: "i-2".to_string(),
            notes: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, StockMovementRouteError::BadInput(_)));
}

// ──────────────────────────────────────────────────────────────────────
// Pin 3 — upstream-only reasons refused at the manual form
// ──────────────────────────────────────────────────────────────────────

#[test]
fn post_refuses_upstream_only_reasons() {
    let dir = test_dir("upstream");
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    ensure_inventory_schema(&db_path);
    let pid = create_with_min_stock(&state, "Upstream", "0");

    for upstream in ["bom_consumption", "wo_completion", "dispatch"] {
        let err = serve::create_stock_movement_request(
            &state,
            &pid,
            "test-operator",
            &CreateStockMovementInputs {
                qty_delta: "-1".to_string(),
                reason: upstream.to_string(),
                idempotency_key: format!("i-{upstream}"),
                notes: None,
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, StockMovementRouteError::BadInput(_)),
            "{upstream} must be refused at the SPA boundary, got {err:?}"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pin 4 — low-stock virtual view
// ──────────────────────────────────────────────────────────────────────

#[test]
fn low_stock_view_surfaces_products_below_min() {
    let dir = test_dir("low-stock");
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    ensure_inventory_schema(&db_path);

    let below = create_with_min_stock(&state, "Below", "10");
    let _above = create_with_min_stock(&state, "Above", "1");
    serve::create_stock_movement_request(
        &state,
        &_above,
        "test-operator",
        &CreateStockMovementInputs {
            qty_delta: "5".to_string(),
            reason: "receipt".to_string(),
            idempotency_key: "i-above".to_string(),
            notes: None,
        },
    )
    .unwrap();

    let rows = serve::list_low_stock_products_request(&state).unwrap();
    let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["Below"], "only the below-min product surfaces");
    assert_eq!(rows[0].product_id, below);
}

// ──────────────────────────────────────────────────────────────────────
// Pin 5 — idempotency
// ──────────────────────────────────────────────────────────────────────

#[test]
fn post_with_duplicate_idempotency_key_surfaces_conflict() {
    let dir = test_dir("idem");
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    ensure_inventory_schema(&db_path);
    let pid = create_with_min_stock(&state, "Idem", "0");

    serve::create_stock_movement_request(
        &state,
        &pid,
        "test-operator",
        &CreateStockMovementInputs {
            qty_delta: "1".to_string(),
            reason: "receipt".to_string(),
            idempotency_key: "shared".to_string(),
            notes: None,
        },
    )
    .unwrap();

    let err = serve::create_stock_movement_request(
        &state,
        &pid,
        "test-operator",
        &CreateStockMovementInputs {
            qty_delta: "1".to_string(),
            reason: "receipt".to_string(),
            idempotency_key: "shared".to_string(),
            notes: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, StockMovementRouteError::Conflict(_)));

    // The conflict did NOT add a second row.
    let listed = serve::list_stock_movements_request(&state, &pid, 50, 0).unwrap();
    assert_eq!(listed.len(), 1);
}
