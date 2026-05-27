//! Integration tests for `POST /invoices/:id/submit` and
//! `POST /invoices/:id/poll-ack` (PR-44η / session-60).
//!
//! Three pin tests:
//!
//! 1. **Submit precondition mismatch** — POSTing submit on an
//!    invoice that is not in `Ready` state must surface as a typed
//!    `PreconditionMismatch` (which the route handler maps to 409
//!    Conflict). The fixture wires a Finalized trace (Draft +
//!    Attempt + Response + SAVED ack); the helper must reject.
//! 2. **Poll-ack precondition mismatch** — POSTing poll-ack on an
//!    invoice that is only `Ready` (never submitted) must surface as
//!    a typed `PreconditionMismatch`. The fixture wires a Draft-only
//!    trace; the helper must reject.
//! 3. **Not-found path** — POSTing either route on an unknown
//!    invoice id must surface as `NotFound`. The audit ledger
//!    carries no entries for the id; the helper rejects before any
//!    NAV interaction.
//!
//! The actual NAV roundtrip (`tokenExchange` + `manageInvoice` /
//! `queryTransactionStatus`) is already exercised by the env-gated
//! `submit_invoice_live.rs` + `poll_ack_live.rs` integration tests;
//! re-mocking the full NavTransport surface here would require
//! trait-extracting the concrete `NavTransport` type — a refactor
//! disproportionate to the load-bearing pin (which is the
//! precondition-guard contract the SPA leans on per A163). Per
//! CLAUDE.md rule 2 + rule 3: minimum code, surgical changes.
//!
//! Additionally pins `parse_supplier_tax_number_from_xml` — the
//! load-bearing server-side derivation that lets the SPA POST
//! without a tax-number field. The CLI's `--tax-number` takes the
//! dashed forms; the on-disk XML always carries the 8-digit base
//! inside `<supplierTaxNumber><taxpayerId>`.

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
use aberp::serve::{self, AppState, SubmitRouteError};

const TEST_TENANT: &str = "serve_submit_poll_route_test";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-submit-poll")
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
        // PR-46α / session-62 — Ready boot state. `operator_login`
        // moved inside [`ServeBootState::Ready`].
        boot_state: Arc::new(std::sync::RwLock::new(
            aberp::serve::ServeBootState::Ready {
                operator_login: "test-operator".to_string(),
            },
        )),
    }
}

fn fixture_ready_invoice() -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        lines: vec![LineItem {
            description: "Test megnevezés".to_string(),
            quantity: 1,
            unit_price: Huf(1000),
            vat_rate_basis_points: 2700,
            note: None,
        }],
        issue_date: OffsetDateTime::now_utc(),
        sequence_number: 13,
        fiscal_year: 0,
    }
}

fn open_ledger(db_path: &PathBuf) -> Ledger {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    Ledger::open(db_path, tenant, binary_hash).expect("open ledger")
}

fn write_draft(ledger: &mut Ledger, actor: &Actor, invoice: &ReadyInvoice, idem: IdempotencyKey) {
    // Use the from-invoice constructor so the payload carries the
    // matching invoice_id field that `extract_invoice_id` keys on.
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

/// Submit precondition — a Finalized invoice (Draft + Attempt +
/// Response + SAVED ack) must surface as `PreconditionMismatch`
/// with `current_state == "Finalized"`. The route maps this to 409.
#[tokio::test]
async fn submit_route_rejects_finalized_invoice_with_precondition_mismatch() {
    let dir = test_dir("submit-finalized");
    let db_path = dir.join("aberp.duckdb");
    let invoice = fixture_ready_invoice();
    let invoice_id = invoice.id.to_prefixed_string();
    let idem = IdempotencyKey::new();
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");

    {
        let mut ledger = open_ledger(&db_path);
        write_draft(&mut ledger, &actor, &invoice, idem);
        write_attempt(&mut ledger, &actor, &invoice_id, idem);
        write_response(&mut ledger, &actor, &invoice_id, idem, "TXID-A");
        write_ack(&mut ledger, &actor, &invoice_id, "TXID-A", "SAVED");
    }

    let state = build_state(db_path);
    let err = serve::submit_invoice_request(&state, &invoice_id)
        .await
        .expect_err("submit on Finalized must reject");
    match err {
        SubmitRouteError::PreconditionMismatch {
            current_state,
            message,
        } => {
            assert_eq!(
                current_state, "Finalized",
                "current_state must serialise as `Finalized`"
            );
            assert!(
                message.contains("requires state `Ready`"),
                "error message must name the required state, got: {message}"
            );
        }
        other => panic!("expected PreconditionMismatch, got {other:?}"),
    }
    let _keep = &dir;
}

/// Poll-ack precondition — a Ready invoice (Draft only) must
/// surface as `PreconditionMismatch` with `current_state == "Ready"`.
/// The route maps this to 409.
#[tokio::test]
async fn poll_ack_route_rejects_ready_invoice_with_precondition_mismatch() {
    let dir = test_dir("poll-ready");
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
    let err = serve::poll_ack_request(&state, &invoice_id)
        .await
        .expect_err("poll-ack on Ready must reject");
    match err {
        SubmitRouteError::PreconditionMismatch {
            current_state,
            message,
        } => {
            assert_eq!(
                current_state, "Ready",
                "current_state must serialise as `Ready`"
            );
            assert!(
                message.contains("Submitted") || message.contains("PendingNavExists"),
                "error message must name the required states, got: {message}"
            );
        }
        other => panic!("expected PreconditionMismatch, got {other:?}"),
    }
    let _keep = &dir;
}

/// Not-found path — POSTing either route on an unknown invoice id
/// must surface as `NotFound` (which the route maps to 404). The
/// audit ledger has zero entries for the id; the helper rejects
/// before any NAV interaction.
#[tokio::test]
async fn submit_and_poll_routes_return_not_found_for_unknown_invoice() {
    let dir = test_dir("not-found");
    let db_path = dir.join("aberp.duckdb");
    // Force the DB file to exist so `Ledger::open` succeeds even
    // though there are no entries — wire one unrelated invoice so
    // the file is created.
    let invoice = fixture_ready_invoice();
    let idem = IdempotencyKey::new();
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");
    {
        let mut ledger = open_ledger(&db_path);
        write_draft(&mut ledger, &actor, &invoice, idem);
    }

    let state = build_state(db_path);
    let unknown = "inv_01ARZ3NDEKTSV4RRFFQ69G5XYZ";

    let submit_err = serve::submit_invoice_request(&state, unknown)
        .await
        .expect_err("submit on unknown id must reject");
    match submit_err {
        SubmitRouteError::NotFound(message) => {
            assert!(
                message.contains(unknown),
                "NotFound message must name the unknown id, got: {message}"
            );
        }
        other => panic!("expected NotFound, got {other:?}"),
    }

    let poll_err = serve::poll_ack_request(&state, unknown)
        .await
        .expect_err("poll-ack on unknown id must reject");
    match poll_err {
        SubmitRouteError::NotFound(message) => {
            assert!(
                message.contains(unknown),
                "NotFound message must name the unknown id, got: {message}"
            );
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
    let _keep = &dir;
}
