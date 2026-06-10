//! Integration tests for the PR-53 / session-73 settings routes:
//!
//!   - `GET /api/seller-info` — read-side counterpart of the wizard.
//!   - `GET /api/nav-credentials-status` — presence flags + login
//!     value.
//!   - `POST /api/rotate-nav-credential` — single-slot rotation.
//!
//! All three routes are Ready-gated + bearer-required (the wizard
//! chain handles the first-run / pre-Ready surface). These tests
//! target the public library helpers per A158 — they don't spin the
//! HTTPS listener.
//!
//! The keychain-touching tests use the same shared in-process mock
//! credential backend the `serve_setup_nav_credentials_route.rs`
//! file uses, replicated locally because:
//!   1. Each integration test file is a separate test binary, so the
//!      process-global `set_default_credential_builder` registration
//!      is per-binary.
//!   2. The `keyring::mock` default backend creates a fresh
//!      `MockCredential` per `Entry::new` — set-then-get returns
//!      `NoEntry` (see `feedback_keyring_mock_per_entry_isolation`).
//!
//! Path-override pattern per session-58 / A158: each test uses a
//! per-test scratch dir + HOME redirect so the per-tenant
//! `~/.aberp/<tenant>/seller.toml` lives in the test scratch
//! filesystem.

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once, OnceLock};

use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_nav_transport::credentials::keychain::write_blob;
use keyring::credential::{Credential, CredentialApi, CredentialBuilderApi, CredentialPersistence};
use keyring::Error as KeyringError;
use ulid::Ulid;

use aberp::serve::{AppState, RotateNavCredentialError, ServeBootState};

// ──────────────────────────────────────────────────────────────────────
// Shared in-process mock keychain backend (mirror of session-62's
// pattern in serve_setup_nav_credentials_route.rs)
// ──────────────────────────────────────────────────────────────────────

fn shared_store() -> &'static Mutex<HashMap<(String, String), String>> {
    static STORE: OnceLock<Mutex<HashMap<(String, String), String>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug)]
struct SharedMockCredential {
    service: String,
    account: String,
}

impl CredentialApi for SharedMockCredential {
    fn set_password(&self, password: &str) -> keyring::Result<()> {
        shared_store()
            .lock()
            .expect("shared mock store poisoned")
            .insert(
                (self.service.clone(), self.account.clone()),
                password.to_string(),
            );
        Ok(())
    }

    fn get_password(&self) -> keyring::Result<String> {
        match shared_store()
            .lock()
            .expect("shared mock store poisoned")
            .get(&(self.service.clone(), self.account.clone()))
        {
            Some(p) => Ok(p.clone()),
            None => Err(KeyringError::NoEntry),
        }
    }

    fn delete_password(&self) -> keyring::Result<()> {
        let mut store = shared_store().lock().expect("shared mock store poisoned");
        if store
            .remove(&(self.service.clone(), self.account.clone()))
            .is_some()
        {
            Ok(())
        } else {
            Err(KeyringError::NoEntry)
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Debug)]
struct SharedMockCredentialBuilder;

impl CredentialBuilderApi for SharedMockCredentialBuilder {
    fn build(
        &self,
        _target: Option<&str>,
        service: &str,
        user: &str,
    ) -> keyring::Result<Box<Credential>> {
        Ok(Box::new(SharedMockCredential {
            service: service.to_string(),
            account: user.to_string(),
        }))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn persistence(&self) -> CredentialPersistence {
        CredentialPersistence::ProcessOnly
    }
}

static INIT_MOCK: Once = Once::new();

fn init_mock_keyring() {
    INIT_MOCK.call_once(|| {
        keyring::set_default_credential_builder(Box::new(SharedMockCredentialBuilder));
    });
}

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-settings")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn unique_tenant(label: &str) -> String {
    format!("settings_{}_{}", label, Ulid::new())
}

fn build_state_for(tenant: &str, db_path: PathBuf) -> AppState {
    let tenant_id = TenantId::new(tenant.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    AppState {
        db_path: Arc::new(db_path),
        tenant: tenant_id,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        nav_poll_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
            aberp::serve::NAV_POLL_DAEMON_CONCURRENCY,
        )),
        boot_state: Arc::new(std::sync::RwLock::new(ServeBootState::Ready {
            operator_login: "old-login".to_string(),
        })),
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

/// PR-57 / session-77 — write the consolidated NAV-credentials blob
/// for the given tenant. Mirrors what
/// `setup_credentials_from_inputs` writes in production: ONE
/// keychain item holding all four artifacts as JSON.
fn write_blob_for_tenant(
    tenant: &str,
    login: &str,
    password: &str,
    sign_key: &str,
    change_key: &str,
) {
    write_blob(tenant, login, password, sign_key, change_key)
        .expect("write_blob must succeed in tests");
}

/// Write a fixture seller.toml at the given path. Tests use the
/// path-explicit helper variants so they don't have to mutate HOME
/// under cargo's parallel test runner (per session-58's
/// `seller_toml_override` precedent).
fn write_fixture_seller_toml_at(path: &std::path::Path) {
    let parent = path.parent().expect("path has parent");
    std::fs::create_dir_all(parent).expect("create tenant dir");
    let body = r#"[seller]
legal_name = "ABERP Supplier Kft."
tax_number = "12345678-1-42"
eu_vat_number = "HU12345678"

[seller.address]
country_code = "HU"
postal_code = "1011"
city = "Budapest"
street = "Fő utca 1."

# Bank info
bank_account_number = "12345678-12345678-12345678"
iban = "HU12345678901234567890"
bank_name = "OTP Bank"
swift_bic = "OTPVHUHB"
"#;
    std::fs::write(path, body).expect("write seller.toml fixture");
}

// ──────────────────────────────────────────────────────────────────────
// Tests for `supplier_from_seller_toml` (PR-53 cross-cutting fix)
// ──────────────────────────────────────────────────────────────────────

/// Happy path — the helper reads the on-disk seller.toml and emits a
/// `SupplierJson` ready for `issue_from_parsed`.
#[test]
fn supplier_from_seller_toml_happy_path() {
    let dir = test_dir("happy");
    let path = dir.join("seller.toml");
    write_fixture_seller_toml_at(&path);

    let supplier =
        aberp::serve::supplier_from_seller_toml_path(&path).expect("happy path must read fixture");
    assert_eq!(supplier.tax_number, "12345678-1-42");
    assert_eq!(supplier.name, "ABERP Supplier Kft.");
    assert_eq!(supplier.address.country_code, "HU");
    assert_eq!(supplier.address.city, "Budapest");
}

/// Missing-file branch — when no seller.toml exists, the helper
/// surfaces the typed `SupplierConfigError::MissingTaxNumber` so the
/// route layer can dress it up as the `missing_seller_config` 400.
#[test]
fn supplier_from_seller_toml_missing_file_maps_to_missing_tax_number() {
    let dir = test_dir("missing");
    let path = dir.join("seller.toml"); // not written

    let err = aberp::serve::supplier_from_seller_toml_path(&path)
        .expect_err("missing file must surface as a typed error");
    match err {
        aberp::nav_xml::SupplierConfigError::MissingTaxNumber => {}
        other => panic!("expected MissingTaxNumber, got {other:?}"),
    }
}

/// Malformed-tax-number branch — when seller.toml has a tax number
/// that doesn't match `xxxxxxxx-y-zz`, the helper surfaces
/// `MalformedTaxNumber`. A hand-edited seller.toml can land here
/// even though the wizard's submit gate would have rejected it.
#[test]
fn supplier_from_seller_toml_malformed_tax_maps_to_malformed_variant() {
    let dir = test_dir("malformed");
    let path = dir.join("seller.toml");
    // Tax number missing the `-y-zz` suffix — the wizard would have
    // rejected this submit, but a hand-edited file lands here.
    std::fs::write(
        &path,
        r#"[seller]
legal_name = "Bad Tax Kft."
tax_number = "24904362"
[seller.address]
country_code = "HU"
postal_code = "1011"
city = "Budapest"
street = "Fő utca 1."
"#,
    )
    .expect("write");

    let err = aberp::serve::supplier_from_seller_toml_path(&path)
        .expect_err("malformed tax number must surface as typed error");
    match err {
        aberp::nav_xml::SupplierConfigError::MalformedTaxNumber { .. } => {}
        other => panic!("expected MalformedTaxNumber, got {other:?}"),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests for the rotate-credential route helper
// ──────────────────────────────────────────────────────────────────────

/// Happy-path rotation — set the consolidated NAV-credentials blob
/// (PR-57) for the tenant, invoke the rotate helper on one slot, and
/// re-read the blob to confirm only the target slot changed.
#[test]
fn rotate_nav_credential_updates_single_slot() {
    init_mock_keyring();
    let dir = test_dir("rotate");
    let tenant = unique_tenant("rotate");

    write_blob_for_tenant(&tenant, "old-login", "old-pass", "old-sign", "old-change");

    let state = build_state_for(&tenant, dir.join("aberp.duckdb"));
    let response =
        aberp::serve::rotate_nav_credential_request(&state, "password", "new-pass-value")
            .expect("rotation must succeed");
    assert_eq!(response, "password");

    // PR-57 — re-read via the public load helper. The blob round-trip
    // must show only the rotated slot changed; the other three remain
    // verbatim.
    let creds = aberp_nav_transport::credentials::NavCredentials::load_from_keychain(&tenant)
        .expect("blob must round-trip after rotation");
    assert_eq!(creds.login(), "old-login");
    assert_eq!(creds.password_bytes(), b"new-pass-value");
    assert_eq!(creds.sign_key_bytes(), b"old-sign");
    assert_eq!(creds.change_key_bytes(), b"old-change");
}

/// Login-slot rotation flips the in-process `operator_login` so
/// subsequent issuance routes derive the audit actor from the new
/// value without a backend restart.
#[test]
fn rotate_nav_credential_login_refreshes_operator_login() {
    init_mock_keyring();
    let dir = test_dir("rotate-login");
    let tenant = unique_tenant("rotate-login");
    write_blob_for_tenant(&tenant, "old-login", "x", "x", "x");

    let state = build_state_for(&tenant, dir.join("aberp.duckdb"));
    aberp::serve::rotate_nav_credential_request(&state, "login", "new-operator")
        .expect("login rotation must succeed");

    let guard = state.boot_state.read().expect("read boot_state");
    match &*guard {
        ServeBootState::Ready { operator_login } => {
            assert_eq!(operator_login, "new-operator");
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

/// Rotation with an unknown item slug returns a typed validation
/// error so the HTTP route surfaces 400.
#[test]
fn rotate_nav_credential_rejects_unknown_item_slug() {
    init_mock_keyring();
    let dir = test_dir("rotate-bad-slug");
    let tenant = unique_tenant("rotate-bad-slug");
    let state = build_state_for(&tenant, dir.join("aberp.duckdb"));

    let err = aberp::serve::rotate_nav_credential_request(&state, "not-a-slug", "x")
        .expect_err("unknown slug must reject");
    match err {
        RotateNavCredentialError::Validation(msg) => {
            assert!(
                msg.contains("not-a-slug"),
                "validation message must echo the rejected slug, got: {msg}"
            );
        }
        other => panic!("expected Validation arm, got {other:?}"),
    }
}

/// Rotation with an empty new_value rejects — the keychain shouldn't
/// land in the "present but blank" state which would surface as a
/// confusing downstream `KeychainItemMissing` later.
#[test]
fn rotate_nav_credential_rejects_empty_value() {
    init_mock_keyring();
    let dir = test_dir("rotate-empty");
    let tenant = unique_tenant("rotate-empty");
    let state = build_state_for(&tenant, dir.join("aberp.duckdb"));

    let err = aberp::serve::rotate_nav_credential_request(&state, "password", "")
        .expect_err("empty new_value must reject");
    match err {
        RotateNavCredentialError::Validation(msg) => {
            assert!(
                msg.contains("empty"),
                "validation message must mention empty value, got: {msg}"
            );
        }
        other => panic!("expected Validation arm, got {other:?}"),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests for the nav-credentials-status read-side helper
// ──────────────────────────────────────────────────────────────────────

/// PR-57 / session-77 — status helper reads the consolidated blob and
/// surfaces presence (all-true when the blob is populated; the four
/// fields move together under the blob model) + the operator-visible
/// login value verbatim.
#[test]
fn nav_credentials_status_reports_presence_and_login_value() {
    init_mock_keyring();
    let dir = test_dir("status");
    let tenant = unique_tenant("status");
    write_blob_for_tenant(&tenant, "visible-login", "secret-pass", "sk", "ck");

    let state = build_state_for(&tenant, dir.join("aberp.duckdb"));
    let status =
        aberp::serve::nav_credentials_status_request(&state).expect("status read must succeed");

    assert!(status.login, "login slot must report present");
    assert!(status.password, "password slot must report present");
    assert!(
        status.sign_key,
        "sign_key must report present (blob carries all 4)"
    );
    assert!(
        status.change_key,
        "change_key must report present (blob carries all 4)"
    );
    assert_eq!(
        status.login_value.as_deref(),
        Some("visible-login"),
        "login value must be returned verbatim (it's not a secret)"
    );
}

/// PR-57 / session-77 — status helper reports all-false when the
/// blob is absent (NeedsSetup boot state, before the wizard fires).
#[test]
fn nav_credentials_status_reports_all_false_when_blob_absent() {
    init_mock_keyring();
    let dir = test_dir("status-empty");
    let tenant = unique_tenant("status-empty");
    // No blob write — keychain entry is absent for this unique tenant.

    let state = build_state_for(&tenant, dir.join("aberp.duckdb"));
    let status =
        aberp::serve::nav_credentials_status_request(&state).expect("status read must succeed");

    assert!(!status.login);
    assert!(!status.password);
    assert!(!status.sign_key);
    assert!(!status.change_key);
    assert_eq!(status.login_value, None);
}

// ──────────────────────────────────────────────────────────────────────
// Tests for the GET seller-info read-side helper
// ──────────────────────────────────────────────────────────────────────

#[test]
fn get_seller_info_returns_persisted_identity_and_bank() {
    let dir = test_dir("seller-info");
    let path = dir.join("seller.toml");
    write_fixture_seller_toml_at(&path);

    let body = aberp::serve::seller_info_request_from_path(&path)
        .expect("seller-info read must succeed")
        .expect("file present");
    assert_eq!(body.legal_name, "ABERP Supplier Kft.");
    assert_eq!(body.tax_number, "12345678-1-42");
    assert_eq!(body.eu_vat_number.as_deref(), Some("HU12345678"));
    assert_eq!(body.address.country_code, "HU");
    assert_eq!(body.address.city, "Budapest");
    assert_eq!(
        body.bank.iban.as_deref(),
        Some("HU12345678901234567890"),
        "IBAN must round-trip through the bank-block reader"
    );
    assert_eq!(body.bank.name.as_deref(), Some("OTP Bank"));
}

#[test]
fn get_seller_info_returns_none_when_file_missing() {
    let dir = test_dir("seller-info-missing");
    let path = dir.join("seller.toml"); // not written

    let result =
        aberp::serve::seller_info_request_from_path(&path).expect("read attempt must not error");
    assert!(result.is_none(), "missing file must surface as Ok(None)");
}
