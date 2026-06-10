//! Integration tests for `POST /invoices/issue` (PR-44ζ /
//! session-59).
//!
//! Three pin tests:
//!
//! 1. **HUF happy path** — `issue_invoice_request` returns
//!    `Ok(summary)`; the audit ledger contains the expected
//!    `InvoiceSequenceReserved` + `InvoiceDraftCreated` pair; the
//!    XML lands on disk at the server-minted path.
//! 2. **EUR happy path** — with a fake `MnbRatesProvider` returning
//!    a known rate, the audit ledger's draft payload carries the
//!    rate metadata stamp per ADR-0037 §1.a + §1.c.
//! 3. **Validation failure** — an empty-lines request fails the
//!    `validate_issue_request` pre-check and would surface at the
//!    route as a 400. Pinned here at the validator boundary so
//!    the test does not have to spin the axum listener.
//!
//! Both happy-path tests inject a fake provider per A140 (matching
//! the offline-test posture in `tests/issue_invoice_eur_offline.rs`)
//! so the test is fully offline.
//!
//! The full HTTP layer (status code emission, JSON body bytes) is
//! structural — axum's `Json(...).into_response()` constructs the
//! response from the typed value; pinning it would couple the test
//! to axum's private response-building details. Per CLAUDE.md rule
//! 2 the discriminator at the `issue_invoice_request` level is the
//! load-bearing pin.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::Currency;
use aberp_mnb_rates::{MnbError, MnbRate};
use time::Date;
use ulid::Ulid;

use aberp::audit_payloads::InvoiceDraftCreatedPayload;
use aberp::issue_invoice::{AddressJson, CustomerJson, LineJson, SupplierJson};
use aberp::mnb_rates_provider::MnbRatesProvider;
use aberp::nav_xml::CustomerVatStatus;
use aberp::serve::{self, AppState, IssueInvoiceRequest};

const TEST_TENANT: &str = "serve_issue_route_test";

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

fn test_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir()
            .join("aberp-serve-issue")
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
        // PR-46α / session-62 — `operator_login` moved inside the
        // [`ServeBootState::Ready`] variant. Tests construct the
        // Ready state directly; the in-process setup-route flip path
        // is covered by `serve_setup_nav_credentials_route.rs`.
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

fn fixture_supplier() -> SupplierJson {
    SupplierJson {
        tax_number: "12345678-1-42".to_string(),
        name: "ABERP Supplier Kft.".to_string(),
        address: AddressJson {
            country_code: "HU".to_string(),
            postal_code: "1011".to_string(),
            city: "Budapest".to_string(),
            street: "Fő utca 1.".to_string(),
        },
    }
}

fn fixture_customer() -> CustomerJson {
    CustomerJson {
        // PR-97 / ADR-0048 — preserve pre-PR-97 implicit
        // Domestic posture for legacy test fixtures.
        vat_status: CustomerVatStatus::Domestic,
        partner_id: None,
        tax_number: "87654321-2-13".to_string(),
        name: "Vevő Kft.".to_string(),
        // PR-77 / session-101 — preflight requires customer.address.
        address: Some(AddressJson {
            country_code: "HU".to_string(),
            postal_code: "1052".to_string(),
            city: "Budapest".to_string(),
            street: "Váci utca 19.".to_string(),
        }),
    }
}

fn fixture_lines() -> Vec<LineJson> {
    vec![LineJson {
        description: "Widget A".to_string(),
        quantity: rust_decimal::Decimal::from(2),
        unit_price: 1000,
        vat_rate_percent: 27,
        note: None,
        unit: None,
    }]
}

fn fixture_request(currency: Currency) -> IssueInvoiceRequest {
    IssueInvoiceRequest {
        customer: fixture_customer(),
        lines: fixture_lines(),
        currency,
        series: None,
        bank_account_id: None,
        invoice_note: None,
        // PR-84 — fixture defaults all three date fields to `None`;
        // the issuance pipeline defaults payment_deadline + delivery_date
        // to the system issue date when absent, preserving the pre-PR-84
        // wire byte shape the integration tests pin against.
        payment_deadline: None,
        delivery_date: None,
        delivery_date_override: None,
        // PR-92 — opt out of the default-on auto-send so the issue
        // integration tests stay SMTP-free.
        email_buyer_on_issue: Some(false),
        submit_to_nav_on_issue: Some(false),
        payment_method: aberp_billing::PaymentMethod::default(),
        email_recipient_override: None,
    }
}

/// PR-53 / session-73 — write a fixture seller.toml so the route's
/// new server-side seller read (`supplier_from_seller_toml`) finds an
/// identity-complete file. The wizard chain (PR-46α + PR-51) gates
/// the boot state on this in production; the integration tests build
/// `AppState::Ready` directly so they have to seed the file
/// themselves.
fn write_fixture_seller_toml(home_dir: &std::path::Path) {
    let tenant_dir = home_dir.join(".aberp").join(TEST_TENANT);
    std::fs::create_dir_all(&tenant_dir).expect("create tenant dir for seller.toml fixture");
    let body = r#"[seller]
legal_name = "ABERP Supplier Kft."
tax_number = "12345678-1-42"

[seller.address]
country_code = "HU"
postal_code = "1011"
city = "Budapest"
street = "Fő utca 1."
"#;
    std::fs::write(tenant_dir.join("seller.toml"), body).expect("write seller.toml fixture");
}

// ──────────────────────────────────────────────────────────────────────
// Fake MnbRatesProvider — mirrors `tests/issue_invoice_eur_offline.rs`
// ──────────────────────────────────────────────────────────────────────

/// HashMap-backed `MnbRatesProvider` for offline tests. Returns
/// `MnbError::NoRateForCurrency` for any (currency, date) tuple not
/// in the map. Mirrors the fake in
/// `tests/issue_invoice_eur_offline.rs` (duplicated per CLAUDE.md
/// rule 3 — extracting to a shared dev-dep helper would widen the
/// surface for a second consumer).
struct FakeMnbRates {
    rates: HashMap<(Currency, Date), MnbRate>,
    calls: Mutex<Vec<(Currency, Date)>>,
}

#[async_trait::async_trait]
impl MnbRatesProvider for FakeMnbRates {
    async fn fetch_official_rate(
        &self,
        currency: Currency,
        date: Date,
    ) -> Result<MnbRate, MnbError> {
        self.calls.lock().unwrap().push((currency, date));
        match self.rates.get(&(currency, date)) {
            Some(rate) => Ok(rate.clone()),
            None => Err(MnbError::NoRateForCurrency {
                currency: currency.iso_code().to_string(),
                date: date.to_string(),
            }),
        }
    }
}

/// Sentinel provider for the HUF path — should never be consulted
/// (issue_from_parsed's HUF branch is rate-free per ADR-0037 §1).
/// Tests that exercise the HUF path use this to prove the
/// rate-fetch path is not entered.
struct UnreachableProvider;

#[async_trait::async_trait]
impl MnbRatesProvider for UnreachableProvider {
    async fn fetch_official_rate(
        &self,
        _currency: Currency,
        _date: Date,
    ) -> Result<MnbRate, MnbError> {
        unreachable!("UnreachableProvider must not be consulted — HUF path is rate-free")
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pin tests
// ──────────────────────────────────────────────────────────────────────

/// HUF happy path — the route's library helper writes the
/// `InvoiceSequenceReserved` + `InvoiceDraftCreated` audit pair and
/// returns a non-empty summary. The returned `invoice_id` matches
/// the prefixed-ULID shape; the NAV XML lands at the server-minted
/// path (recorded on the draft payload's `nav_xml_path` field).
#[tokio::test(flavor = "current_thread")]
async fn issue_route_huf_happy_path_writes_audit_pair_and_xml() {
    let dir = test_dir("huf");
    // HOME redirect so the server-side `~/.aberp/serve/<tenant>/issued/`
    // path stays inside the test scratch directory. The redirect is
    // process-wide but this test file has only the three serial tests
    // below and none of them depend on $HOME beyond this redirect.
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

    let summary = serve::issue_invoice_request(
        &state,
        fixture_request(Currency::Huf),
        fixture_supplier(),
        &UnreachableProvider,
        actor,
        None,
    )
    .await
    .expect("HUF happy path must succeed");

    assert!(
        summary.invoice_id.starts_with("inv_"),
        "invoice_id `{}` must be prefixed-ULID",
        summary.invoice_id
    );
    // S165 — the emit path now prepends the build-profile prefix
    // (`TEST-` on dev/test builds, empty on production). Compose the
    // expected stem from the const so this pins under both flavours.
    let number_stem = format!(
        "{}INV-default/",
        aberp::build_profile::INVOICE_NUMBER_TEST_PREFIX
    );
    assert!(
        summary.invoice_number.starts_with(&number_stem),
        "invoice_number `{}` must carry the build prefix + series stem `{number_stem}`",
        summary.invoice_number
    );
    assert!(
        summary.nav_xml_path.exists(),
        "NAV XML must land on disk at the server-minted path"
    );

    // Walk the audit ledger and prove the two-event pair landed.
    let ledger = Ledger::open(
        dir.join("aberp.duckdb"),
        TenantId::new(TEST_TENANT.to_string()).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
    .expect("open ledger");
    let entries = ledger.entries().expect("read entries");
    let kinds: Vec<&EventKind> = entries.iter().map(|e| &e.kind).collect();
    assert!(
        kinds.contains(&&EventKind::InvoiceSequenceReserved),
        "InvoiceSequenceReserved must be in the ledger after issuance"
    );
    assert!(
        kinds.contains(&&EventKind::InvoiceDraftCreated),
        "InvoiceDraftCreated must be in the ledger after issuance"
    );

    // Keep the scratch dir alive until here.
    let _keep = &dir;
}

/// EUR happy path — with a FakeMnbRates provider returning a known
/// rate on the issue date, the audit ledger's draft payload carries
/// the rate-metadata stamp per ADR-0037 §1.a + §1.c. The route does
/// NOT make any real network call (the fake provider is the only
/// rate source). Mirrors the fake-provider pattern from
/// `tests/issue_invoice_eur_offline.rs` per A140.
#[tokio::test(flavor = "current_thread")]
async fn issue_route_eur_happy_path_stamps_rate_metadata_on_draft() {
    let dir = test_dir("eur");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

    // The supply date is whatever `OffsetDateTime::now_utc().date()`
    // returns inside `issue_from_parsed`; populate the fake for
    // today AND tomorrow so the walk-back loop has at least one
    // hit regardless of the clock-tick between this line and the
    // route call.
    let today = time::OffsetDateTime::now_utc().date();
    let mut fake_rates = HashMap::new();
    fake_rates.insert(
        (Currency::Eur, today),
        MnbRate {
            currency: Currency::Eur,
            date: today,
            unit: 1,
            value: "405.230000".to_string(),
        },
    );
    fake_rates.insert(
        (Currency::Eur, today - time::Duration::days(1)),
        MnbRate {
            currency: Currency::Eur,
            date: today - time::Duration::days(1),
            unit: 1,
            value: "405.230000".to_string(),
        },
    );
    let provider = FakeMnbRates {
        rates: fake_rates,
        calls: Mutex::new(Vec::new()),
    };

    let summary = serve::issue_invoice_request(
        &state,
        fixture_request(Currency::Eur),
        fixture_supplier(),
        &provider,
        actor,
        None,
    )
    .await
    .expect("EUR happy path must succeed");

    // Walk the ledger; find the matching draft entry; assert the
    // rate-metadata fields are populated per ADR-0037 §1.a.
    let ledger = Ledger::open(
        dir.join("aberp.duckdb"),
        TenantId::new(TEST_TENANT.to_string()).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
    .expect("open ledger");
    let entries = ledger.entries().expect("read entries");
    let draft_payload: InvoiceDraftCreatedPayload = entries
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::InvoiceDraftCreated)
        .map(|e| serde_json::from_slice(&e.payload).expect("decode draft payload"))
        .expect("InvoiceDraftCreated must be in the ledger after EUR issuance");
    assert_eq!(
        draft_payload.invoice_id, summary.invoice_id,
        "draft payload's invoice_id must match the returned summary id"
    );
    assert_eq!(
        draft_payload.currency.as_deref(),
        Some("EUR"),
        "EUR draft payload must stamp currency='EUR' per ADR-0037 §1.a"
    );
    assert_eq!(
        draft_payload.exchange_rate.as_deref(),
        Some("405.230000"),
        "EUR draft payload must stamp the MNB rate at 6-decimal precision per §1.c / C11"
    );
    assert_eq!(
        draft_payload.exchange_rate_source.as_deref(),
        Some("MNB"),
        "EUR draft payload must stamp source='MNB' per ADR-0037 §2.a"
    );
    assert!(
        draft_payload.exchange_rate_date.is_some(),
        "EUR draft payload must stamp the rate publication date per §1.a + §2.b"
    );
    assert!(
        draft_payload.huf_equivalent_total.is_some(),
        "EUR draft payload must stamp the round-half-even HUF-equivalent per §1.c / A137"
    );

    let _keep = &dir;
}

/// Validation failure — an empty-lines request fails the
/// `validate_issue_request` precheck. The current call path goes
/// through `issue_invoice_request` → `issue_from_parsed`, which
/// also rejects empty lines with `"input has no lines"`; the route
/// handler short-circuits at the validator BEFORE calling the
/// library helper so the SPA sees a 400 rather than a 500.
///
/// This test pins the library-helper layer's rejection so a
/// regression at either layer (handler skipping validation OR
/// library not rejecting) surfaces loud per CLAUDE.md rule 12.
#[tokio::test(flavor = "current_thread")]
async fn issue_route_rejects_empty_lines_with_loud_error() {
    let dir = test_dir("invalid");
    std::env::set_var("HOME", &dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

    let request = IssueInvoiceRequest {
        customer: fixture_customer(),
        lines: Vec::new(), // ← validation failure trigger
        currency: Currency::Huf,
        series: None,
        bank_account_id: None,
        invoice_note: None,
        payment_deadline: None,
        delivery_date: None,
        delivery_date_override: None,
        email_buyer_on_issue: Some(false),
        submit_to_nav_on_issue: Some(false),
        payment_method: aberp_billing::PaymentMethod::default(),
        email_recipient_override: None,
    };

    let err = serve::issue_invoice_request(
        &state,
        request,
        fixture_supplier(),
        &UnreachableProvider,
        actor,
        None,
    )
    .await
    .expect_err("empty-lines request must fail loud");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no lines") || msg.contains("line"),
        "loud-fail message must reference the lines validation: got `{msg}`"
    );
}

/// PR-50 / session-70 — supplier-config gate must fire at the
/// LIBRARY layer (defence in depth for any future caller that
/// bypasses `serve::handle_issue_invoice`'s route-layer validate).
///
/// This pin covers `issue_from_parsed`'s pre-issuance shape check
/// directly: a malformed supplier tax (`"24904362"` — the exact
/// failure mode Ervin hit on 2026-05-25, bare 8 digits without the
/// `-y-zz` segments) must loud-fail BEFORE the audit ledger burns a
/// sequence number. A regression that drops the gate would let a
/// fresh draft land on disk and then fail at submit time — the
/// pre-PR-50 failure mode the brief inverts.
#[tokio::test(flavor = "current_thread")]
async fn issue_route_rejects_malformed_supplier_tax_with_loud_error() {
    let dir = test_dir("malformed-supplier");
    std::env::set_var("HOME", &dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

    let mut bad_supplier = fixture_supplier();
    bad_supplier.tax_number = "24904362".to_string(); // ← bare base; no `-y-zz`

    let request = IssueInvoiceRequest {
        customer: fixture_customer(),
        lines: fixture_lines(),
        currency: Currency::Huf,
        series: None,
        bank_account_id: None,
        invoice_note: None,
        payment_deadline: None,
        delivery_date: None,
        delivery_date_override: None,
        email_buyer_on_issue: Some(false),
        submit_to_nav_on_issue: Some(false),
        payment_method: aberp_billing::PaymentMethod::default(),
        email_recipient_override: None,
    };

    // PR-53 / session-73 — supplier comes via the new arg, not the
    // wire body. `issue_invoice_request` itself doesn't re-validate
    // (the route handler calls `supplier_from_seller_toml` for the
    // typed 400 path); the supplier_config gate still fires inside
    // `issue_from_parsed` so the defence-in-depth pin remains.
    let err = serve::issue_invoice_request(
        &state,
        request,
        bad_supplier,
        &UnreachableProvider,
        actor,
        None,
    )
    .await
    .expect_err("malformed supplier tax number must fail loud");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("supplier_config_invalid"),
        "loud-fail must carry the `supplier_config_invalid` sentinel for the route layer's detection: got `{msg}`"
    );
    assert!(
        msg.contains("24904362"),
        "loud-fail must echo the rejected input so the operator sees the typo: got `{msg}`"
    );
    assert!(
        msg.contains("xxxxxxxx-y-zz"),
        "loud-fail must surface the expected shape so the operator knows the fix: got `{msg}`"
    );

    // The audit ledger must NOT have a fresh draft — the gate fires
    // BEFORE `pre_tx_setup`. A regression that opened the DB before
    // the gate would surface here as a present-but-empty DB file.
    assert!(
        !dir.join("aberp.duckdb").exists(),
        "pre-issuance gate must fail before any DB write; aberp.duckdb leaked at {}",
        dir.join("aberp.duckdb").display()
    );
}
