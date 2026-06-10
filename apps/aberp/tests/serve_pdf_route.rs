//! Integration tests for the `GET /invoices/:id/pdf` route added at
//! PR-44ε.UI / session-58.
//!
//! Two pin tests:
//!
//! 1. **Happy path** — `get_invoice_pdf` returns `Some(rendered)` for a
//!    wired invoice id; the `pdf_bytes` parse as a real PDF
//!    (via `pdf-extract`), the `invoice_number` round-trips off the
//!    NAV body, and the bytes are non-empty.
//! 2. **Missing-id path** — `get_invoice_pdf` returns `None` for an
//!    invoice id that has no `InvoiceDraftCreated` audit entry. This
//!    is the discriminator the route uses to emit 404 vs 200.
//!
//! The full HTTP layer (status code emission, Content-Type / Content-
//! Disposition header bytes) is structural — axum's `into_response`
//! constructs the response from the `(headers, body)` tuple the
//! handler returns; pinning it would couple the test to axum's
//! private response-building details. Per CLAUDE.md rule 2 (minimum
//! code) the discriminator at the `get_invoice_pdf` level is the
//! load-bearing pin; the route handler is a thin wrapper.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::{
    Currency, CustomerId, Huf, IdempotencyKey, InvoiceId, LineItem, RateMetadata, ReadyInvoice,
    SeriesCode, SeriesId,
};
use rust_decimal::Decimal;
use time::macros::date;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp::audit_payloads::InvoiceDraftCreatedPayload;
use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
use aberp::serve::{self, AppState};

const TEST_TENANT: &str = "serve_pdf_route_test";

/// Per-test scratch dir under the OS temp root. Same posture as
/// `print_invoice_render.rs::test_dir` per A155: a fresh ULID-suffixed
/// directory per fixture, leaked at end-of-test.
fn test_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir()
            .join("aberp-serve-pdf")
            .join(format!("{}-{}", label, Ulid::new()));
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn fixture_seller_toml(dir: &Path) -> PathBuf {
    let p = dir.join("seller.toml");
    fs::write(
        &p,
        r#"[seller]
bank_account_number = "12345678-12345678-12345678"
iban = "HU12 1234 5678 9012 3456 7890"
bank_name = "OTP Bank Nyrt."
swift_bic = "OTPVHUHB"
"#,
    )
    .expect("write seller.toml");
    p
}

fn fixture_parties() -> NavParties {
    NavParties {
        supplier: SupplierInfo {
            tax_number: "12345678-1-42".to_string(),
            name: "ABERP Test Kft.".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1234".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Test utca 1.".to_string(),
        },
        customer: CustomerInfo {
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit
            // Domestic posture for legacy test fixtures.
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("87654321-2-13".to_string()),
            name: "Vevő Kft.".to_string(),
            // PR-77 / session-101 — `customerAddress` required for any
            // DOMESTIC customerVatStatus per NAV business-rule
            // `CUSTOMER_DATA_EXPECTED`.
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Váci utca 19.".to_string(),
            }),
        },
    }
}

fn fixture_ready_invoice() -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        lines: vec![LineItem {
            description: "Test megnevezés".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(1000),
            vat_rate_basis_points: 2700,
            note: None,
            unit: None,
        }],
        issue_date: OffsetDateTime::now_utc(),
        // PR-84 — fixture defaults both date fields to issue date.
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
        sequence_number: 13,
        fiscal_year: 0,
    }
}

struct WiredInvoice {
    dir: PathBuf,
    db_path: PathBuf,
    invoice_id: String,
    seller_toml: PathBuf,
}

fn wire_invoice(label: &str, currency: Currency, rate: Option<&RateMetadata>) -> WiredInvoice {
    let dir = test_dir(label);
    let db_path = dir.join("aberp.duckdb");
    let xml_path = dir.join("invoice.xml");

    let invoice = fixture_ready_invoice();
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = fixture_parties();

    let xml = nav_xml::render_invoice_data(&invoice, &series, &parties, currency, rate)
        .expect("render NAV XML");
    fs::write(&xml_path, &xml).expect("write NAV XML");

    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    let mut ledger = Ledger::open(&db_path, tenant, binary_hash).expect("open ledger");

    let idempotency_key = IdempotencyKey::new();
    let payload = if let Some(rate) = rate {
        InvoiceDraftCreatedPayload::from_invoice_with_rate(
            &invoice,
            idempotency_key,
            Some(xml_path.clone()),
            currency,
            rate,
        )
    } else {
        InvoiceDraftCreatedPayload::from_invoice_with_xml_path(
            &invoice,
            idempotency_key,
            xml_path.clone(),
        )
    };
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");
    ledger
        .append(
            EventKind::InvoiceDraftCreated,
            payload.to_bytes(),
            actor,
            Some(idempotency_key.to_canonical_string()),
        )
        .expect("append InvoiceDraftCreated");

    let seller_toml = fixture_seller_toml(&dir);

    WiredInvoice {
        invoice_id: invoice.id.to_prefixed_string(),
        dir,
        db_path,
        seller_toml,
    }
}

fn build_state(wired: &WiredInvoice) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    AppState {
        db_path: Arc::new(wired.db_path.clone()),
        tenant,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
        session_token: Arc::new("test-token".to_string()),
        // PR-46α / session-62 — Ready boot state (see
        // `serve_setup_nav_credentials_route.rs` for the
        // NeedsSetup-path coverage).
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

// ──────────────────────────────────────────────────────────────────────
// Pin tests
// ──────────────────────────────────────────────────────────────────────

/// Happy path — `get_invoice_pdf` returns `Some(rendered)` whose
/// `pdf_bytes` parse as a real PDF with non-empty extracted text.
/// Mirrors the CLI-side `print_invoice_render::eur_invoice_renders_*`
/// pin but at the route surface: the route shares the orchestrator
/// (per the PR-44ε.UI refactor — `run` and `get_invoice_pdf` both
/// call `print_invoice::render_to_bytes`), so a regression that
/// breaks the route's bytes also surfaces here.
#[test]
fn invoice_pdf_route_returns_pdf_bytes_for_existing_invoice() {
    let rate = RateMetadata {
        rate: Decimal::from_str("356.69").unwrap(),
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        huf_equivalent_total: 453,
    };
    let wired = wire_invoice("happy", Currency::Eur, Some(&rate));
    let state = build_state(&wired);

    let result = serve::get_invoice_pdf(&state, &wired.invoice_id, Some(&wired.seller_toml))
        .expect("get_invoice_pdf returns Ok");
    let rendered = result.expect("get_invoice_pdf returns Some for wired invoice");
    assert!(
        !rendered.pdf_bytes.is_empty(),
        "rendered PDF bytes must be non-empty"
    );
    assert!(
        rendered.invoice_number.contains("INV-default"),
        "invoice_number `{}` must reflect the NAV body's <invoiceNumber>",
        rendered.invoice_number,
    );
    // Parse the bytes as a PDF — a malformed body fails here loud.
    let text = pdf_extract::extract_text_from_mem(&rendered.pdf_bytes)
        .expect("pdf-extract parses rendered bytes as a PDF");
    assert!(
        text.contains("EUR"),
        "expected EUR currency in rendered PDF text:\n{text}"
    );
    // Keep the per-test scratch dir alive until here so the on-disk
    // NAV XML the renderer reads is not pruned by an early `drop`.
    let _keep = &wired.dir;
}

/// Missing-id path — `get_invoice_pdf` returns `Ok(None)` for an id
/// that has no `InvoiceDraftCreated` audit entry. The route uses
/// this discriminator to emit 404 vs 200 per the same shape as
/// `/invoices/:id`. A regression that propagated the not-found
/// branch as `Err` would surface as a 500 with a confusing message
/// rather than the operator-actionable 404.
#[test]
fn invoice_pdf_route_returns_none_for_unknown_invoice_id() {
    // Wire a real invoice so the ledger exists; query a different id.
    let wired = wire_invoice("missing", Currency::Huf, None);
    let state = build_state(&wired);

    let result = serve::get_invoice_pdf(&state, "inv_NOT_A_REAL_ID", Some(&wired.seller_toml))
        .expect("get_invoice_pdf returns Ok even for missing id");
    assert!(
        result.is_none(),
        "get_invoice_pdf must return None for an id with no draft entry"
    );
}
