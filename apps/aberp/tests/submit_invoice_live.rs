//! End-to-end NAV submission conformance test (PR-7-B-3).
//!
//! ENV-GATED. Body runs only when `ABERP_NAV_LIVE_TEST=1` is set;
//! otherwise the test returns early. Matches the PR-7-A
//! `tls_handshake.rs` and the PR-7-B-2 `token_exchange_live.rs`
//! pattern so CI does not need NAV creds and offline contributors do
//! not have a flaky-by-design test.
//!
//! Required environment when ABERP_NAV_LIVE_TEST=1:
//!
//!   ABERP_NAV_LIVE_TEST=1
//!   ABERP_NAV_TENANT_ID=<tenant id whose keychain is populated>
//!   ABERP_NAV_TEST_TAX_NUMBER=<dashed full form of the test taxpayer>
//!   ABERP_NAV_TEST_SUPPLIER_NAME=<test-taxpayer business name>
//!
//! Optional (defaults shown):
//!
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
//!   2. Issue a fixture invoice via the same JSON shape
//!      `fixtures/invoice_minimal.json` uses, calling
//!      `aberp::issue_invoice::run` so the issuance entries are
//!      identical to a production run.
//!   3. Submit it via `aberp::submit_invoice::run`.
//!   4. Re-open the audit ledger and verify post-state: that
//!      `verify_chain()` succeeds; that two new `EventKind` entries
//!      are present (`InvoiceSubmissionAttempt` and
//!      `InvoiceSubmissionResponse`); that the
//!      `InvoiceSubmissionResponse` payload's `transaction_id` is
//!      non-empty; and that every NAV-submission entry shares the
//!      same `idempotency_key` as the prior issuance entries (F8).

use std::fs;
use std::path::PathBuf;

use aberp::audit_payloads::{InvoiceSubmissionAttemptPayload, InvoiceSubmissionResponsePayload};
use aberp::cli::{IssueInvoiceArgs, NavEnv, SubmitInvoiceArgs};
use aberp::{issue_invoice, submit_invoice};
use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-submit-live-{}-{}-{:?}",
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

    // Hand-built JSON via format! is acceptable here because every
    // interpolated value is operator-supplied env data, not arbitrary
    // user input. The production audit payloads go through
    // serde_json::to_vec on typed structs per F9 — that contract
    // is unchanged by this test fixture.
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
      "description": "Live conformance test widget",
      "quantity": 1,
      "unitPrice": 1000,
      "vatRatePercent": 27
    }}
  ]
}}"#
    )
}

#[test]
fn submit_invoice_against_api_test_end_to_end() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping submit_invoice_against_api_test_end_to_end \
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

    // 1. Write the input JSON fixture.
    let input_json = build_input_json(&supplier_tax);
    fs::write(&json_path, input_json.as_bytes()).expect("write input JSON");

    // 2. Issue via the binary's own orchestration.
    let issue_args = IssueInvoiceArgs {
        r#in: json_path.clone(),
        out: xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        series: "INV-LIVE-TEST".to_string(),

        currency: aberp::cli::CurrencyArg::Huf,
    };
    issue_invoice::run(&issue_args).expect("issue-invoice must succeed");

    // Read the issued invoice id back out of the audit ledger so we
    // can pass it to submit-invoice without re-parsing the binary's
    // stdout. The first InvoiceDraftCreated entry is the one we just
    // wrote (the DB was freshly created above).
    let tenant = TenantId::new(tenant_id_str.clone()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]); // placeholder; the ledger
                                                         // reads existing rows regardless.
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

    // 3. Submit.
    let submit_args = SubmitInvoiceArgs {
        invoice_xml: xml_path,
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax,
        db: db_path.clone(),
        tenant: tenant_id_str,
        endpoint: NavEnv::Test,
    };
    submit_invoice::run(&submit_args).expect("submit-invoice must succeed against api-test");

    // 4. Re-open the ledger and verify the post-state.
    let ledger = Ledger::open(&db_path, tenant, binary_hash).expect("re-open ledger");
    let verified = ledger
        .verify_chain()
        .expect("chain still verifies after submit");
    assert!(
        verified >= 4,
        "expected ≥4 entries (2 issuance + 2 submission), got {verified}"
    );

    let entries = ledger.entries().expect("read entries");

    let attempts: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceSubmissionAttempt)
        .collect();
    assert_eq!(
        attempts.len(),
        1,
        "exactly one InvoiceSubmissionAttempt expected, got {}",
        attempts.len()
    );

    let responses: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceSubmissionResponse)
        .collect();
    assert_eq!(
        responses.len(),
        1,
        "exactly one InvoiceSubmissionResponse expected, got {}",
        responses.len()
    );

    // Parse the submission_response payload — transaction_id must be
    // non-empty (NAV always assigns one on OK).
    let response_payload: InvoiceSubmissionResponsePayload =
        serde_json::from_slice(&responses[0].payload).expect("typed response payload decode");
    assert!(
        !response_payload.transaction_id.is_empty(),
        "NAV transactionId must be persisted"
    );
    assert_eq!(response_payload.invoice_id, invoice_id);

    // The F8 contract: every NAV-related entry for this invoice
    // carries the SAME idempotency_key as the issuance entries.
    assert_eq!(
        attempts[0].idempotency_key,
        Some(issuance_idempotency_key.clone()),
        "submission_attempt idempotency_key must equal issuance idempotency_key (F8)"
    );
    assert_eq!(
        responses[0].idempotency_key,
        Some(issuance_idempotency_key.clone()),
        "submission_response idempotency_key must equal issuance idempotency_key (F8)"
    );

    // Verify the attempt payload also encodes the right invoice_id +
    // endpoint label (defence-in-depth on the typed-payload shape).
    let attempt_payload: InvoiceSubmissionAttemptPayload =
        serde_json::from_slice(&attempts[0].payload).expect("typed attempt payload decode");
    assert_eq!(attempt_payload.invoice_id, invoice_id);
    assert_eq!(attempt_payload.endpoint, "test");
    assert!(!attempt_payload.request_xml.is_empty());
}
