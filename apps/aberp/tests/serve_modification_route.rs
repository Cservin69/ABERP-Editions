//! Integration tests for `POST /api/invoices/:id/modification` (PR-47β
//! / session-65).
//!
//! Three pin tests:
//!
//! 1. **Modification precondition mismatch (Ready)** — POSTing
//!    modification on an invoice that is only `Ready` (never submitted)
//!    must surface as a typed `PreconditionMismatch` (which the route
//!    handler maps to 409 Conflict). The fixture wires a Draft-only
//!    trace; `modification_invoice_request` must reject before reaching
//!    any DB write.
//! 2. **C6 currency mismatch (400 BadRequest)** — POSTing a body with
//!    a currency different from the base's stored currency must
//!    surface as `BadRequest`. Defence against a curl bypass of the
//!    SPA's locked currency dropdown; without this guard the core
//!    would silently override the operator-supplied currency inside
//!    `run_single_tx` via `inherit_rate_metadata_for_chain`, masking
//!    the operator's mistake from any audit-visible signal.
//! 3. **Not-found path** — POSTing the modification route on an
//!    unknown invoice id must surface as `NotFound`. The audit ledger
//!    carries no entries for the id; the helper rejects before any
//!    DB write.
//!
//! The actual modification-issuance happy path is exercised by the
//! existing `tests/issue_modification_local.rs` (CLI surface) —
//! `modification_from_inputs` is the same code path with the
//! operator's Actor minted at the call site; re-exercising that here
//! would only add the C6 currency-read seam on top of the existing
//! coverage, and that seam is covered by the BadRequest pin below.

#![allow(clippy::too_many_arguments)]

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::{CustomerId, Huf, IdempotencyKey, InvoiceId, LineItem, ReadyInvoice, SeriesId};
use time::OffsetDateTime;
use ulid::Ulid;

use aberp::audit_payloads::{
    InvoiceAckStatusPayload, InvoiceDraftCreatedPayload, InvoiceSubmissionAttemptPayload,
    InvoiceSubmissionResponsePayload,
};
use aberp::issue_invoice::{AddressJson, CustomerJson, LineJson, SupplierJson};
use aberp::nav_xml::CustomerVatStatus;
use aberp::serve::{self, AppState, ModificationInvoiceRequest, ModificationRouteError};

const TEST_TENANT: &str = "serve_modification_route_test";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-modification")
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

fn fixture_request_body(currency: aberp_billing::Currency) -> ModificationInvoiceRequest {
    ModificationInvoiceRequest {
        customer: CustomerJson {
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit
            // Domestic posture for legacy test fixtures.
            vat_status: CustomerVatStatus::Domestic,
            partner_id: None,
            tax_number: "87654321-2-13".to_string(),
            name: "Test Buyer Kft.".to_string(),
            // PR-77 / session-101 — preflight requires customer.address.
            address: Some(AddressJson {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Váci utca 19.".to_string(),
            }),
        },
        lines: vec![LineJson {
            description: "Corrected line".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            unit_price: 1500,
            vat_rate_percent: 27,
            note: None,
            unit: None,
        }],
        currency,
        modification_date: "2026-05-24".to_string(),
        series: None,
        // S184 — the post-issue tail is FIRE-AND-FORGET; the
        // precondition pin tests in this file fail BEFORE the spawn
        // would fire (the modification_invoice_request errors on
        // precondition before returning Ok). Pass `Some(false)` for
        // defence in depth — if a future test fixture upgrades to a
        // happy-path body, the auto-tail will not try to reach SMTP /
        // NAV from the precondition-test surface.
        email_buyer_on_modification: Some(false),
        submit_to_nav_on_modification: Some(false),
        // PR-203 / S203 — modification fixture leaves the override unset;
        // the resolver falls back to partner.email (today's behaviour).
        email_recipient_override: None,
    }
}

fn open_ledger(db_path: &PathBuf) -> Ledger {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    Ledger::open(db_path, tenant, binary_hash).expect("open ledger")
}

fn write_draft(ledger: &mut Ledger, actor: &Actor, invoice: &ReadyInvoice, idem: IdempotencyKey) {
    let payload = InvoiceDraftCreatedPayload::from_invoice(invoice, idem);
    ledger
        .append(
            EventKind::InvoiceDraftCreated,
            payload.to_bytes(),
            actor.clone(),
            Some(idem.to_canonical_string()),
        )
        .expect("append InvoiceDraftCreated");
}

fn write_attempt(ledger: &mut Ledger, actor: &Actor, invoice_id: &str, idem: IdempotencyKey) {
    let payload =
        InvoiceSubmissionAttemptPayload::new(invoice_id, idem, "test", b"<req/>".to_vec());
    ledger
        .append(
            EventKind::InvoiceSubmissionAttempt,
            payload.to_bytes(),
            actor.clone(),
            Some(idem.to_canonical_string()),
        )
        .expect("append InvoiceSubmissionAttempt");
}

fn write_response(
    ledger: &mut Ledger,
    actor: &Actor,
    invoice_id: &str,
    idem: IdempotencyKey,
    txid: &str,
) {
    let payload = InvoiceSubmissionResponsePayload::new(invoice_id, idem, txid, b"<res/>".to_vec());
    ledger
        .append(
            EventKind::InvoiceSubmissionResponse,
            payload.to_bytes(),
            actor.clone(),
            Some(idem.to_canonical_string()),
        )
        .expect("append InvoiceSubmissionResponse");
}

fn write_ack(ledger: &mut Ledger, actor: &Actor, invoice_id: &str, txid: &str, status: &str) {
    let payload = InvoiceAckStatusPayload::new(invoice_id, txid, status, b"<ack/>".to_vec());
    ledger
        .append(
            EventKind::InvoiceAckStatus,
            payload.to_bytes(),
            actor.clone(),
            None,
        )
        .expect("append InvoiceAckStatus");
}

// ──────────────────────────────────────────────────────────────────────
// Pin tests
// ──────────────────────────────────────────────────────────────────────

/// Modification precondition — a Ready invoice (Draft only) must
/// surface as `PreconditionMismatch` with `current_state == "Ready"`.
/// ADR-0024 §6 requires the base to be `Finalized` OR `Amended` before
/// a modification can issue; the route's loud-fail at the 409 boundary
/// matches the SPA's `buttonsForState` table per the A163 mirror
/// invariant.
#[test]
fn modification_route_rejects_ready_invoice_with_precondition_mismatch() {
    let dir = test_dir("modification-ready");
    let db_path = dir.join("aberp.duckdb");
    let invoice = fixture_ready_invoice();
    let invoice_id = invoice.id.to_prefixed_string();
    let idem = IdempotencyKey::new();
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");

    {
        let mut ledger = open_ledger(&db_path);
        write_draft(&mut ledger, &actor, &invoice, idem);
    }

    let state = build_state(db_path);
    let body = fixture_request_body(aberp_billing::Currency::Huf);
    let err = serve::modification_invoice_request(&state, &invoice_id, body)
        .expect_err("modification on Ready must reject");
    match err {
        ModificationRouteError::PreconditionMismatch {
            current_state,
            message,
        } => {
            assert_eq!(
                current_state, "Ready",
                "current_state must serialise as `Ready`"
            );
            assert!(
                message.contains("`Finalized` or `Amended`"),
                "error message must name the required states, got: {message}"
            );
        }
        other => panic!("expected PreconditionMismatch, got {other:?}"),
    }
    let _keep = &dir;
}

/// Modification C6 invariant — a Finalized HUF base + a body claiming
/// EUR currency must surface as `BadRequest`. The route's 400 is the
/// defence-in-depth complement to the SPA's locked currency dropdown:
/// without this guard, the core would silently override the body's
/// currency inside `run_single_tx`, leaving the operator's mistake
/// invisible until the printed-invoice render diverged from
/// expectations. CLAUDE.md rule 12: fail loud.
#[tokio::test(flavor = "current_thread")]
async fn modification_route_rejects_c6_currency_mismatch_with_bad_request() {
    let dir = test_dir("modification-c6");
    let db_path = dir.join("aberp.duckdb");
    // A Finalized base must exist BOTH in the audit ledger AND in the
    // billing table for the C6 read to succeed (the route opens a
    // billing-side tx to fetch the row's currency column). Use the
    // CLI library helper to mint a fresh local invoice end-to-end so
    // the billing row + audit trace are co-created in lockstep.
    let xml_out = dir.join("base.xml");
    aberp::issue_invoice::issue_from_parsed(
        aberp::issue_invoice::InvoiceInputJson {
            supplier: SupplierJson {
                tax_number: "12345678-1-42".to_string(),
                name: "Test Supplier Kft.".to_string(),
                address: AddressJson {
                    country_code: "HU".to_string(),
                    postal_code: "1011".to_string(),
                    city: "Budapest".to_string(),
                    street: "Fő utca 1.".to_string(),
                },
            },
            customer: CustomerJson {
                // PR-97 / ADR-0048 — preserve pre-PR-97 implicit
                // Domestic posture for legacy test fixtures.
                vat_status: CustomerVatStatus::Domestic,
                partner_id: None,
                tax_number: "87654321-2-13".to_string(),
                name: "Test Buyer Kft.".to_string(),
                // PR-77 / session-101 — base invoice must carry an address so
                // its emitted NAV body passes the strengthened validator.
                address: Some(AddressJson {
                    country_code: "HU".to_string(),
                    postal_code: "1052".to_string(),
                    city: "Budapest".to_string(),
                    street: "Váci utca 19.".to_string(),
                }),
            },
            lines: vec![LineJson {
                description: "Base line".to_string(),
                quantity: rust_decimal::Decimal::from(1),
                unit_price: 1000,
                vat_rate_percent: 27,
                note: None,
                unit: None,
            }],
            invoice_note: None,
            // PR-84 — fixture leaves all three date fields `None`; the
            // issuance pipeline defaults payment_deadline + delivery_date
            // to the system issue date.
            payment_deadline: None,
            delivery_date: None,
            delivery_date_override: None,
            // S160 — fixture uses the default payment method (Transfer).
            payment_method: aberp_billing::PaymentMethod::default(),
            // PR-203 / S203 — fixture leaves the override unset; the
            // base's resolver continues to fall back to partner.email.
            email_recipient_override: None,
        },
        &db_path,
        TEST_TENANT,
        "INV-default",
        aberp_billing::Currency::Huf,
        xml_out,
        Actor::from_local_cli("sess".to_string(), "test-user"),
        &NeverProvider,
        // S392 — base fixture does not exercise the NAV number pre-flight.
        None,
        None,
    )
    .await
    .expect("issue base HUF invoice");

    // Find the base id from the audit ledger.
    let base_invoice_id = {
        let ledger = open_ledger(&db_path);
        let entries = ledger.entries().expect("ledger entries");
        let mut id: Option<String> = None;
        for entry in &entries {
            if entry.kind == EventKind::InvoiceDraftCreated {
                let parsed: InvoiceDraftCreatedPayload =
                    serde_json::from_slice(&entry.payload).expect("parse draft payload");
                id = Some(parsed.invoice_id);
                break;
            }
        }
        id.expect("base invoice id from ledger")
    };

    // Finalize the base so the precondition guard passes (Finalized
    // accepts modification). Submission + SAVED ack land via direct
    // ledger appends.
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");
    let txid = "TXID-C6";
    let idem = IdempotencyKey::new();
    {
        let mut ledger = open_ledger(&db_path);
        write_attempt(&mut ledger, &actor, &base_invoice_id, idem);
        write_response(&mut ledger, &actor, &base_invoice_id, idem, txid);
        write_ack(&mut ledger, &actor, &base_invoice_id, txid, "SAVED");
    }

    // Now POST a modification body claiming EUR even though the base
    // is stored as HUF; the route must reject with BadRequest naming
    // the C6 invariant.
    let state = build_state(db_path);
    let body = fixture_request_body(aberp_billing::Currency::Eur);
    let err = serve::modification_invoice_request(&state, &base_invoice_id, body)
        .expect_err("modification with mismatched currency must reject");
    match err {
        ModificationRouteError::BadRequest(message) => {
            assert!(
                message.contains("C6"),
                "BadRequest message must name the C6 invariant, got: {message}"
            );
            assert!(
                message.contains("EUR") && message.contains("HUF"),
                "BadRequest message must name both currencies, got: {message}"
            );
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
    let _keep = &dir;
}

/// Not-found path — POSTing the modification route on an unknown
/// invoice id must surface as `NotFound`.
#[test]
fn modification_route_returns_not_found_for_unknown_invoice() {
    let dir = test_dir("modification-not-found");
    let db_path = dir.join("aberp.duckdb");
    // Force the DB file to exist with one unrelated invoice so
    // `Ledger::open` succeeds (the unknown-id walk needs a real
    // tenant DB).
    let invoice = fixture_ready_invoice();
    let idem = IdempotencyKey::new();
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");
    {
        let mut ledger = open_ledger(&db_path);
        write_draft(&mut ledger, &actor, &invoice, idem);
    }

    let state = build_state(db_path);
    let unknown = "inv_01ARZ3NDEKTSV4RRFFQ69G5XYZ";
    let body = fixture_request_body(aberp_billing::Currency::Huf);
    let err = serve::modification_invoice_request(&state, unknown, body)
        .expect_err("modification on unknown id must reject");
    match err {
        ModificationRouteError::NotFound(message) => {
            assert!(
                message.contains(unknown),
                "NotFound message must name the unknown id, got: {message}"
            );
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
    let _keep = &dir;
}

/// Stand-in MnbRatesProvider that never gets called — the C6
/// fixture issues a HUF invoice (no rate fetch on the HUF branch
/// per ADR-0037 §1).
struct NeverProvider;
#[async_trait::async_trait]
impl aberp::mnb_rates_provider::MnbRatesProvider for NeverProvider {
    async fn fetch_official_rate(
        &self,
        _currency: aberp_billing::Currency,
        _date: time::Date,
    ) -> std::result::Result<aberp_mnb_rates::MnbRate, aberp_mnb_rates::MnbError> {
        unreachable!("HUF issuance path never consults the rate provider")
    }
}
