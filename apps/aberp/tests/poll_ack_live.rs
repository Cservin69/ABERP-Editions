//! End-to-end NAV poll-ack conformance test (PR-7-C-2).
//!
//! ENV-GATED. Body runs only when `ABERP_NAV_LIVE_TEST=1` is set;
//! otherwise the test returns early. Matches the PR-7-A
//! `tls_handshake.rs`, the PR-7-B-2 `token_exchange_live.rs`, and the
//! PR-7-B-3 `submit_invoice_live.rs` shapes so CI does not need NAV
//! creds and offline contributors do not have a flaky-by-design test.
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
//!   2. Issue a fixture invoice via `aberp::issue_invoice::run`.
//!   3. Submit it via `aberp::submit_invoice::run`.
//!   4. Poll the ack via `aberp::poll_ack::run`.
//!   5. Re-open the audit ledger and verify post-state:
//!        - chain verifies cleanly
//!        - at least one `InvoiceAckStatus` entry exists
//!        - the LAST `InvoiceAckStatus` entry's parsed `ack_status` is
//!          one of the four NAV enum values
//!        - the typed payload's `transaction_id` matches the one persisted
//!          by the prior `InvoiceSubmissionResponse`
//!
//! What this test does NOT assert:
//!
//!   - It does NOT pin a specific terminal status. NAV's `api-test`
//!     environment routinely returns `RECEIVED` / `PROCESSING` for
//!     several seconds before SAVED, so a `SAVED` assertion would be
//!     flaky-by-design. The test asserts the *shape* of the poll
//!     evidence; the terminal-state advance (Finalized/Rejected/Stuck)
//!     is exercised by the binary's stdout print, which the operator
//!     reads.

use std::fs;
use std::path::PathBuf;

use aberp::audit_payloads::{InvoiceAckStatusPayload, InvoiceSubmissionResponsePayload};
use aberp::cli::{IssueInvoiceArgs, NavEnv, PollAckArgs, SubmitInvoiceArgs};
use aberp::{issue_invoice, poll_ack, submit_invoice};
use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-poll-ack-live-{}-{}-{:?}",
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

    // Same `format!`-built JSON pattern as submit_invoice_live.rs: every
    // interpolated value is operator-supplied env data, not arbitrary
    // user input. The production audit payloads go through
    // serde_json::to_vec on typed structs per F9 — unchanged here.
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
      "description": "Live poll-ack conformance test widget",
      "quantity": 1,
      "unitPrice": 1000,
      "vatRatePercent": 27
    }}
  ]
}}"#
    )
}

#[test]
fn poll_ack_against_api_test_end_to_end() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping poll_ack_against_api_test_end_to_end \
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

    // 1. Write input JSON fixture.
    let input_json = build_input_json(&supplier_tax);
    fs::write(&json_path, input_json.as_bytes()).expect("write input JSON");

    // 2. Issue.
    let issue_args = IssueInvoiceArgs {
        r#in: json_path.clone(),
        out: xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        series: "INV-LIVE-POLL".to_string(),

        currency: aberp::cli::CurrencyArg::Huf,
    };
    issue_invoice::run(&issue_args).expect("issue-invoice must succeed");

    // Look up the invoice id from the freshly-created ledger.
    let tenant = TenantId::new(tenant_id_str.clone()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]); // placeholder; ledger
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
    drop(ledger_for_lookup);

    // 3. Submit.
    let submit_args = SubmitInvoiceArgs {
        invoice_xml: xml_path,
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        endpoint: NavEnv::Test,
    };
    submit_invoice::run(&submit_args).expect("submit-invoice must succeed against api-test");

    // Resolve the persisted transactionId out of the InvoiceSubmissionResponse
    // entry — same source poll_ack uses internally. We capture it here
    // independently so the test can later assert that the AckStatus
    // payload references THIS specific txid (defence-in-depth).
    let ledger_after_submit =
        Ledger::open(&db_path, tenant.clone(), binary_hash).expect("open ledger after submit");
    let entries_after_submit = ledger_after_submit.entries().expect("read entries");
    let persisted_txid = entries_after_submit
        .iter()
        .rev()
        .find_map(|e| {
            if e.kind == EventKind::InvoiceSubmissionResponse {
                let payload: InvoiceSubmissionResponsePayload =
                    serde_json::from_slice(&e.payload).ok()?;
                if payload.invoice_id == invoice_id {
                    Some(payload.transaction_id)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .expect("InvoiceSubmissionResponse must carry the txid");
    assert!(
        !persisted_txid.is_empty(),
        "persisted transaction id must be non-empty"
    );
    drop(ledger_after_submit);

    // 4. Poll-ack.
    let poll_args = PollAckArgs {
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax,
        db: db_path.clone(),
        tenant: tenant_id_str,
        endpoint: NavEnv::Test,
    };
    poll_ack::run(&poll_args).expect("poll-ack must succeed against api-test");

    // 5. Re-open the ledger and verify the post-state.
    let ledger = Ledger::open(&db_path, tenant, binary_hash).expect("re-open ledger");
    let verified = ledger
        .verify_chain()
        .expect("chain still verifies after poll-ack");
    // 2 issuance + 2 submission + ≥1 poll = ≥5 entries.
    assert!(
        verified >= 5,
        "expected ≥5 entries (2 issuance + 2 submission + ≥1 poll), got {verified}"
    );

    let entries = ledger.entries().expect("read entries");

    let acks: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceAckStatus)
        .collect();
    assert!(
        !acks.is_empty(),
        "at least one InvoiceAckStatus entry expected after poll-ack"
    );

    // Inspect the LAST ack entry: shape, status, txid reference.
    let last_ack = acks.last().expect("non-empty by the assertion above");
    let last_payload: InvoiceAckStatusPayload =
        serde_json::from_slice(&last_ack.payload).expect("typed ack payload decode");
    assert_eq!(
        last_payload.invoice_id, invoice_id,
        "last InvoiceAckStatus payload must reference the submitted invoice id"
    );
    assert_eq!(
        last_payload.transaction_id, persisted_txid,
        "last InvoiceAckStatus payload must reference the same transactionId as the prior InvoiceSubmissionResponse"
    );
    assert!(
        !last_payload.response_xml.is_empty(),
        "last InvoiceAckStatus payload must carry the verbatim NAV response_xml"
    );

    // ack_status must be one of the four NAV enum values per ADR-0009 §2.
    // (Note: this is a STRING in the audit payload because the
    // InvoiceAckStatusPayload schema is stable across PR-7-B-3 and
    // PR-7-C-2; the typed-enum parse happens upstream in
    // `aberp_nav_transport::operations::query_transaction_status::ProcessingStatus`.)
    let allowed = ["RECEIVED", "PROCESSING", "SAVED", "ABORTED"];
    assert!(
        allowed.contains(&last_payload.ack_status.as_str()),
        "last ack_status `{}` must be one of {:?}",
        last_payload.ack_status,
        allowed
    );

    // Every ack's invoice_id and transactionId match this invoice — no
    // cross-invoice contamination. Sanity for a future PR that handles
    // multiple invoices in one ledger.
    for ack in &acks {
        let p: InvoiceAckStatusPayload =
            serde_json::from_slice(&ack.payload).expect("typed ack payload decode");
        assert_eq!(p.invoice_id, invoice_id);
        assert_eq!(p.transaction_id, persisted_txid);
    }
}
