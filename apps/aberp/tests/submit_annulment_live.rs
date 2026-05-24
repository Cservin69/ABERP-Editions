//! End-to-end NAV submit-annulment conformance test (PR-13, ADR-0026).
//!
//! ENV-GATED. Body runs only when `ABERP_NAV_LIVE_TEST=1` is set;
//! otherwise the test returns early. Matches the existing PR-7-A /
//! PR-7-B-2 / PR-7-B-3 / PR-8-2 live-test pattern so CI does not
//! need NAV creds and offline contributors do not have a flaky-by-
//! design test.
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
//!   2. Issue + submit a fixture invoice (mirrors
//!      `submit_invoice_live.rs` for steps 1-3).
//!   3. Wait for SAVED via `poll-ack` (some testbed environments are
//!      asynchronous enough to need it; PR-13 keeps the test
//!      minimal and submits the annulment regardless of the prior
//!      ack — ADR-0025 §6 permits annulment of Rejected / Stuck
//!      bases too, so we don't gate on ack outcome).
//!   4. Run `request-technical-annulment` against the issued invoice
//!      to produce the on-disk `<InvoiceAnnulment>` XML + the
//!      operator-decision audit entry.
//!   5. Run `submit-annulment` — this PR's load-bearing surface.
//!   6. Re-open the audit ledger and verify post-state:
//!      - `verify_chain()` succeeds.
//!      - Exactly one `InvoiceAnnulmentSubmissionAttempt` entry.
//!      - Exactly one `InvoiceAnnulmentSubmissionResponse` entry.
//!      - The response payload's `transaction_id` is non-empty.
//!      - Both new entries carry the annulment-request's
//!        idempotency_key (ADR-0026 §"F8 contract").

use std::fs;
use std::path::PathBuf;

use aberp::audit_payloads::{
    InvoiceAnnulmentSubmissionAttemptPayload, InvoiceAnnulmentSubmissionResponsePayload,
    InvoiceTechnicalAnnulmentRequestedPayload,
};
use aberp::cli::{
    AnnulmentCode, IssueInvoiceArgs, NavEnv, RequestTechnicalAnnulmentArgs, SubmitAnnulmentArgs,
    SubmitInvoiceArgs,
};
use aberp::{issue_invoice, request_technical_annulment, submit_annulment, submit_invoice};
use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-submit-annulment-live-{}-{}-{:?}",
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
      "description": "Live annulment conformance test widget",
      "quantity": 1,
      "unitPrice": 1000,
      "vatRatePercent": 27
    }}
  ]
}}"#
    )
}

#[test]
fn submit_annulment_against_api_test_end_to_end() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping submit_annulment_against_api_test_end_to_end \
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
    let invoice_xml_path = temp_path("invoice.xml");
    let annulment_xml_path = temp_path("annulment.xml");

    // 1. Write the input JSON fixture.
    let input_json = build_input_json(&supplier_tax);
    fs::write(&json_path, input_json.as_bytes()).expect("write input JSON");

    // 2. Issue + submit the base invoice via the same orchestration
    //    the binary uses. This establishes the prior
    //    InvoiceSubmissionResponse entry that
    //    request-technical-annulment will look for as its
    //    precondition (ADR-0025 §6).
    let issue_args = IssueInvoiceArgs {
        r#in: json_path.clone(),
        out: invoice_xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        series: "INV-ANNUL-LIVE-TEST".to_string(),

        currency: aberp::cli::CurrencyArg::Huf,
    };
    issue_invoice::run(&issue_args).expect("issue-invoice must succeed");

    let tenant = TenantId::new(tenant_id_str.clone()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    let ledger = Ledger::open(&db_path, tenant.clone(), binary_hash).expect("open ledger");
    let entries = ledger.entries().expect("read entries");
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
    drop(ledger);

    let submit_args = SubmitInvoiceArgs {
        invoice_xml: invoice_xml_path,
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        endpoint: NavEnv::Test,
    };
    submit_invoice::run(&submit_args).expect("submit-invoice must succeed against api-test");

    // 3. Skip explicit poll-ack — ADR-0025 §6 permits annulment of
    //    bases in any post-submission state (SAVED, ABORTED, Stuck).
    //    The submit-annulment precondition only requires a prior
    //    InvoiceTechnicalAnnulmentRequested entry (ADR-0026 §6),
    //    which step 4 below creates.

    // 4. Run request-technical-annulment to produce the on-disk
    //    annulment XML + the operator-decision audit entry.
    let request_args = RequestTechnicalAnnulmentArgs {
        references: invoice_id.clone(),
        code: AnnulmentCode::ErraticData,
        reason: "live conformance test — withdrawing the test submission".to_string(),
        out: annulment_xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
    };
    request_technical_annulment::run(&request_args)
        .expect("request-technical-annulment must succeed");

    // Capture the annulment-request's idempotency key for the F8
    // contract assertion below.
    let ledger = Ledger::open(&db_path, tenant.clone(), binary_hash).expect("re-open ledger");
    let entries = ledger.entries().expect("read entries");
    let request_payload: InvoiceTechnicalAnnulmentRequestedPayload = entries
        .iter()
        .find(|e| e.kind == EventKind::InvoiceTechnicalAnnulmentRequested)
        .map(|e| {
            serde_json::from_slice(&e.payload)
                .expect("typed decode of InvoiceTechnicalAnnulmentRequestedPayload")
        })
        .expect("annulment-request entry must exist after request-technical-annulment");
    let annulment_idem = request_payload.idempotency_key.clone();
    drop(ledger);

    // 5. Run submit-annulment. This is PR-13's load-bearing
    //    surface — the wire call to NAV's manageAnnulment endpoint.
    let submit_annulment_args = SubmitAnnulmentArgs {
        annulment_xml: annulment_xml_path,
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax,
        db: db_path.clone(),
        tenant: tenant_id_str,
        endpoint: NavEnv::Test,
    };
    submit_annulment::run(&submit_annulment_args)
        .expect("submit-annulment must succeed against api-test");

    // 6. Re-open the ledger and verify the post-state.
    let ledger = Ledger::open(&db_path, tenant, binary_hash).expect("re-open ledger");
    let verified = ledger
        .verify_chain()
        .expect("chain still verifies after submit-annulment");
    assert!(
        verified >= 6,
        "expected >=6 entries (2 issuance + 2 invoice-submit + 1 annulment-request + 2 annulment-submit), got {verified}"
    );

    let entries = ledger.entries().expect("read entries");

    let attempts: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceAnnulmentSubmissionAttempt)
        .collect();
    assert_eq!(
        attempts.len(),
        1,
        "exactly one InvoiceAnnulmentSubmissionAttempt expected, got {}",
        attempts.len()
    );

    let responses: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceAnnulmentSubmissionResponse)
        .collect();
    assert_eq!(
        responses.len(),
        1,
        "exactly one InvoiceAnnulmentSubmissionResponse expected, got {}",
        responses.len()
    );

    // The response payload's transaction_id must be non-empty —
    // NAV always assigns one on OK.
    let response_payload: InvoiceAnnulmentSubmissionResponsePayload =
        serde_json::from_slice(&responses[0].payload).expect("typed response payload decode");
    assert!(
        !response_payload.transaction_id.is_empty(),
        "NAV annulment transactionId must be persisted"
    );
    assert_eq!(response_payload.invoice_id, invoice_id);

    // The F8 contract per ADR-0026 §"F8 contract": both wire-
    // evidence entries carry the annulment-request's idempotency
    // key, NOT a fresh per-wire-submission key. This is the load-
    // bearing audit-evidence-bundle link from the wire entries
    // back to the operator-decision entry.
    let attempt_payload: InvoiceAnnulmentSubmissionAttemptPayload =
        serde_json::from_slice(&attempts[0].payload).expect("typed attempt payload decode");
    assert_eq!(
        attempt_payload.idempotency_key, annulment_idem,
        "annulment-wire-attempt idempotency_key must equal annulment-request idempotency_key (ADR-0026 §F8)"
    );
    assert_eq!(
        response_payload.idempotency_key, annulment_idem,
        "annulment-wire-response idempotency_key must equal annulment-request idempotency_key (ADR-0026 §F8)"
    );
    assert_eq!(attempt_payload.invoice_id, invoice_id);
    assert_eq!(attempt_payload.endpoint, "test");
    assert!(!attempt_payload.request_xml.is_empty());
}
