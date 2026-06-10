//! PR-57 / session-77 — pin tests for the consolidated
//! `nav_credentials_blob` keychain item.
//!
//! Pin coverage (per the session-77 brief):
//!
//! 1. **Round-trip** — write the blob via
//!    `setup_credentials_from_inputs` (the shared core both the CLI
//!    and the SPA setup-wizard route go through) and re-read via the
//!    production `NavCredentials::load_from_keychain` path. All four
//!    fields must match byte-for-byte.
//! 2. **Legacy → blob migration** — pre-populate the four legacy
//!    per-artifact entries (no blob present), call
//!    `NavCredentials::load_from_keychain`, and assert the blob is
//!    materialised, the four legacy entries are deleted, and all four
//!    field values round-trip.
//! 3. **Single-slot rotation** — write the blob, call
//!    `rotate_nav_credential_request` for one field, and assert ONLY
//!    that field changed; the other three are byte-identical to the
//!    pre-rotation values.
//! 4. **Source-grep pin** — assert that the boot read path
//!    (`NavCredentials::load_from_keychain`) calls `read_blob` exactly
//!    once on the happy path, NOT four `read_secret` calls. This is
//!    the A151 pattern: a future contributor who fans the read back
//!    out to four entries (e.g., by reverting the consolidation)
//!    fails this pin at compile-time before they hit the operator-
//!    visible regression.
//!
//! ## Keychain isolation
//!
//! This file uses the same shared in-process `SharedMockCredential`
//! pattern as `serve_setup_nav_credentials_route.rs` (per
//! `feedback_keyring_mock_per_entry_isolation`: the default
//! `keyring::mock` backend mints a fresh credential per
//! `Entry::new`, so set-then-get returns `NoEntry`).

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::sync::{Mutex, Once, OnceLock};

use aberp_nav_transport::credentials::keychain::{
    service_name, write_blob, ITEM_CHANGE_KEY, ITEM_LOGIN, ITEM_NAV_CREDENTIALS_BLOB,
    ITEM_PASSWORD, ITEM_SIGN_KEY,
};
use aberp_nav_transport::credentials::NavCredentials;
use keyring::credential::{Credential, CredentialApi, CredentialBuilderApi, CredentialPersistence};
use keyring::Error as KeyringError;
use ulid::Ulid;

use aberp::setup_nav_credentials::{setup_credentials_from_inputs, NavCredentialInputs};

// ── Shared in-process mock keychain backend ──────────────────────────

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

fn unique_tenant(label: &str) -> String {
    format!("blob-{label}-{}", Ulid::new())
}

fn read_back(service: &str, item: &'static str) -> Option<String> {
    let entry = keyring::Entry::new(service, item).expect("build keyring::Entry");
    match entry.get_password() {
        Ok(s) => Some(s),
        Err(keyring::Error::NoEntry) => None,
        Err(e) => panic!("unexpected keychain error reading {item}: {e}"),
    }
}

fn fixture_inputs() -> NavCredentialInputs {
    NavCredentialInputs {
        technical_user_login: "techuser-blob-01".to_string(),
        technical_user_password: "pw-with-special-chars-\"\\".to_string(),
        xml_sign_key: "sk-aaaa-bbbb-cccc".to_string(),
        xml_change_key: "ck-16-bytes-blob".to_string(),
    }
}

// ── Pin 1 — round-trip (write blob → load via NavCredentials) ────────

#[test]
fn blob_round_trip_via_setup_core() {
    init_mock_keyring();
    let tenant = unique_tenant("roundtrip");
    let inputs = fixture_inputs();

    setup_credentials_from_inputs(&tenant, &inputs)
        .expect("write blob via shared core must succeed");

    // Re-read via the production read path.
    let creds = NavCredentials::load_from_keychain(&tenant)
        .expect("blob must round-trip via NavCredentials::load_from_keychain");
    assert_eq!(creds.login(), inputs.technical_user_login.as_str());
    assert_eq!(
        creds.password_bytes(),
        inputs.technical_user_password.as_bytes(),
        "password must round-trip — fixture contains JSON-special chars (quote + backslash)"
    );
    assert_eq!(creds.sign_key_bytes(), inputs.xml_sign_key.as_bytes());
    assert_eq!(creds.change_key_bytes(), inputs.xml_change_key.as_bytes());

    // Surface check: the consolidated blob entry exists, the four
    // legacy entries do not.
    let service = service_name(&tenant);
    assert!(read_back(&service, ITEM_NAV_CREDENTIALS_BLOB).is_some());
    assert_eq!(read_back(&service, ITEM_LOGIN), None);
    assert_eq!(read_back(&service, ITEM_PASSWORD), None);
    assert_eq!(read_back(&service, ITEM_SIGN_KEY), None);
    assert_eq!(read_back(&service, ITEM_CHANGE_KEY), None);
}

// ── Pin 2 — legacy → blob migration ──────────────────────────────────

#[test]
fn legacy_entries_migrate_to_blob_on_first_load() {
    init_mock_keyring();
    let tenant = unique_tenant("migrate");
    let service = service_name(&tenant);

    // Pre-populate the four legacy entries directly (mimics an
    // installation populated pre-PR-57). No blob present.
    keyring::Entry::new(&service, ITEM_LOGIN)
        .unwrap()
        .set_password("legacy-login")
        .unwrap();
    keyring::Entry::new(&service, ITEM_PASSWORD)
        .unwrap()
        .set_password("legacy-pass")
        .unwrap();
    keyring::Entry::new(&service, ITEM_SIGN_KEY)
        .unwrap()
        .set_password("legacy-sign")
        .unwrap();
    keyring::Entry::new(&service, ITEM_CHANGE_KEY)
        .unwrap()
        .set_password("legacy-change")
        .unwrap();
    assert_eq!(read_back(&service, ITEM_NAV_CREDENTIALS_BLOB), None);

    // First load triggers the one-shot migration: read legacy →
    // write blob → delete legacy.
    let creds = NavCredentials::load_from_keychain(&tenant)
        .expect("migration must succeed when all 4 legacy entries are present");
    assert_eq!(creds.login(), "legacy-login");
    assert_eq!(creds.password_bytes(), b"legacy-pass");
    assert_eq!(creds.sign_key_bytes(), b"legacy-sign");
    assert_eq!(creds.change_key_bytes(), b"legacy-change");

    // Post-migration: blob is present, legacy entries are gone.
    assert!(
        read_back(&service, ITEM_NAV_CREDENTIALS_BLOB).is_some(),
        "blob must be materialised after migration"
    );
    assert_eq!(
        read_back(&service, ITEM_LOGIN),
        None,
        "legacy login must be deleted after migration"
    );
    assert_eq!(read_back(&service, ITEM_PASSWORD), None);
    assert_eq!(read_back(&service, ITEM_SIGN_KEY), None);
    assert_eq!(read_back(&service, ITEM_CHANGE_KEY), None);

    // Second load takes the blob-only path (legacy entries no longer
    // exist) and must succeed with identical field values.
    let creds_again = NavCredentials::load_from_keychain(&tenant)
        .expect("second load must succeed via blob-only path");
    assert_eq!(creds_again.login(), "legacy-login");
    assert_eq!(creds_again.password_bytes(), b"legacy-pass");
}

// ── Pin 3 — single-slot rotation via the SPA route helper ────────────

#[test]
fn rotation_preserves_other_three_fields() {
    init_mock_keyring();
    let tenant = unique_tenant("rotate");

    // Set up a populated blob.
    write_blob(&tenant, "lg-pre", "pw-pre", "sk-pre", "ck-pre").expect("seed blob must succeed");

    // Build a minimal AppState to invoke the rotation helper. We
    // reuse the helper directly (not the HTTP route) to keep this
    // file independent of axum.
    use std::sync::Arc;
    let tenant_id = aberp_audit_ledger::TenantId::new(tenant.to_string()).expect("tenant id");
    let binary_hash = aberp_audit_ledger::BinaryHash::from_bytes([0u8; 32]);
    let state = aberp::serve::AppState {
        db_path: Arc::new(std::env::temp_dir().join(format!("aberp-blob-{}.duckdb", Ulid::new()))),
        tenant: tenant_id,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        boot_state: Arc::new(std::sync::RwLock::new(
            aberp::serve::ServeBootState::Ready {
                operator_login: "lg-pre".to_string(),
            },
        )),
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
    };

    // Rotate password only.
    aberp::serve::rotate_nav_credential_request(&state, "password", "pw-NEW")
        .expect("password rotation must succeed");

    let creds =
        NavCredentials::load_from_keychain(&tenant).expect("blob must round-trip after rotation");
    assert_eq!(creds.login(), "lg-pre", "login must NOT have changed");
    assert_eq!(
        creds.password_bytes(),
        b"pw-NEW",
        "password must have changed"
    );
    assert_eq!(
        creds.sign_key_bytes(),
        b"sk-pre",
        "sign_key must NOT have changed"
    );
    assert_eq!(
        creds.change_key_bytes(),
        b"ck-pre",
        "change_key must NOT have changed"
    );
}

// ── Pin 4 — source-grep pin (A151 pattern) ───────────────────────────

/// PR-57 / session-77 — source-grep pin asserting the boot read path
/// in `NavCredentials::load_from_keychain` uses ONE blob read (the
/// `keychain::read_blob` call), not four `keychain::read_secret`
/// calls. A future contributor reverting the consolidation (or
/// fanning the read back out across four entries for any reason)
/// trips this pin at test-compile time before the operator hits the
/// regression as "I'm prompted four times again on every rebuild".
///
/// The A151 pattern is `include_str!` + `.contains(...)` — small
/// runtime cost, big readability win when the regression lands. The
/// alternative (a runtime mock that counts get_password calls) is
/// more code AND less obvious as a structural-invariant guard.
#[test]
fn source_grep_pin_load_path_reads_blob_once() {
    let source = include_str!("../../../crates/nav-transport/src/credentials/mod.rs");

    // The blob-first read happens via `keychain::read_blob` — ONE
    // call site, on the happy path.
    let blob_read_count = source.matches("keychain::read_blob(").count();
    assert_eq!(
        blob_read_count, 1,
        "NavCredentials::load_from_keychain must call keychain::read_blob() \
         exactly once on the happy path (found {blob_read_count}); a fan-out \
         to multiple blob reads or a removal of the blob read both regress \
         the prompt-count win."
    );

    // The legacy read site is the migration-only path. Exactly one
    // call to `read_legacy_artifacts` (the function that internally
    // reads the four legacy items).
    let legacy_call_count = source.matches("keychain::read_legacy_artifacts(").count();
    assert_eq!(
        legacy_call_count, 1,
        "NavCredentials::load_from_keychain must call \
         keychain::read_legacy_artifacts() exactly once \
         (migration path; found {legacy_call_count})."
    );

    // Defence in depth: no direct fan-out to four individual
    // `keychain::read_secret` calls inside this module.
    let direct_read_secret_count = source.matches("keychain::read_secret(").count();
    assert_eq!(
        direct_read_secret_count, 0,
        "NavCredentials::load_from_keychain must NOT call \
         keychain::read_secret() directly anymore — the four-entry \
         fan-out is what PR-57 removed. Found {direct_read_secret_count} \
         direct calls."
    );
}
