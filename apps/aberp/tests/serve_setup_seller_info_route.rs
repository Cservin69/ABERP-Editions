//! Integration tests for the PR-51 / session-71 seller-info setup
//! route + the `NeedsSellerConfig` boot-state machinery.
//!
//! Pin coverage:
//!
//! 1. **Happy path** — POSTing a complete `SellerInfoInputs` writes
//!    `~/.aberp/<tenant>/seller.toml` (via per-test tempfile
//!    override) AND flips boot state from `NeedsSellerConfig` to
//!    `Ready` with the carried `operator_login` preserved.
//! 2. **Validation failure → field-level 400 body** — invalid tax
//!    number surfaces as `SetupSellerRouteError::Validation` with the
//!    field name `taxNumber` so the SPA can highlight the offending
//!    input. The file MUST NOT be created on validation failure.
//! 3. **Multi-field validation** — blank legal name + blank city
//!    surface together in one response so the operator fixes them in
//!    one round-trip instead of discovering them serially.
//! 4. **Existing file overwritten atomically** — POSTing twice with
//!    different identities leaves only the second body on disk; the
//!    file's contents are exactly what was last written.
//! 5. **Parent dir auto-created** — pointing the override at a path
//!    whose parent does not yet exist must succeed (the helper
//!    mkdirs and chmods to 0700 before the rename).
//! 6. **Bank-only legacy parser round-trips** — the bank fields on
//!    the written file are readable by
//!    `print_invoice::parse_seller_toml` so the PDF render path
//!    keeps working post-wizard.
//! 7. **Boot-state token contract** — the new
//!    `NeedsSellerConfig` variant emits `needs-seller-config` as its
//!    handshake state token. (One pin in the unit-level
//!    `serve_setup_nav_credentials_route.rs` file too — this one is
//!    defence-in-depth so the contract surfaces from EITHER
//!    integration binary if a future rename drifts it.)

#![allow(clippy::too_many_arguments)]

use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, TenantId};
use ulid::Ulid;

use aberp::serve::{self, AppState, ServeBootState, SetupSellerRouteError};
use aberp::setup_seller_info::SellerInfoInputs;

fn unique_tenant(label: &str) -> String {
    format!("seller-route-{label}-{}", Ulid::new())
}

fn build_state(boot_state: ServeBootState, tenant: &str) -> AppState {
    let tenant_id = TenantId::new(tenant.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    let db_path = std::env::temp_dir().join(format!("aberp-seller-{}.duckdb", Ulid::new()));
    AppState {
        db_path: Arc::new(db_path),
        tenant: tenant_id,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        boot_state: Arc::new(std::sync::RwLock::new(boot_state)),
        nav_poll_semaphore: Arc::new(tokio::sync::Semaphore::new(
            aberp::serve::NAV_POLL_DAEMON_CONCURRENCY,
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

fn good_inputs() -> SellerInfoInputs {
    SellerInfoInputs {
        legal_name: "Áben Consulting KFT.".to_string(),
        tax_number: "24904362-2-41".to_string(),
        eu_vat_number: Some("HU24904362".to_string()),
        address_country_code: "HU".to_string(),
        address_postal_code: "1037".to_string(),
        address_city: "Budapest".to_string(),
        address_street: "Visszatérő köz 6".to_string(),
        bank_account_number: Some("12345678-12345678-12345678".to_string()),
        iban: Some("LT14 3250 0448 1318 6860".to_string()),
        bank_name: Some("Revolut".to_string()),
        swift_bic: Some("REVOLT21".to_string()),
    }
}

/// Per-test tempfile path. `Ulid` keeps parallel test binaries +
/// parallel test cases isolated.
fn temp_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("aberp-seller-info-{label}-{}.toml", Ulid::new()))
}

/// Test 1 — happy path: route helper writes the file AND flips boot
/// state to Ready with `operator_login` preserved across the
/// transition.
#[test]
fn route_happy_path_writes_file_and_flips_boot_state() {
    let tenant = unique_tenant("happy");
    let state = build_state(
        ServeBootState::NeedsSellerConfig {
            operator_login: "op-from-keychain".to_string(),
        },
        &tenant,
    );
    let path = temp_path("happy");

    let inputs = good_inputs();
    serve::setup_seller_info_request(&state, &inputs, Some(&path))
        .expect("happy path must succeed");

    assert!(
        path.exists(),
        "seller.toml must exist at {}",
        path.display()
    );
    let body = std::fs::read_to_string(&path).expect("read seller.toml");
    assert!(
        body.contains("legal_name = \"Áben Consulting KFT.\""),
        "body: {body}"
    );
    assert!(
        body.contains("tax_number = \"24904362-2-41\""),
        "body: {body}"
    );
    assert!(body.contains("country_code = \"HU\""), "body: {body}");

    let guard = state.boot_state.read().unwrap();
    match &*guard {
        ServeBootState::Ready { operator_login } => {
            assert_eq!(operator_login, "op-from-keychain");
        }
        other => panic!("expected Ready, got {other:?}"),
    }

    let _ = std::fs::remove_file(&path);
}

/// Test 2 — invalid tax number: 400 with `taxNumber` field error;
/// file MUST NOT be created.
#[test]
fn route_rejects_invalid_tax_number_with_field_error_no_partial_write() {
    let tenant = unique_tenant("bad-tax");
    let state = build_state(
        ServeBootState::NeedsSellerConfig {
            operator_login: "op".to_string(),
        },
        &tenant,
    );
    let path = temp_path("bad-tax");

    let mut inputs = good_inputs();
    inputs.tax_number = "24904362".to_string(); // bare 8-digit

    let err = serve::setup_seller_info_request(&state, &inputs, Some(&path))
        .expect_err("malformed tax must fail");
    match err {
        SetupSellerRouteError::Validation(fields) => {
            let tax = fields
                .iter()
                .find(|fe| fe.field == "taxNumber")
                .expect("taxNumber field error present");
            assert!(
                tax.message.contains("not a valid Hungarian"),
                "message must surface ADÓSZÁM hint: {}",
                tax.message
            );
        }
        SetupSellerRouteError::Other(e) => panic!("expected Validation, got Other({e:#})"),
    }

    assert!(!path.exists(), "no file on validation failure");
    let guard = state.boot_state.read().unwrap();
    assert!(matches!(&*guard, ServeBootState::NeedsSellerConfig { .. }));
}

/// Test 3 — multi-field validation: blank legal name + blank city
/// surface together in one response body.
#[test]
fn route_collects_all_field_errors_at_once() {
    let tenant = unique_tenant("multi");
    let state = build_state(
        ServeBootState::NeedsSellerConfig {
            operator_login: "op".to_string(),
        },
        &tenant,
    );
    let path = temp_path("multi");

    let mut inputs = good_inputs();
    inputs.legal_name = String::new();
    inputs.address_city = "  ".to_string();

    let err = serve::setup_seller_info_request(&state, &inputs, Some(&path))
        .expect_err("multi-field invalid must fail");
    match err {
        SetupSellerRouteError::Validation(fields) => {
            let names: Vec<&str> = fields.iter().map(|fe| fe.field).collect();
            assert!(names.contains(&"legalName"), "names: {names:?}");
            assert!(names.contains(&"addressCity"), "names: {names:?}");
        }
        SetupSellerRouteError::Other(e) => panic!("expected Validation, got Other({e:#})"),
    }
    assert!(!path.exists());
}

/// Test 4 — atomic overwrite of an existing file.
#[test]
fn route_overwrites_existing_file_atomically() {
    let tenant = unique_tenant("overwrite");
    let state = build_state(
        ServeBootState::NeedsSellerConfig {
            operator_login: "op".to_string(),
        },
        &tenant,
    );
    let path = temp_path("overwrite");

    // First write — vanilla good inputs.
    let inputs1 = good_inputs();
    serve::setup_seller_info_request(&state, &inputs1, Some(&path))
        .expect("first write must succeed");
    let body1 = std::fs::read_to_string(&path).unwrap();
    assert!(body1.contains("Áben Consulting"), "first body: {body1}");

    // Reset boot state so the second call's transition succeeds.
    *state.boot_state.write().unwrap() = ServeBootState::NeedsSellerConfig {
        operator_login: "op".to_string(),
    };

    // Second write — different identity.
    let mut inputs2 = good_inputs();
    inputs2.legal_name = "Other Company LTD.".to_string();
    serve::setup_seller_info_request(&state, &inputs2, Some(&path))
        .expect("overwrite must succeed");
    let body2 = std::fs::read_to_string(&path).unwrap();
    assert!(body2.contains("Other Company"), "second body: {body2}");
    assert!(
        !body2.contains("Áben Consulting"),
        "first body must be fully replaced: {body2}"
    );

    let _ = std::fs::remove_file(&path);
}

/// Test 5 — parent dir auto-created. Pointing the override at
/// `<tempdir>/parent-{ulid}/seller.toml` must succeed even though the
/// `parent-{ulid}` dir does not exist beforehand.
#[test]
fn route_auto_creates_missing_parent_dir() {
    let tenant = unique_tenant("mkdir");
    let state = build_state(
        ServeBootState::NeedsSellerConfig {
            operator_login: "op".to_string(),
        },
        &tenant,
    );
    let parent = std::env::temp_dir().join(format!("aberp-seller-parent-{}", Ulid::new()));
    let path = parent.join("seller.toml");
    assert!(!parent.exists(), "precondition: parent must not exist");

    serve::setup_seller_info_request(&state, &good_inputs(), Some(&path))
        .expect("mkdir + write must succeed");
    assert!(path.exists(), "file must exist after auto-mkdir");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&parent);
}

/// Test 6 — bank-only legacy parser still reads the bank block from
/// a wizard-written file. The PDF renderer keeps working after PR-51
/// rewrites the file via the wizard.
#[test]
fn legacy_parse_seller_toml_round_trips_bank_fields_from_wizard_write() {
    let tenant = unique_tenant("legacy-bank");
    let state = build_state(
        ServeBootState::NeedsSellerConfig {
            operator_login: "op".to_string(),
        },
        &tenant,
    );
    let path = temp_path("legacy-bank");

    let inputs = good_inputs();
    serve::setup_seller_info_request(&state, &inputs, Some(&path)).expect("write must succeed");
    let body = std::fs::read_to_string(&path).unwrap();

    let bank = aberp::print_invoice::parse_seller_toml(&body).expect("legacy parses");
    assert_eq!(
        bank.bank_account_number.as_deref(),
        Some("12345678-12345678-12345678"),
    );
    assert_eq!(bank.iban.as_deref(), Some("LT14 3250 0448 1318 6860"));
    assert_eq!(bank.bank_name.as_deref(), Some("Revolut"));
    assert_eq!(bank.swift_bic.as_deref(), Some("REVOLT21"));

    let _ = std::fs::remove_file(&path);
}

/// Test 7 — handshake state-token contract for the new variant.
/// Defence-in-depth duplicate of the pin in
/// `serve_setup_nav_credentials_route.rs` so EITHER integration
/// binary catches a future rename drift.
#[test]
fn boot_state_token_for_needs_seller_config() {
    assert_eq!(
        ServeBootState::NeedsSellerConfig {
            operator_login: "x".to_string()
        }
        .state_token(),
        "needs-seller-config"
    );
}

/// Test 8 — bypass-bearer contract: NeedsSetup AND NeedsSellerConfig
/// BOTH allow bypass; Ready does not. The route handler keys on this
/// to skip the bearer-auth check while a wizard chain is in flight.
#[test]
fn allows_setup_bypass_covers_both_pre_ready_variants() {
    assert!(ServeBootState::NeedsSetup.allows_setup_bypass());
    assert!(ServeBootState::NeedsSellerConfig {
        operator_login: "x".to_string()
    }
    .allows_setup_bypass());
    assert!(!ServeBootState::Ready {
        operator_login: "x".to_string()
    }
    .allows_setup_bypass());
}
