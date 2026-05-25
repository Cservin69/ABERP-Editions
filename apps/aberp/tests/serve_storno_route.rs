//! Integration tests for `POST /api/invoices/:id/storno` (PR-47α /
//! session-64).
//!
//! Three pin tests:
//!
//! 1. **Storno precondition mismatch (not Finalized)** — POSTing
//!    storno on an invoice that is only `Ready` (never submitted) must
//!    surface as a typed `PreconditionMismatch` (which the route
//!    handler maps to 409 Conflict). The fixture wires a Draft-only
//!    trace; `storno_invoice_request` must reject before reaching the
//!    audit-ledger sibling-input-JSON read.
//! 2. **Storno precondition mismatch (Submitted, not yet Finalized)**
//!    — POSTing storno on an invoice that submitted but has no SAVED
//!    ack yet must surface as `PreconditionMismatch` with
//!    `current_state == "Submitted"`. Defence in depth: an operator
//!    racing the poll-ack against the storno-button click must not
//!    burn a storno sequence number against a still-pending base.
//! 3. **Not-found path** — POSTing the storno route on an unknown
//!    invoice id must surface as `NotFound`. The audit ledger carries
//!    no entries for the id; the helper rejects before any DB write.
//!
//! The actual storno-issuance happy path is exercised by the existing
//! `tests/issue_storno_local.rs` (CLI surface) — `storno_from_inputs`
//! is the same code path with the operator's Actor minted at the
//! call site; re-exercising that here would only add the
//! sibling-input-JSON read seam on top of the existing coverage. The
//! sibling-path discipline itself is covered by the pure-Rust
//! `sibling_input_json_path` round-trip test in the serve.rs unit
//! tests module (added at this PR).

#![allow(clippy::too_many_arguments)]

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::{
    CustomerId, Huf, IdempotencyKey, InvoiceId, LineItem, ReadyInvoice, SeriesId,
};
use time::OffsetDateTime;
use ulid::Ulid;

use aberp::audit_payloads::{
    InvoiceAckStatusPayload, InvoiceDraftCreatedPayload, InvoiceSubmissionAttemptPayload,
    InvoiceSubmissionResponsePayload,
};
use aberp::serve::{self, AppState, SubmitRouteError};

const TEST_TENANT: &str = "serve_storno_route_test";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-storno")
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
    let payload = InvoiceSubmissionAttemptPayload::new(invoice_id, idem, "test", b"<req/>".to_vec());
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

/// Storno precondition — a Ready invoice (Draft only) must surface as
/// `PreconditionMismatch` with `current_state == "Ready"`. The route
/// maps this to 409. Defence against the operator clicking storno on
/// an unsubmitted invoice — the ADR-0023 §1 ladder rejects every non-
/// Finalized state with a named reason, but the route-level guard
/// fires first so the operator gets a 409 (operator-actionable) rather
/// than a 500 from the precondition classifier.
#[test]
fn storno_route_rejects_ready_invoice_with_precondition_mismatch() {
    let dir = test_dir("storno-ready");
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
    let err = serve::storno_invoice_request(&state, &invoice_id)
        .expect_err("storno on Ready must reject");
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
                message.contains("requires state `Finalized`"),
                "error message must name the required state, got: {message}"
            );
        }
        other => panic!("expected PreconditionMismatch, got {other:?}"),
    }
    let _keep = &dir;
}

/// Storno precondition — a Submitted invoice (Draft + Attempt +
/// Response, no SAVED ack yet) must surface as `PreconditionMismatch`
/// with `current_state == "Submitted"`. Belt-and-braces guard for the
/// poll-ack-vs-storno race the operator could trigger by clicking
/// faster than the modal can refetch.
#[test]
fn storno_route_rejects_submitted_invoice_with_precondition_mismatch() {
    let dir = test_dir("storno-submitted");
    let db_path = dir.join("aberp.duckdb");
    let invoice = fixture_ready_invoice();
    let invoice_id = invoice.id.to_prefixed_string();
    let idem = IdempotencyKey::new();
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");

    {
        let mut ledger = open_ledger(&db_path);
        write_draft(&mut ledger, &actor, &invoice, idem);
        write_attempt(&mut ledger, &actor, &invoice_id, idem);
        write_response(&mut ledger, &actor, &invoice_id, idem, "TXID-S");
    }

    let state = build_state(db_path);
    let err = serve::storno_invoice_request(&state, &invoice_id)
        .expect_err("storno on Submitted must reject");
    match err {
        SubmitRouteError::PreconditionMismatch {
            current_state,
            message,
        } => {
            assert_eq!(
                current_state, "Submitted",
                "current_state must serialise as `Submitted`"
            );
            assert!(
                message.contains("requires state `Finalized`"),
                "error message must name the required state, got: {message}"
            );
        }
        other => panic!("expected PreconditionMismatch, got {other:?}"),
    }
    let _keep = &dir;
}

/// Not-found path — POSTing the storno route on an unknown invoice id
/// must surface as `NotFound` (which the route maps to 404). The audit
/// ledger has zero entries for the id; the helper rejects before any
/// DB write.
///
/// PR-47α / session-64 — third precondition pin: defence against the
/// race where an operator clicks storno on a list-cached id whose
/// underlying ledger row has been hand-removed (or never existed).
#[test]
fn storno_route_returns_not_found_for_unknown_invoice() {
    let dir = test_dir("storno-not-found");
    let db_path = dir.join("aberp.duckdb");
    // Force the DB file to exist so `Ledger::open` succeeds even
    // though there are no entries — wire one unrelated invoice so the
    // file is created.
    let invoice = fixture_ready_invoice();
    let idem = IdempotencyKey::new();
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");
    {
        let mut ledger = open_ledger(&db_path);
        write_draft(&mut ledger, &actor, &invoice, idem);
        write_attempt(
            &mut ledger,
            &actor,
            &invoice.id.to_prefixed_string(),
            idem,
        );
        write_response(
            &mut ledger,
            &actor,
            &invoice.id.to_prefixed_string(),
            idem,
            "TXID-NF",
        );
        // Mark the unrelated invoice as Finalized so the trace is not
        // empty even though no entry references the unknown id we hit
        // the route with.
        write_ack(
            &mut ledger,
            &actor,
            &invoice.id.to_prefixed_string(),
            "TXID-NF",
            "SAVED",
        );
    }

    let state = build_state(db_path);
    let unknown = "inv_01ARZ3NDEKTSV4RRFFQ69G5XYZ";

    let storno_err = serve::storno_invoice_request(&state, unknown)
        .expect_err("storno on unknown id must reject");
    match storno_err {
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
