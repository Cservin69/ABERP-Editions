//! Integration tests for the PR-46α / session-62 first-run NAV
//! credentials setup route + boot-state machinery.
//!
//! Pin coverage:
//!
//! 1. **NeedsSetup boot state from a fresh keychain** — when the
//!    keychain is empty for the tenant, the
//!    [`ServeBootState::NeedsSetup`] discriminator drives the
//!    handshake `state=needs-setup` suffix and the 503 gate on every
//!    other route.
//! 2. **Setup route happy path** — POSTing the four credential inputs
//!    runs the shared `setup_credentials_from_inputs` core, re-loads
//!    NAV credentials from the keychain, and flips the boot state to
//!    `Ready { operator_login }`.
//! 3. **Validation failure → 400 surface** — the typed
//!    `SetupRouteHelperError::Validation` carries the operator-readable
//!    message verbatim. Per CLAUDE.md rule 9, one pin per validator
//!    branch would be tautological with the unit tests in
//!    `apps/aberp/src/setup_nav_credentials.rs`; we pin the
//!    propagation contract once.
//! 4. **Gated route in NeedsSetup → 503-eligible error** — the helper
//!    surfaces consulted by mutation routes (`submit_invoice_request`,
//!    `poll_ack_request`) loud-fail with an
//!    `anyhow`-wrapped "NeedsSetup state" message rather than
//!    proceeding silently. The HTTP layer's 503 mapping is verified
//!    via the unit test on `require_ready` in `serve.rs::tests`.
//! 5. **CLI / HTTP parity** — running the shared
//!    `setup_credentials_from_inputs` core via either entry point
//!    leaves identical keychain state.
//!
//! ## Keychain isolation
//!
//! These tests swap the process-global keychain backend to a custom
//! `SharedMockCredentialBuilder` (defined below) on first invocation
//! via `Once::call_once`. The default `keyring::mock` backend mints a
//! fresh `MockCredential` for every `Entry::new` call — set and get
//! land on DIFFERENT in-memory cells, so a write-then-read roundtrip
//! always returns `NoEntry`, which makes it unsuitable for these
//! tests. Our shared mock keys credentials by `(service, account)` in
//! a process-global `HashMap` so two `Entry::new` calls for the same
//! key share state. This is process-wide; this file is deliberately
//! the only integration-test binary that calls
//! `set_default_credential_builder`. Other serve tests construct
//! `ServeBootState::Ready { operator_login }` directly and never
//! reach for the keychain.

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once, OnceLock};

use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_nav_transport::credentials::keychain::{
    service_name, ITEM_CHANGE_KEY, ITEM_LOGIN, ITEM_PASSWORD, ITEM_SIGN_KEY,
};
use keyring::credential::{
    Credential, CredentialApi, CredentialBuilderApi, CredentialPersistence,
};
use keyring::Error as KeyringError;
use ulid::Ulid;

use aberp::serve::{self, AppState, ServeBootState, SetupRouteHelperError};
use aberp::setup_nav_credentials::{NavCredentialInputs, SetupCredentialsError};

// ── Shared in-process mock keychain backend ──────────────────────────
//
// The default `keyring::mock` backend creates a fresh `MockCredential`
// per `Entry::new`, so set-then-get on the same (service, account)
// returns `NoEntry`. Our shared mock keys entries in a process-global
// HashMap so subsequent Entry::new instances for the same key share
// state.

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

/// Swap to the shared in-process mock keychain backend ONCE per test
/// binary. Backend state is process-global; we scope per-test
/// isolation by using a unique tenant id per test (which produces a
/// unique `service_name`).
static INIT_MOCK: Once = Once::new();

fn init_mock_keyring() {
    INIT_MOCK.call_once(|| {
        keyring::set_default_credential_builder(Box::new(SharedMockCredentialBuilder));
    });
}

/// Generate a unique tenant id for this test so parallel tests in this
/// binary don't collide on the mock keychain. The mock backend keys
/// entries by (service, account), and `service_name(tenant)` includes
/// the tenant id; a unique ulid-derived tenant guarantees isolation.
fn unique_tenant(label: &str) -> String {
    format!("setup-route-{label}-{}", Ulid::new())
}

fn build_state(boot_state: ServeBootState, tenant: &str) -> AppState {
    let tenant_id = TenantId::new(tenant.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    // The db path doesn't matter for these tests — the setup route
    // doesn't open the ledger. We point at a scratch path under the
    // OS tempdir.
    let db_path = std::env::temp_dir().join(format!("aberp-setup-{}.duckdb", Ulid::new()));
    AppState {
        db_path: Arc::new(db_path),
        tenant: tenant_id,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
        session_token: Arc::new("test-token".to_string()),
        boot_state: Arc::new(std::sync::RwLock::new(boot_state)),
    }
}

fn fixture_inputs() -> NavCredentialInputs {
    NavCredentialInputs {
        technical_user_login: "techuser-test-01".to_string(),
        technical_user_password: "pw-very-strong".to_string(),
        xml_sign_key: "sk-aaaaaaaabbbbbbbbcccccccc".to_string(),
        xml_change_key: "ck-16-byte-blob1".to_string(),
    }
}

fn read_back(service: &str, item: &'static str) -> Option<String> {
    let entry = keyring::Entry::new(service, item).expect("build keyring::Entry");
    match entry.get_password() {
        Ok(s) => Some(s),
        Err(keyring::Error::NoEntry) => None,
        Err(e) => panic!("unexpected keychain error reading {item}: {e}"),
    }
}

/// PR-46α / session-62 — happy path: POSTing the four credential
/// inputs writes all four keychain entries AND flips the boot state
/// from NeedsSetup to Ready with operator_login extracted from the
/// just-written login entry.
#[test]
fn setup_route_happy_path_writes_keychain_and_flips_boot_state() {
    init_mock_keyring();
    let tenant = unique_tenant("happy");
    let state = build_state(ServeBootState::NeedsSetup, &tenant);

    let inputs = fixture_inputs();
    serve::setup_nav_credentials_request(&state, &inputs)
        .expect("happy path must succeed");

    // Per-field keychain verification: all four entries land verbatim.
    let service = service_name(&tenant);
    assert_eq!(
        read_back(&service, ITEM_LOGIN).as_deref(),
        Some(inputs.technical_user_login.as_str()),
    );
    assert_eq!(
        read_back(&service, ITEM_PASSWORD).as_deref(),
        Some(inputs.technical_user_password.as_str()),
    );
    assert_eq!(
        read_back(&service, ITEM_SIGN_KEY).as_deref(),
        Some(inputs.xml_sign_key.as_str()),
    );
    assert_eq!(
        read_back(&service, ITEM_CHANGE_KEY).as_deref(),
        Some(inputs.xml_change_key.as_str()),
    );

    // Boot state flipped to Ready with the login extracted from the
    // just-written keychain entry.
    let guard = state.boot_state.read().unwrap();
    match &*guard {
        ServeBootState::Ready { operator_login } => {
            assert_eq!(operator_login, &inputs.technical_user_login);
        }
        ServeBootState::NeedsSetup => panic!("boot state should have flipped to Ready"),
    }
}

/// PR-46α / session-62 — validation: an empty login surfaces as
/// `SetupRouteHelperError::Validation` (which the HTTP handler maps
/// to 400). The keychain MUST remain untouched.
#[test]
fn setup_route_rejects_empty_login_without_partial_write() {
    init_mock_keyring();
    let tenant = unique_tenant("validation");
    let state = build_state(ServeBootState::NeedsSetup, &tenant);

    let mut inputs = fixture_inputs();
    inputs.technical_user_login = "  ".to_string();

    let err = serve::setup_nav_credentials_request(&state, &inputs)
        .expect_err("blank login must fail validation");
    match err {
        SetupRouteHelperError::Validation(msg) => {
            assert!(msg.contains("Technical-user login"), "msg = {msg}");
        }
        SetupRouteHelperError::Other(e) => panic!("expected Validation, got Other({e:#})"),
    }

    // No keychain writes happened — every entry is None.
    let service = service_name(&tenant);
    assert_eq!(read_back(&service, ITEM_LOGIN), None);
    assert_eq!(read_back(&service, ITEM_PASSWORD), None);
    assert_eq!(read_back(&service, ITEM_SIGN_KEY), None);
    assert_eq!(read_back(&service, ITEM_CHANGE_KEY), None);

    // Boot state is still NeedsSetup.
    let guard = state.boot_state.read().unwrap();
    assert!(matches!(*guard, ServeBootState::NeedsSetup));
}

/// PR-46α / session-62 — gated route guard: in NeedsSetup state, the
/// mutation helpers (`submit_invoice_request`, `poll_ack_request`)
/// surface an error rather than proceeding. The HTTP handler's
/// `require_ready` middleware maps this to 503; the helper-level
/// pin asserts the boot-state read short-circuits before any audit-
/// ledger access.
#[test]
fn submit_invoice_request_refuses_when_needs_setup() {
    init_mock_keyring();
    let tenant = unique_tenant("gated-submit");
    let state = build_state(ServeBootState::NeedsSetup, &tenant);

    let err = serve::submit_invoice_request(&state, "inv_unknown")
        .expect_err("submit_invoice_request must reject in NeedsSetup");
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("NeedsSetup"),
        "error message must name NeedsSetup state: {msg}"
    );
}

/// PR-46α / session-62 — `poll_ack_request` mirror of the
/// `submit_invoice_request` gate. Defence in depth: a regression that
/// dropped the gate from ONE of the two mutation helpers would still
/// fail here.
#[test]
fn poll_ack_request_refuses_when_needs_setup() {
    init_mock_keyring();
    let tenant = unique_tenant("gated-poll");
    let state = build_state(ServeBootState::NeedsSetup, &tenant);

    let err = serve::poll_ack_request(&state, "inv_unknown")
        .expect_err("poll_ack_request must reject in NeedsSetup");
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("NeedsSetup"),
        "error message must name NeedsSetup state: {msg}"
    );
}

/// PR-46α / session-62 — handshake state-token contract. The
/// `ServeBootState::state_token` method emits the string the
/// backend's println includes on the handshake line; the Tauri shell's
/// handshake parser keys on it. Pin one per variant so a future
/// rename (e.g. "ready" → "running") surfaces here loud.
#[test]
fn boot_state_token_matches_handshake_wire_form() {
    assert_eq!(ServeBootState::NeedsSetup.state_token(), "needs-setup");
    assert_eq!(
        ServeBootState::Ready {
            operator_login: "x".to_string()
        }
        .state_token(),
        "ready"
    );
}

/// PR-46α / session-62 — CLI / HTTP parity at the keychain level.
/// Both surfaces call `setup_credentials_from_inputs` directly; the
/// route surfaces it via `setup_nav_credentials_request`, the CLI via
/// its interactive `run`. This pin proves that for identical inputs
/// the keychain end-state is byte-identical regardless of entry path.
#[test]
fn cli_and_http_reach_same_keychain_end_state() {
    init_mock_keyring();

    let cli_tenant = unique_tenant("cli-parity");
    let http_tenant = unique_tenant("http-parity");
    let inputs = fixture_inputs();

    // "CLI" path — direct call to the shared core.
    aberp::setup_nav_credentials::setup_credentials_from_inputs(&cli_tenant, &inputs)
        .expect("CLI-path core call must succeed");

    // "HTTP" path — through the route helper, which adds the
    // re-load + boot-state-flip on top of the shared core.
    let state = build_state(ServeBootState::NeedsSetup, &http_tenant);
    serve::setup_nav_credentials_request(&state, &inputs)
        .expect("HTTP-path route helper must succeed");

    // Both keychain end-states match input byte-for-byte.
    for (tenant_label, tenant) in [("CLI", &cli_tenant), ("HTTP", &http_tenant)] {
        let service = service_name(tenant);
        assert_eq!(
            read_back(&service, ITEM_LOGIN).as_deref(),
            Some(inputs.technical_user_login.as_str()),
            "{tenant_label}: login"
        );
        assert_eq!(
            read_back(&service, ITEM_PASSWORD).as_deref(),
            Some(inputs.technical_user_password.as_str()),
            "{tenant_label}: password"
        );
        assert_eq!(
            read_back(&service, ITEM_SIGN_KEY).as_deref(),
            Some(inputs.xml_sign_key.as_str()),
            "{tenant_label}: sign key"
        );
        assert_eq!(
            read_back(&service, ITEM_CHANGE_KEY).as_deref(),
            Some(inputs.xml_change_key.as_str()),
            "{tenant_label}: change key"
        );
    }
}

/// PR-46α / session-62 — defence-in-depth pin on the shared core's
/// validation: every variant of `SetupCredentialsError::Validation`
/// names the offending field in operator-readable form so the SPA's
/// inline-error pane prints something the operator can act on. The
/// per-field branches are unit-tested in `setup_nav_credentials.rs`;
/// this integration pin closes the loop on the propagation contract
/// from the route helper back to the caller.
#[test]
fn shared_core_validation_names_offending_field() {
    let mut inputs = fixture_inputs();
    inputs.xml_sign_key = "".to_string();

    let err = aberp::setup_nav_credentials::setup_credentials_from_inputs(
        "any-tenant",
        &inputs,
    )
    .expect_err("blank sign key must fail validation");
    match err {
        SetupCredentialsError::Validation(msg) => {
            assert!(msg.contains("xmlSignKey"), "got: {msg}");
        }
        SetupCredentialsError::Backend(e) => {
            panic!("expected Validation, got Backend({e:#})")
        }
    }
}
