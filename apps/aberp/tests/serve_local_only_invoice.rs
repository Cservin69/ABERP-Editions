//! S434 — integration pins for the NAV-off "LOCAL ONLY" invoice path.
//!
//! A NAV-disabled tenant (`AppState::nav_enabled == false`) must never
//! POST to NAV. Instead `submit_invoice_request` short-circuits: it writes
//! ONE `InvoiceLocalOnlyEmitted` ledger row (so `derive_state` returns
//! `LocalOnly`) and returns an outcome flagged `local_only == true`. The
//! PDF + audit trail are produced by the normal issuance path; only the
//! NAV wire send is skipped.
//!
//! This is the library-level slice of the [[customer-journey-e2e-gate]]:
//! issue (Draft in the ledger) → submit under NAV-off → LocalOnly, with
//! the dedupe/idempotency guard proving `derive_state` reads the new row.
//!
//! The harness mirrors `serve_submit_poll_route.rs` (same Ready-invoice
//! fixture + ledger writers); only `nav_enabled` differs.

#![allow(clippy::too_many_arguments)]

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::{CustomerId, Huf, IdempotencyKey, InvoiceId, LineItem, ReadyInvoice, SeriesId};
use time::OffsetDateTime;
use ulid::Ulid;

use aberp::audit_payloads::InvoiceDraftCreatedPayload;
use aberp::serve::{self, AppState};

const TEST_TENANT: &str = "serve_local_only_invoice_test";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-local-only")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// `nav_enabled` is the only knob this test cares about — the rest mirrors
/// the standard `serve_submit_poll_route.rs` Ready AppState fixture.
fn build_state(db_path: PathBuf, nav_enabled: bool) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    AppState {
        db_path: Arc::new(db_path),
        tenant,
        nav_enabled,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(binary_hash),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        nav_poll_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
            aberp::serve::NAV_POLL_DAEMON_CONCURRENCY,
        )),
        boot_state: Arc::new(std::sync::RwLock::new(
            aberp::serve::ServeBootState::Ready {
                operator_login: serve::NAV_DISABLED_LOGIN.to_string(),
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

fn fixture_ready_invoice() -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        lines: vec![LineItem {
            description: "CNC machining".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(9000),
            vat_rate_basis_points: 0,
            note: None,
            unit: None,
        }],
        issue_date: OffsetDateTime::now_utc(),
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
        sequence_number: 1,
        fiscal_year: 0,
    }
}

fn open_ledger(db_path: &PathBuf) -> Ledger {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    Ledger::open(db_path, tenant, binary_hash).expect("open ledger")
}

fn write_ready_draft(db_path: &PathBuf, invoice: &ReadyInvoice) {
    let idem = IdempotencyKey::new();
    let actor = Actor::from_local_cli("sess".to_string(), "demo-operator");
    let mut ledger = open_ledger(db_path);
    let payload = InvoiceDraftCreatedPayload::from_invoice(invoice, idem);
    ledger
        .append(
            EventKind::InvoiceDraftCreated,
            payload.to_bytes(),
            actor,
            Some(idem.to_canonical_string()),
        )
        .expect("append InvoiceDraftCreated");
}

fn count_local_only(db_path: &PathBuf) -> usize {
    open_ledger(db_path)
        .entries()
        .expect("read ledger entries")
        .into_iter()
        .filter(|e| e.kind == EventKind::InvoiceLocalOnlyEmitted)
        .count()
}

/// Customer-journey slice: a Ready invoice issued under a NAV-disabled
/// tenant, submitted, becomes LocalOnly — one ledger row, never a NAV POST,
/// and the outcome is flagged `local_only`.
#[tokio::test]
async fn nav_off_submit_marks_invoice_local_only() {
    let dir = test_dir("nav-off-submit");
    let db_path = dir.join("aberp.duckdb");
    let invoice = fixture_ready_invoice();
    let invoice_id = invoice.id.to_prefixed_string();
    write_ready_draft(&db_path, &invoice);

    let state = build_state(db_path.clone(), /* nav_enabled */ false);
    let outcome = serve::submit_invoice_request(&state, &invoice_id)
        .await
        .expect("NAV-off submit must succeed (local-only)");
    assert!(
        outcome.local_only,
        "outcome must be flagged local_only for a NAV-off tenant"
    );
    assert!(
        outcome.transaction_id.is_empty(),
        "a local-only invoice has no NAV transactionId"
    );
    assert_eq!(
        count_local_only(&db_path),
        1,
        "exactly one InvoiceLocalOnlyEmitted row must be written"
    );
    let _keep = &dir;
}

/// Idempotency + `derive_state` proof: a SECOND submit of an
/// already-LocalOnly invoice short-circuits (still `local_only`) WITHOUT a
/// second ledger row. The short-circuit only fires when `derive_state`
/// reads the prior `InvoiceLocalOnlyEmitted` row as `LocalOnly`, so this
/// pins the derive ladder too.
#[tokio::test]
async fn nav_off_resubmit_is_idempotent() {
    let dir = test_dir("nav-off-resubmit");
    let db_path = dir.join("aberp.duckdb");
    let invoice = fixture_ready_invoice();
    let invoice_id = invoice.id.to_prefixed_string();
    write_ready_draft(&db_path, &invoice);

    let state = build_state(db_path.clone(), false);
    serve::submit_invoice_request(&state, &invoice_id)
        .await
        .expect("first NAV-off submit");
    let second = serve::submit_invoice_request(&state, &invoice_id)
        .await
        .expect("second NAV-off submit must succeed idempotently");
    assert!(second.local_only, "re-submit still reports local_only");
    assert_eq!(
        count_local_only(&db_path),
        1,
        "re-submit must NOT write a second InvoiceLocalOnlyEmitted row"
    );
    let _keep = &dir;
}
