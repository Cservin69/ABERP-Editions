//! End-to-end NAV poll-annulment-ack conformance test (PR-14, ADR-0027).
//!
//! ENV-GATED. Body runs only when `ABERP_NAV_LIVE_TEST=1` is set;
//! otherwise the test returns early. Matches the existing PR-7-A /
//! PR-7-B-2 / PR-7-B-3 / PR-8-2 / PR-13 live-test pattern so CI
//! does not need NAV creds and offline contributors do not have a
//! flaky-by-design test.
//!
//! Required environment when ABERP_NAV_LIVE_TEST=1 — same set as
//! `submit_annulment_live.rs`:
//!
//!   ABERP_NAV_LIVE_TEST=1
//!   ABERP_NAV_TENANT_ID=<tenant id whose keychain is populated>
//!   ABERP_NAV_TEST_TAX_NUMBER=<dashed full form of the test taxpayer>
//!   ABERP_NAV_TEST_SUPPLIER_NAME=<test-taxpayer business name>
//!
//! Flow:
//!
//!   1. Issue + submit a fixture invoice.
//!   2. `request-technical-annulment` to produce the on-disk XML.
//!   3. `submit-annulment` to POST to NAV's `manageAnnulment` and
//!      record the annulment-side `transactionId` in the audit
//!      ledger.
//!   4. `poll-annulment-ack` — this PR's load-bearing surface.
//!      Drives the bounded poll loop against NAV's
//!      `queryTransactionStatus` keyed on the annulment-side
//!      transactionId.
//!   5. Re-open the audit ledger and verify post-state:
//!      - `verify_chain()` succeeds.
//!      - At least one `InvoiceAnnulmentAckStatus` entry exists.
//!      - Every `InvoiceAnnulmentAckStatus` entry's
//!        `transaction_id` matches the annulment-side wire txid
//!        from PR-13's `InvoiceAnnulmentSubmissionResponse`
//!        (NOT the base invoice's submission txid — that's the
//!        load-bearing distinction ADR-0027 §2 + §3 names).
//!      - Every `InvoiceAnnulmentAckStatus` entry's `invoice_id`
//!        matches the base invoice id.
//!      - Every `InvoiceAnnulmentAckStatus` entry's `ack_status`
//!        is one of the four NAV v3.0 values.

use std::fs;
use std::path::PathBuf;

use aberp::audit_payloads::{
    InvoiceAnnulmentAckStatusPayload, InvoiceAnnulmentSubmissionResponsePayload,
};
use aberp::cli::{
    AnnulmentCode, IssueInvoiceArgs, NavEnv, PollAnnulmentAckArgs,
    RequestTechnicalAnnulmentArgs, SubmitAnnulmentArgs, SubmitInvoiceArgs,
};
use aberp::{
    issue_invoice, poll_annulment_ack, request_technical_annulment, submit_annulment,
    submit_invoice,
};
use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-poll-annulment-ack-live-{}-{}-{:?}",
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
      "description": "Live annulment-poll conformance test widget",
      "quantity": 1,
      "unitPrice": 1000,
      "vatRatePercent": 27
    }}
  ]
}}"#
    )
}

#[test]
fn poll_annulment_ack_against_api_test_end_to_end() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping poll_annulment_ack_against_api_test_end_to_end \
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

    // 1. Issue + submit the base invoice.
    let input_json = build_input_json(&supplier_tax);
    fs::write(&json_path, input_json.as_bytes()).expect("write input JSON");

    let issue_args = IssueInvoiceArgs {
        r#in: json_path.clone(),
        out: invoice_xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        series: "INV-ANNUL-POLL-LIVE-TEST".to_string(),

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

    // 2. request-technical-annulment.
    let request_args = RequestTechnicalAnnulmentArgs {
        references: invoice_id.clone(),
        code: AnnulmentCode::ErraticData,
        reason: "live conformance test — withdrawing the test submission for the poll path"
            .to_string(),
        out: annulment_xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
    };
    request_technical_annulment::run(&request_args)
        .expect("request-technical-annulment must succeed");

    // 3. submit-annulment — records the annulment-side
    //    transactionId in the audit ledger; that txid is what
    //    PR-14's poll keys on (NOT the base invoice's
    //    submission txid).
    let submit_annulment_args = SubmitAnnulmentArgs {
        annulment_xml: annulment_xml_path,
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        endpoint: NavEnv::Test,
    };
    submit_annulment::run(&submit_annulment_args)
        .expect("submit-annulment must succeed against api-test");

    // Capture the annulment-side wire transactionId so we can
    // assert that the poll entries reference IT (and not the
    // base invoice's submission txid) — load-bearing per
    // ADR-0027 §3 / §2.
    let ledger = Ledger::open(&db_path, tenant.clone(), binary_hash).expect("re-open ledger");
    let entries = ledger.entries().expect("read entries");
    let wire_response_payload: InvoiceAnnulmentSubmissionResponsePayload = entries
        .iter()
        .rev()
        .find_map(|e| {
            if e.kind == EventKind::InvoiceAnnulmentSubmissionResponse {
                serde_json::from_slice(&e.payload).ok()
            } else {
                None
            }
        })
        .expect("InvoiceAnnulmentSubmissionResponse entry must exist after submit-annulment");
    let annulment_wire_txid = wire_response_payload.transaction_id.clone();
    assert!(
        !annulment_wire_txid.is_empty(),
        "NAV annulment transactionId must be non-empty after submit-annulment"
    );
    drop(ledger);

    // 4. poll-annulment-ack — the PR-14 load-bearing surface.
    let poll_args = PollAnnulmentAckArgs {
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax,
        db: db_path.clone(),
        tenant: tenant_id_str,
        endpoint: NavEnv::Test,
    };
    poll_annulment_ack::run(&poll_args)
        .expect("poll-annulment-ack must succeed against api-test");

    // 5. Re-open the ledger; verify post-state.
    let ledger = Ledger::open(&db_path, tenant, binary_hash).expect("re-open ledger");
    let verified = ledger
        .verify_chain()
        .expect("chain still verifies after poll-annulment-ack");
    assert!(
        verified >= 7,
        "expected >=7 entries (2 issuance + 2 invoice-submit + 1 annulment-request + 2 annulment-submit + >=1 annulment-poll), got {verified}"
    );

    let entries = ledger.entries().expect("read entries");
    let polls: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceAnnulmentAckStatus)
        .collect();
    assert!(
        !polls.is_empty(),
        "expected at least one InvoiceAnnulmentAckStatus entry after poll-annulment-ack"
    );

    // Every poll entry must reference the ANNULMENT-side wire
    // transactionId (NOT the base invoice's submission txid) —
    // load-bearing per ADR-0027 §3 + §2. A future refactor that
    // accidentally polled the base invoice's submission txid
    // would write entries pointing at the wrong txid; this
    // assertion catches it loud.
    for (i, entry) in polls.iter().enumerate() {
        let payload: InvoiceAnnulmentAckStatusPayload =
            serde_json::from_slice(&entry.payload).expect("typed annulment-poll payload decode");
        assert_eq!(
            payload.transaction_id, annulment_wire_txid,
            "poll entry #{i} must reference the annulment-side wire transactionId (ADR-0027 §3 / §2)"
        );
        assert_eq!(
            payload.invoice_id, invoice_id,
            "poll entry #{i} must reference the base invoice id"
        );
        // ack_status must be one of the four NAV v3.0 values.
        let acks = ["RECEIVED", "PROCESSING", "SAVED", "ABORTED"];
        assert!(
            acks.contains(&payload.ack_status.as_str()),
            "poll entry #{i} ack_status `{}` not in NAV v3.0 enum",
            payload.ack_status
        );
        assert!(
            !payload.response_xml.is_empty(),
            "poll entry #{i} response_xml must carry verbatim NAV response bytes (ADR-0009 §8)"
        );
    }
}
