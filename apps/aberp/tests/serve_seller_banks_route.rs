//! PR-72 / session-94 — integration pins for the bank-account routes
//! (PR-B of the multi-bank-account initiative; ADR-0040 §addendum).
//!
//! Mirror of the `serve_partners_route.rs` posture: each test drives
//! the public `*_request` library helper with a per-test scratch dir +
//! a path_override so the HTTPS listener never spins and `HOME` never
//! mutates. The full HTTP-status mapping (200 / 201 / 400 / 404 / 409
//! / 500) is structural — `axum`'s `(Status, Json(...)).into_response()`
//! does the assembly; pinning the response bytes would couple the test
//! to axum's private response shape per CLAUDE.md rule 2.
//!
//! Pin coverage (one per documented invariant):
//!   1. **create happy path** — empty file → one HUF entry; the
//!      response is the full collection with the new entry marked
//!      default (per the "first entry for a currency becomes default"
//!      route-layer rule).
//!   2. **create demotes previous default** — pre-existing HUF
//!      default + new HUF entry with `set_as_default = true` →
//!      previous default is demoted in the same write.
//!   3. **create validation 400 — bad currency** — `currency = "USD"`
//!      surfaces a typed `Validation` error with the bilingual message
//!      naming the currency field.
//!   4. **create validation 400 — missing required field** — empty
//!      `account_number` surfaces a typed `Validation` error keyed by
//!      the matching camelCase field name.
//!   5. **update happy path** — edit a row's bank_name + swift_bic; the
//!      default flag is preserved (set-default is a separate route).
//!   6. **update 404** — unknown id surfaces `NotFound`.
//!   7. **set-default flip** — flipping the default to another HUF
//!      entry demotes the previous default in the same write.
//!   8. **delete the only HUF entry with EUR still present → 409**
//!      Conflict — the brief's "refuse if this is the only entry for
//!      its currency AND other currencies still have entries" rule.
//!   9. **delete the marked default → promote the next remaining HUF
//!      entry** — the per-currency-default invariant must remain
//!      satisfied after the delete.
//!   10. **atomic write round-trip** — after a create, re-reading the
//!       on-disk file produces an equal collection. Identity sections
//!       outside the bank block survive the write verbatim.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, TenantId};

use aberp::seller_banks::read_seller_banks;
use aberp::serve::{self, AppState, SellerBankInputs, SellerBankRouteError, ServeBootState};
use aberp_billing::Currency;

const TEST_TENANT: &str = "serve_seller_banks_route_test";

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

fn test_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "aberp-serve-seller-banks-{label}-{}-{}-{}",
        std::process::id(),
        nanos,
        seq,
    ));
    fs::create_dir_all(&dir).expect("create test dir");
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
        boot_state: Arc::new(std::sync::RwLock::new(ServeBootState::Ready {
            operator_login: "test-operator".to_string(),
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

fn good_inputs(currency: &str, account: &str) -> SellerBankInputs {
    SellerBankInputs {
        currency: currency.to_string(),
        account_number: account.to_string(),
        bank_name: "Test Bank".to_string(),
        swift_bic: "GIBAHUHB".to_string(),
        set_as_default: false,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pin tests
// ──────────────────────────────────────────────────────────────────────

/// Pin #1 — create happy path. An empty seller.toml + a single HUF
/// create produces a one-entry collection with `is_default = true`
/// (the first entry for a currency is the implicit default).
#[test]
fn create_seller_bank_happy_path_returns_full_collection() {
    let dir = test_dir("create-happy");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");
    let inputs = good_inputs("HUF", "11111111-22222222-33333333");

    let banks =
        serve::create_seller_bank_request(&state, &inputs, Some(&path)).expect("create happy path");
    assert_eq!(banks.entries().len(), 1);
    let entry = &banks.entries()[0];
    assert_eq!(entry.currency, Currency::Huf);
    assert_eq!(entry.account_number, "11111111-22222222-33333333");
    assert!(
        entry.default,
        "first entry for a currency must become the implicit default"
    );
    assert!(
        entry.id.starts_with("bnk_"),
        "deterministic id must use bnk_ prefix"
    );
}

/// Pin #2 — create demotes the previous default for the same currency
/// in the same write. The "exactly one default per currency" invariant
/// holds end-to-end across the route.
#[test]
fn create_with_set_as_default_demotes_previous_default() {
    let dir = test_dir("create-demote");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");

    // Seed: HUF-A marked default.
    let _ = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("HUF", "HUF-A")
        },
        Some(&path),
    )
    .expect("seed first HUF entry");

    // Add HUF-B asking to be the new default.
    let banks = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("HUF", "HUF-B")
        },
        Some(&path),
    )
    .expect("add second HUF as new default");

    assert_eq!(banks.entries().len(), 2);
    let huf_defaults: Vec<&str> = banks
        .entries()
        .iter()
        .filter(|e| e.currency == Currency::Huf && e.default)
        .map(|e| e.account_number.as_str())
        .collect();
    assert_eq!(
        huf_defaults,
        vec!["HUF-B"],
        "exactly one HUF default after the set-as-default create, must be HUF-B"
    );
}

/// PR-74 / session-96 — Pin #2b — create with `set_as_default = false`
/// on a currency that already has a default does NOT demote the
/// existing default. The PR-74 brief flagged this as a suspected
/// silent-corruption hypothesis: an unconditional demote on the
/// create path would leave the currency with zero defaults and would
/// fail the next read with `NoDefaultAmongEntries`. This pin locks
/// the route's mint-then-conditional-demote behaviour so the regression
/// cannot land silently.
#[test]
fn create_without_set_as_default_preserves_existing_default() {
    let dir = test_dir("create-no-demote");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");

    // Seed: HUF-A marked default.
    let _ = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("HUF", "HUF-A")
        },
        Some(&path),
    )
    .expect("seed first HUF entry");

    // Add HUF-B WITHOUT asking to be the default (the
    // operator-typical "I'm adding a secondary account" path).
    let banks = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: false,
            ..good_inputs("HUF", "HUF-B")
        },
        Some(&path),
    )
    .expect("add second HUF without set_as_default");

    assert_eq!(banks.entries().len(), 2);
    let huf_defaults: Vec<&str> = banks
        .entries()
        .iter()
        .filter(|e| e.currency == Currency::Huf && e.default)
        .map(|e| e.account_number.as_str())
        .collect();
    assert_eq!(
        huf_defaults,
        vec!["HUF-A"],
        "the existing HUF default must survive a non-default add — \
         unconditional demote would leave zero defaults and corrupt the \
         per-currency-default invariant",
    );
    // Defence-in-depth — re-read the on-disk file and confirm the same
    // shape survives the atomic write round-trip.
    let on_disk = read_seller_banks(&path).expect("re-read");
    let on_disk_defaults: Vec<&str> = on_disk
        .entries()
        .iter()
        .filter(|e| e.currency == Currency::Huf && e.default)
        .map(|e| e.account_number.as_str())
        .collect();
    assert_eq!(on_disk_defaults, vec!["HUF-A"]);
}

/// Pin #3 — create with `currency = "USD"` (outside the ADR-0037
/// closed vocab) loud-fails as a typed Validation error. The error
/// is keyed by the `currency` field name and the bilingual message
/// names the unsupported value.
#[test]
fn create_seller_bank_rejects_unsupported_currency() {
    let dir = test_dir("create-bad-currency");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");
    let inputs = SellerBankInputs {
        currency: "USD".to_string(),
        ..good_inputs("HUF", "USD-A")
    };

    let err = serve::create_seller_bank_request(&state, &inputs, Some(&path))
        .expect_err("USD must fail closed-vocab");
    match err {
        SellerBankRouteError::Validation(fields) => {
            let currency = fields
                .iter()
                .find(|f| f.field == "currency")
                .expect("currency field error present");
            assert!(
                currency.message.contains("USD"),
                "message must name the unsupported value: {}",
                currency.message
            );
            assert!(
                currency.message.contains("HUF") && currency.message.contains("EUR"),
                "message must name the allowed values: {}",
                currency.message
            );
            assert!(
                currency.message.contains("Pénznem") && currency.message.contains("Unsupported"),
                "message must be bilingual: {}",
                currency.message
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

/// Pin #4 — create with an empty required field (here:
/// `account_number`) surfaces a typed Validation error keyed by
/// `accountNumber` (camelCase, matching the SPA form field name).
#[test]
fn create_seller_bank_rejects_empty_required_field() {
    let dir = test_dir("create-empty-field");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");
    let inputs = SellerBankInputs {
        account_number: "   ".to_string(),
        ..good_inputs("HUF", "_")
    };

    let err = serve::create_seller_bank_request(&state, &inputs, Some(&path))
        .expect_err("blank account_number must fail");
    match err {
        SellerBankRouteError::Validation(fields) => {
            assert!(
                fields.iter().any(|f| f.field == "accountNumber"),
                "must flag accountNumber: {fields:?}"
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

/// Pin #5 — update edits one row's bank_name + swift_bic; the
/// `default` flag is preserved (the brief explicitly carves set-
/// default out as a separate route).
#[test]
fn update_seller_bank_preserves_default_flag() {
    let dir = test_dir("update-preserve-default");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");

    let banks = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("HUF", "HUF-1")
        },
        Some(&path),
    )
    .expect("create");
    let id = banks.entries()[0].id.clone();

    let updated = serve::update_seller_bank_request(
        &state,
        &id,
        &SellerBankInputs {
            currency: "HUF".to_string(),
            account_number: "HUF-1".to_string(),
            bank_name: "Renamed Bank".to_string(),
            swift_bic: "OTPVHUHB".to_string(),
            set_as_default: false, // explicitly NOT set — must not demote
        },
        Some(&path),
    )
    .expect("update happy");
    assert_eq!(updated.entries().len(), 1);
    let entry = &updated.entries()[0];
    assert_eq!(entry.bank_name, "Renamed Bank");
    assert_eq!(entry.swift_bic, "OTPVHUHB");
    assert!(
        entry.default,
        "update must preserve the existing default flag; set-default is a separate route"
    );
}

/// Pin #6 — update with an unknown id surfaces NotFound.
#[test]
fn update_seller_bank_returns_not_found_for_unknown_id() {
    let dir = test_dir("update-404");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");

    let err = serve::update_seller_bank_request(
        &state,
        "bnk_does-not-exist",
        &good_inputs("HUF", "HUF-X"),
        Some(&path),
    )
    .expect_err("unknown id must surface NotFound");
    assert!(
        matches!(err, SellerBankRouteError::NotFound),
        "expected NotFound, got {err:?}"
    );
}

/// Pin #7 — set-default flips the marked default to another HUF
/// entry; the previous default is demoted in the same write so the
/// invariant remains exactly-one-per-currency.
#[test]
fn set_default_flips_the_marked_default_and_demotes_previous() {
    let dir = test_dir("set-default-flip");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");

    let _ = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("HUF", "HUF-A")
        },
        Some(&path),
    )
    .unwrap();
    let banks =
        serve::create_seller_bank_request(&state, &good_inputs("HUF", "HUF-B"), Some(&path))
            .unwrap();
    // HUF-A is currently the default; flip to HUF-B.
    let huf_b_id = banks
        .entries()
        .iter()
        .find(|e| e.account_number == "HUF-B")
        .expect("HUF-B present")
        .id
        .clone();
    let after =
        serve::set_default_seller_bank_request(&state, &huf_b_id, Some(&path)).expect("flip");
    let huf_defaults: Vec<&str> = after
        .entries()
        .iter()
        .filter(|e| e.currency == Currency::Huf && e.default)
        .map(|e| e.account_number.as_str())
        .collect();
    assert_eq!(huf_defaults, vec!["HUF-B"]);
}

/// Pin #8 — delete the only entry for a currency while a different
/// currency still has entries returns 409 Conflict (the brief's
/// explicit rule). The on-disk file is NOT mutated when the route
/// errors out.
#[test]
fn delete_only_entry_for_currency_with_other_currencies_returns_conflict() {
    let dir = test_dir("delete-conflict");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");

    let huf_only = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("HUF", "HUF-1")
        },
        Some(&path),
    )
    .unwrap();
    let huf_id = huf_only.entries()[0].id.clone();
    let _eur = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("EUR", "EUR-1")
        },
        Some(&path),
    )
    .unwrap();

    let body_before_delete = fs::read_to_string(&path).expect("file present before delete");

    let err = serve::delete_seller_bank_request(&state, &huf_id, Some(&path))
        .expect_err("must refuse with Conflict");
    match err {
        SellerBankRouteError::Conflict { message } => {
            assert!(
                message.contains("HUF"),
                "message names the orphaned currency: {message}"
            );
            assert!(
                message.contains("Nem törölhető") && message.contains("Cannot delete"),
                "message bilingual: {message}"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    // The file must be unchanged: the route validates pre-write.
    let body_after_failed_delete =
        fs::read_to_string(&path).expect("file present after refused delete");
    assert_eq!(body_before_delete, body_after_failed_delete);
}

/// Pin #9 — deleting the marked default for a currency that still
/// has additional entries promotes the next remaining entry to
/// default so the invariant remains satisfied (no zero-defaults
/// failure on the next load).
#[test]
fn delete_marked_default_promotes_next_remaining_entry() {
    let dir = test_dir("delete-promote-next");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");

    let _ = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("HUF", "HUF-A")
        },
        Some(&path),
    )
    .unwrap();
    let banks =
        serve::create_seller_bank_request(&state, &good_inputs("HUF", "HUF-B"), Some(&path))
            .unwrap();
    let huf_a_id = banks
        .entries()
        .iter()
        .find(|e| e.account_number == "HUF-A")
        .unwrap()
        .id
        .clone();

    let after = serve::delete_seller_bank_request(&state, &huf_a_id, Some(&path))
        .expect("delete must succeed");
    assert_eq!(after.entries().len(), 1);
    let entry = &after.entries()[0];
    assert_eq!(entry.account_number, "HUF-B");
    assert!(
        entry.default,
        "removing the marked default must promote the next remaining entry"
    );
}

/// Pin #10 — atomic write round-trip + identity-section preservation.
/// After a create the on-disk file re-reads into an equal collection
/// AND any pre-existing `[seller]` / `[seller.address]` identity
/// sections survive the write verbatim.
#[test]
fn atomic_write_round_trip_preserves_identity_block() {
    let dir = test_dir("round-trip");
    let state = build_state(dir.join("aberp.duckdb"));
    let path = dir.join("seller.toml");

    // Seed the file with an identity block (no bank section yet).
    let identity_body = "\
# Pre-existing seller config\n\
[seller]\n\
legal_name = \"Áben Consulting KFT.\"\n\
tax_number = \"24904362-2-41\"\n\
\n\
[seller.address]\n\
country_code = \"HU\"\n\
postal_code = \"1037\"\n\
city = \"Budapest\"\n\
street = \"Visszatérő köz 6\"\n";
    fs::write(&path, identity_body).unwrap();

    let banks = serve::create_seller_bank_request(
        &state,
        &SellerBankInputs {
            set_as_default: true,
            ..good_inputs("HUF", "12345678-12345678-12345678")
        },
        Some(&path),
    )
    .expect("create");

    // Re-read the file from disk: the bank block round-trips and the
    // identity sections are preserved verbatim.
    let body_on_disk = fs::read_to_string(&path).expect("file present");
    assert!(
        body_on_disk.contains("legal_name = \"Áben Consulting KFT.\""),
        "identity legal_name preserved: {body_on_disk}"
    );
    assert!(
        body_on_disk.contains("[seller.address]"),
        "address heading preserved: {body_on_disk}"
    );
    assert!(
        body_on_disk.contains("city = \"Budapest\""),
        "address fields preserved: {body_on_disk}"
    );
    assert!(
        body_on_disk.contains("[[seller.banks]]"),
        "bank section emitted in canonical new form: {body_on_disk}"
    );

    let reloaded = read_seller_banks(&path).expect("re-load");
    assert_eq!(banks, reloaded, "write→read round-trip must be lossless");
}
