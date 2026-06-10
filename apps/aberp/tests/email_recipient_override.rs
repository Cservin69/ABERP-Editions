//! PR-203 / S203 — pin tests for the per-invoice
//! `email_recipient_override` column + the send-path resolver's
//! override-first ladder.
//!
//! Scope:
//!
//! 1. **Round-trip** — issue with an override, read the row back via
//!    `aberp_billing::load_email_recipient_override_in_tx`; the operator-
//!    typed comma-separated list comes back verbatim.
//! 2. **None round-trip** — issue with `email_recipient_override: None`;
//!    the column is read back as `None` (no silent "" coercion).
//! 3. **Validation rejection** — `validate_issue_request` (defence-in-
//!    depth: also fires for hand-crafted curl callers) refuses a
//!    malformed list with a `Plain` error class.
//! 4. **CR/LF-injection rejection** — a CR-bearing override is rejected
//!    BEFORE the row commits; the header-injection family that
//!    `partners::parse_and_validate_emails` already catches at the issue
//!    boundary is the first line of defence (`validate_no_crlf` at SMTP
//!    send time is the second).
//!
//! The resolver's "override-first" rung itself is structurally pinned
//! by the round-trip + column-read tests above plus the call-site
//! ladder in `serve::resolve_recipient_email` (rung 1 short-circuits
//! when the column is non-blank — verified by reading the column the
//! resolver consults). Spinning the SMTP transport for an actual
//! integration test would require a fake MTA which the SMTP-free
//! integration test surfaces in this crate intentionally avoid.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{Actor, BinaryHash, TenantId};
use aberp_billing::Currency;
use ulid::Ulid;

use aberp::issue_invoice::{AddressJson, CustomerJson, LineJson, SupplierJson};
use aberp::mnb_rates_provider::MnbRatesProvider;
use aberp::nav_xml::CustomerVatStatus;
use aberp::serve::{self, AppState, IssueInvoiceRequest};

const TEST_TENANT: &str = "email_recipient_override_test";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-s203")
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
        vat_status: CustomerVatStatus::Domestic,
        partner_id: None,
        tax_number: "87654321-2-13".to_string(),
        name: "Vevő Kft.".to_string(),
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

fn fixture_request(override_value: Option<String>) -> IssueInvoiceRequest {
    IssueInvoiceRequest {
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
        email_recipient_override: override_value,
    }
}

fn write_fixture_seller_toml(home_dir: &std::path::Path) {
    let tenant_dir = home_dir.join(".aberp").join(TEST_TENANT);
    std::fs::create_dir_all(&tenant_dir).expect("create tenant dir for seller.toml fixture");
    let body = r#"[seller]
legal_name = "ABERP Supplier Kft."
tax_number = "12345678-1-42"
address_country = "HU"
address_postal_code = "1011"
address_city = "Budapest"
address_street = "Fő utca 1."
"#;
    std::fs::write(tenant_dir.join("seller.toml"), body).expect("write seller.toml");
}

struct UnreachableProvider;

#[async_trait::async_trait]
impl MnbRatesProvider for UnreachableProvider {
    async fn fetch_official_rate(
        &self,
        _currency: Currency,
        _date: time::Date,
    ) -> Result<aberp_mnb_rates::MnbRate, aberp_mnb_rates::MnbError> {
        unreachable!("HUF path is rate-free")
    }
}

/// Pin 1 — round-trip. Issue with an operator-typed comma-separated
/// override; read it back off `invoice.email_recipient_override` via
/// the library's `load_email_recipient_override_in_tx` helper. The
/// stored string is byte-identical to the wire input (the route
/// normalises trim only; multi-recipient + canonical `", "` separator
/// pass through verbatim).
#[tokio::test(flavor = "current_thread")]
async fn override_round_trips_through_duckdb() {
    let dir = test_dir("round-trip");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

    let summary = serve::issue_invoice_request(
        &state,
        fixture_request(Some("buyer@example.com, cc@example.com".to_string())),
        fixture_supplier(),
        &UnreachableProvider,
        actor,
        None,
    )
    .await
    .expect("issue happy path with override");

    let mut conn = duckdb::Connection::open(&db_path).expect("open duckdb");
    let tx = conn.transaction().expect("begin read tx");
    let value = aberp_billing::load_email_recipient_override_in_tx(&tx, &summary.invoice_id)
        .expect("load override");
    tx.commit().expect("commit");
    assert_eq!(
        value.as_deref(),
        Some("buyer@example.com, cc@example.com"),
        "operator-typed override must round-trip through DuckDB byte-for-byte"
    );
}

/// Pin 2 — None round-trip. Issue without an override; the column
/// reads back as `None` (NOT an empty string — DuckDB's NULL is the
/// canonical "no value", and the resolver's `is_empty()` check should
/// never have a chance to mask the difference).
#[tokio::test(flavor = "current_thread")]
async fn override_round_trips_as_none_when_unset() {
    let dir = test_dir("none-round-trip");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

    let summary = serve::issue_invoice_request(
        &state,
        fixture_request(None),
        fixture_supplier(),
        &UnreachableProvider,
        actor,
        None,
    )
    .await
    .expect("issue happy path without override");

    let mut conn = duckdb::Connection::open(&db_path).expect("open duckdb");
    let tx = conn.transaction().expect("begin read tx");
    let value = aberp_billing::load_email_recipient_override_in_tx(&tx, &summary.invoice_id)
        .expect("load override");
    tx.commit().expect("commit");
    assert!(
        value.is_none(),
        "absent override must round-trip as None (got {value:?})"
    );
}

/// Pin 3 — empty-string is normalised to None at the issue route. An
/// operator who submits an empty input (e.g. cleared the prefilled
/// partner.email) must not produce an empty-string row that the
/// resolver would then treat as "use the override" (a future
/// resolver-bug that drops the `is_empty()` guard would silently
/// short-circuit to a blank Mailbox build).
#[tokio::test(flavor = "current_thread")]
async fn empty_override_normalised_to_none() {
    let dir = test_dir("empty-norm");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

    let summary = serve::issue_invoice_request(
        &state,
        fixture_request(Some("   ".to_string())),
        fixture_supplier(),
        &UnreachableProvider,
        actor,
        None,
    )
    .await
    .expect("issue happy path with whitespace override");

    let mut conn = duckdb::Connection::open(&db_path).expect("open duckdb");
    let tx = conn.transaction().expect("begin read tx");
    let value = aberp_billing::load_email_recipient_override_in_tx(&tx, &summary.invoice_id)
        .expect("load override");
    tx.commit().expect("commit");
    assert!(
        value.is_none(),
        "whitespace-only override must normalise to None (got {value:?})"
    );
}

/// Pin 4 — malformed override is rejected loudly. The
/// `partners::parse_and_validate_emails` gate at `validate_issue_request`
/// catches a bare token without `@` and surfaces it as a route-level
/// validation error. A regression that drops the per-segment gate
/// would let `not-an-email` reach the SMTP `Mailbox::new` and fail
/// deeper / less actionably.
#[tokio::test(flavor = "current_thread")]
async fn malformed_override_rejected_before_issuance() {
    let dir = test_dir("malformed");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let db_path = dir.join("aberp.duckdb");
    let state = build_state(db_path.clone());
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

    let err = serve::issue_invoice_request(
        &state,
        fixture_request(Some("not-an-email".to_string())),
        fixture_supplier(),
        &UnreachableProvider,
        actor,
        None,
    )
    .await;
    // NOTE: `issue_invoice_request` itself does NOT re-run
    // `validate_issue_request` — that gate fires at the HTTP handler
    // (`handle_issue_invoice`) BEFORE entering this library helper. So
    // for the helper-level pin, we exercise the validator directly:
    // a malformed value rejected at the route boundary never reaches
    // this layer in production. The route-level coverage is the
    // structural pin (the handler's `validate_issue_request` call site
    // is the seam tested via the existing `issue_route_rejects_empty_lines_*`
    // test family — same code path, different field).
    //
    // Defence in depth: a happy-path issue with an `@`-less override
    // here would land an unusable row; the helper-level result is
    // `Ok` today because the helper trusts the route's validation. The
    // assertion below pins THAT expectation explicitly so a future
    // contributor who moves the gate from route to helper sees the
    // pin (and updates it deliberately).
    let _ = err.expect("library helper trusts route-level validation; happy path with @-less token currently lands");

    // Direct validator invocation — the operator-visible HTTP path.
    use aberp::serve::IssueRequestValidationOutcome;
    let req = fixture_request(Some("not-an-email".to_string()));
    let outcome = aberp::serve::validate_issue_request_for_test(&req);
    assert!(
        matches!(outcome, IssueRequestValidationOutcome::Plain(_)),
        "malformed override must surface as Plain validation error from validate_issue_request"
    );
    if let IssueRequestValidationOutcome::Plain(msg) = outcome {
        assert!(
            msg.contains("email recipient override"),
            "validator message must name the field: got `{msg}`"
        );
    }
}

/// Pin 5 — CR/LF in the override is rejected at the route validator.
/// `partners::validate_email_token` lists `\r` / `\n` in its
/// forbidden-character set; the gate fires BEFORE the issuance commits.
/// This is the operator-blocking version of email_invoice's
/// `validate_no_crlf` (which runs at the SMTP-compose seam as defence
/// in depth).
#[tokio::test(flavor = "current_thread")]
async fn crlf_in_override_rejected_at_validator() {
    use aberp::serve::IssueRequestValidationOutcome;
    let req = fixture_request(Some(
        "buyer@example.com\r\nBcc: attacker@evil.com".to_string(),
    ));
    let outcome = aberp::serve::validate_issue_request_for_test(&req);
    assert!(
        matches!(outcome, IssueRequestValidationOutcome::Plain(_)),
        "CR/LF-bearing override must surface as Plain validation error"
    );
}
