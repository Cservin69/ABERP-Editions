//! End-to-end NAV retry-submission conformance test (PR-8-1).
//!
//! ENV-GATED. Body runs only when `ABERP_NAV_LIVE_TEST=1` is set;
//! otherwise the test returns early. Matches the
//! `submit_invoice_live.rs` / `poll_ack_live.rs` shapes.
//!
//! Required environment when ABERP_NAV_LIVE_TEST=1:
//!
//!   ABERP_NAV_LIVE_TEST=1
//!   ABERP_NAV_TENANT_ID=<tenant id whose keychain is populated>
//!   ABERP_NAV_TEST_TAX_NUMBER=<dashed full form of the test taxpayer>
//!   ABERP_NAV_TEST_SUPPLIER_NAME=<test-taxpayer business name>
//!
//! Optional (defaults shown):
//!   ABERP_NAV_TEST_SUPPLIER_COUNTRY=HU
//!   ABERP_NAV_TEST_SUPPLIER_POSTAL=1011
//!   ABERP_NAV_TEST_SUPPLIER_CITY=Budapest
//!   ABERP_NAV_TEST_SUPPLIER_STREET="Fő utca 1."
//!   ABERP_NAV_TEST_CUSTOMER_TAX=87654321-2-21
//!   ABERP_NAV_TEST_CUSTOMER_NAME="Test Customer Zrt."
//!
//! Flow:
//!
//!   1. Build a temp DuckDB at a per-process unique path.
//!   2. Issue + submit a fixture invoice (issue_invoice +
//!      submit_invoice). DO NOT run poll-ack — leaving the invoice
//!      with a `submission_response` but no `ack_status` puts it in
//!      the audit-query "stuck (None last ack)" classification, which
//!      is one of the two legitimate retry preconditions per
//!      audit_query.rs.
//!   3. Run `retry_submission::run` against api-test.
//!   4. Re-open the audit ledger and verify post-state:
//!        - chain still verifies
//!        - exactly one `InvoiceRetryRequested` entry exists, payload
//!          references the prior txid and the operator-supplied reason
//!        - one extra `InvoiceSubmissionAttempt` + one extra
//!          `InvoiceSubmissionResponse` entry exist (the retry)
//!        - the retry's `InvoiceSubmissionResponse.transaction_id` is
//!          non-empty (NAV always assigns one on OK), and the F8
//!          contract — every retry-side entry carries the same
//!          idempotency_key as the original issuance — holds.
//!
//! What this test does NOT assert:
//!
//!   - It does NOT assert that the new txid differs from the prior
//!     txid. NAV's api-test may legitimately collapse to the same
//!     txid on rapid re-submission of an identical InvoiceData body;
//!     the test asserts that a *response* arrived, not that it
//!     differs from the prior one.

use std::fs;
use std::path::PathBuf;

use aberp::audit_payloads::{InvoiceRetryRequestedPayload, InvoiceSubmissionResponsePayload};
use aberp::cli::{IssueInvoiceArgs, NavEnv, RetrySubmissionArgs, SubmitInvoiceArgs};
use aberp::{issue_invoice, retry_submission, submit_invoice};
use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-retry-submission-live-{}-{}-{:?}",
        std::process::id(),
        tag,
        std::thread::current().id(),
    ));
    p
}

fn build_input_json(supplier_tax: &str) -> String {
    let supplier_name = std::env::var("ABERP_NAV_TEST_SUPPLIER_NAME")
        .expect("ABERP_NAV_TEST_SUPPLIER_NAME must be set when ABERP_NAV_LIVE_TEST=1");
    let country = std::env::var("ABERP_NAV_TEST_SUPPLIER_COUNTRY").unwrap_or_else(|_| "HU".into());
    let postal = std::env::var("ABERP_NAV_TEST_SUPPLIER_POSTAL").unwrap_or_else(|_| "1011".into());
    let city = std::env::var("ABERP_NAV_TEST_SUPPLIER_CITY").unwrap_or_else(|_| "Budapest".into());
    let street =
        std::env::var("ABERP_NAV_TEST_SUPPLIER_STREET").unwrap_or_else(|_| "Fő utca 1.".into());
    let customer_tax =
        std::env::var("ABERP_NAV_TEST_CUSTOMER_TAX").unwrap_or_else(|_| "87654321-2-21".into());
    let customer_name = std::env::var("ABERP_NAV_TEST_CUSTOMER_NAME")
        .unwrap_or_else(|_| "Test Customer Zrt.".into());
    format!(
        r#"{{
  "supplier": {{
    "taxNumber": "{supplier_tax}",
    "name": "{supplier_name}",
    "address": {{
      "countryCode": "{country}",
      "postalCode": "{postal}",
      "city": "{city}",
      "street": "{street}"
    }}
  }},
  "customer": {{
    "taxNumber": "{customer_tax}",
    "name": "{customer_name}"
  }},
  "lines": [
    {{
      "description": "Live retry-submission conformance test widget",
      "quantity": 1,
      "unitPrice": 1000,
      "vatRatePercent": 27
    }}
  ]
}}"#
    )
}

#[test]
fn retry_submission_against_api_test_end_to_end() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping retry_submission_against_api_test_end_to_end \
             (set ABERP_NAV_LIVE_TEST=1 + ABERP_NAV_TENANT_ID + \
             ABERP_NAV_TEST_TAX_NUMBER + ABERP_NAV_TEST_SUPPLIER_NAME \
             to run)"
        );
        return;
    }

    let tenant_id_str =
        std::env::var("ABERP_NAV_TENANT_ID").expect("ABERP_NAV_TENANT_ID must be set");
    let supplier_tax =
        std::env::var("ABERP_NAV_TEST_TAX_NUMBER").expect("ABERP_NAV_TEST_TAX_NUMBER must be set");

    let db_path = temp_path("db.duckdb");
    let json_path = temp_path("input.json");
    let xml_path = temp_path("nav.xml");

    // 1. Write input JSON fixture + issue invoice.
    fs::write(&json_path, build_input_json(&supplier_tax).as_bytes()).expect("write input JSON");
    let issue_args = IssueInvoiceArgs {
        r#in: json_path.clone(),
        out: xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        series: "INV-LIVE-RETRY".to_string(),

        currency: aberp::cli::CurrencyArg::Huf,
    };
    issue_invoice::run(&issue_args).expect("issue-invoice must succeed");

    // Look up the invoice id from the freshly-created ledger.
    let tenant = TenantId::new(tenant_id_str.clone()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    let ledger_for_lookup =
        Ledger::open(&db_path, tenant.clone(), binary_hash).expect("open ledger after issue");
    let entries = ledger_for_lookup.entries().expect("read entries");
    let invoice_id = entries
        .iter()
        .find_map(|e| {
            if e.kind == EventKind::InvoiceDraftCreated {
                let v: serde_json::Value = serde_json::from_slice(&e.payload).ok()?;
                v.get("invoice_id")
                    .and_then(|x| x.as_str())
                    .map(String::from)
            } else {
                None
            }
        })
        .expect("InvoiceDraftCreated entry must exist after issue-invoice");
    let issuance_idempotency_key = entries
        .iter()
        .find_map(|e| {
            if e.kind == EventKind::InvoiceDraftCreated {
                e.idempotency_key.clone()
            } else {
                None
            }
        })
        .expect("InvoiceDraftCreated entry must carry idempotency_key");
    drop(ledger_for_lookup);

    // 2. Submit (no poll). The submission_response without an ack
    //    leaves the invoice in the audit_query "Stuck (None last ack)"
    //    classification — legitimate retry precondition.
    let submit_args = SubmitInvoiceArgs {
        invoice_xml: xml_path.clone(),
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        endpoint: NavEnv::Test,
    };
    submit_invoice::run(&submit_args).expect("submit-invoice must succeed");

    // Capture the prior transactionId for cross-checking after retry.
    let ledger_after_submit =
        Ledger::open(&db_path, tenant.clone(), binary_hash).expect("open ledger after submit");
    let entries_after_submit = ledger_after_submit.entries().expect("read entries");
    let prior_txid = entries_after_submit
        .iter()
        .rev()
        .find_map(|e| {
            if e.kind == EventKind::InvoiceSubmissionResponse {
                let p: InvoiceSubmissionResponsePayload =
                    serde_json::from_slice(&e.payload).ok()?;
                if p.invoice_id == invoice_id {
                    Some(p.transaction_id)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .expect("submission_response must carry prior txid");
    assert!(!prior_txid.is_empty(), "prior txid must be non-empty");
    drop(ledger_after_submit);

    // 3. Retry-submission.
    let retry_args = RetrySubmissionArgs {
        invoice_xml: xml_path,
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax,
        db: db_path.clone(),
        tenant: tenant_id_str,
        endpoint: NavEnv::Test,
        reason: "live conformance test — operator initiated".to_string(),
    };
    retry_submission::run(&retry_args).expect("retry-submission must succeed against api-test");

    // 4. Re-open ledger; verify post-state.
    let ledger = Ledger::open(&db_path, tenant, binary_hash).expect("re-open ledger");
    let verified = ledger
        .verify_chain()
        .expect("chain still verifies after retry");
    // Issuance (2) + submit (2) + retry (3 — retry_requested + attempt + response) = ≥7.
    assert!(
        verified >= 7,
        "expected ≥7 entries (2 issuance + 2 submit + 3 retry), got {verified}"
    );

    let entries = ledger.entries().expect("read entries");

    // exactly one InvoiceRetryRequested
    let retry_requested: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceRetryRequested)
        .collect();
    assert_eq!(
        retry_requested.len(),
        1,
        "exactly one InvoiceRetryRequested expected, got {}",
        retry_requested.len()
    );

    // payload references prior txid + reason text + F8 idempotency_key
    let retry_payload: InvoiceRetryRequestedPayload =
        serde_json::from_slice(&retry_requested[0].payload)
            .expect("typed retry-requested payload decode");
    assert_eq!(retry_payload.invoice_id, invoice_id);
    // PR-19 / ADR-0032 §4: prior_transaction_id is now Option<String>.
    // The state-3 AwaitingAck path (this test) carries Some(txid).
    assert_eq!(
        retry_payload.prior_transaction_id.as_deref(),
        Some(prior_txid.as_str())
    );
    assert!(retry_payload.reason.contains("live conformance"));
    assert_eq!(
        retry_payload.idempotency_key, issuance_idempotency_key,
        "retry_requested idempotency_key must equal issuance idempotency_key (F8)"
    );

    // two InvoiceSubmissionAttempt (original + retry) and two InvoiceSubmissionResponse
    let attempts: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceSubmissionAttempt)
        .collect();
    assert_eq!(
        attempts.len(),
        2,
        "expected 2 InvoiceSubmissionAttempt (original + retry), got {}",
        attempts.len()
    );
    let responses: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceSubmissionResponse)
        .collect();
    assert_eq!(
        responses.len(),
        2,
        "expected 2 InvoiceSubmissionResponse (original + retry), got {}",
        responses.len()
    );

    // The retry's response carries a non-empty txid + the F8 key.
    let retry_response_payload: InvoiceSubmissionResponsePayload =
        serde_json::from_slice(&responses[1].payload).expect("typed retry-response decode");
    assert!(
        !retry_response_payload.transaction_id.is_empty(),
        "retry response must carry a non-empty transaction_id"
    );
    assert_eq!(
        responses[1].idempotency_key,
        Some(issuance_idempotency_key.clone()),
        "retry submission_response idempotency_key must equal issuance idempotency_key (F8)"
    );

    // The retry's attempt also carries the F8 key.
    assert_eq!(
        attempts[1].idempotency_key,
        Some(issuance_idempotency_key),
        "retry submission_attempt idempotency_key must equal issuance idempotency_key (F8)"
    );
}
