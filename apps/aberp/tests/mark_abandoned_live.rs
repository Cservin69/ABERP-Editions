//! End-to-end mark-abandoned conformance test (PR-8-2).
//!
//! ENV-GATED on NAV credentials because the setup phase needs a real
//! `submit-invoice` to stage the stuck precondition. The
//! `mark-abandoned` command itself does NOT call NAV — but reaching
//! the `Stuck` precondition requires a live `submit-invoice`, which
//! is what gates the test.
//!
//! Required environment when ABERP_NAV_LIVE_TEST=1:
//!
//!   ABERP_NAV_LIVE_TEST=1
//!   ABERP_NAV_TENANT_ID=<tenant id whose keychain is populated>
//!   ABERP_NAV_TEST_TAX_NUMBER=<dashed full form of the test taxpayer>
//!   ABERP_NAV_TEST_SUPPLIER_NAME=<test-taxpayer business name>
//!
//! Flow:
//!
//!   1. Issue + submit an invoice (real NAV submission).
//!   2. Mark it abandoned via `mark_abandoned::run` (no NAV).
//!   3. Verify post-state: chain verifies; exactly one
//!      `InvoiceMarkedAbandoned` entry exists; payload carries the
//!      prior txid + the operator's reason + the F8 idempotency_key.
//!   4. Try to mark-abandoned the SAME invoice AGAIN — must loud-fail
//!      with the `AlreadyAbandoned` precondition. Defence-in-depth on
//!      the terminal-in-ledger invariant.

use std::fs;
use std::path::PathBuf;

use aberp::audit_payloads::{InvoiceMarkedAbandonedPayload, InvoiceSubmissionResponsePayload};
use aberp::cli::{IssueInvoiceArgs, MarkAbandonedArgs, NavEnv, SubmitInvoiceArgs};
use aberp::{issue_invoice, mark_abandoned, submit_invoice};
use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-mark-abandoned-live-{}-{}-{:?}",
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
      "description": "Live mark-abandoned conformance test widget",
      "quantity": 1,
      "unitPrice": 1000,
      "vatRatePercent": 27
    }}
  ]
}}"#
    )
}

#[test]
fn mark_abandoned_against_stuck_invoice_end_to_end() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping mark_abandoned_against_stuck_invoice_end_to_end \
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

    fs::write(&json_path, build_input_json(&supplier_tax).as_bytes()).expect("write input JSON");
    let issue_args = IssueInvoiceArgs {
        r#in: json_path.clone(),
        out: xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        series: "INV-LIVE-ABANDON".to_string(),

        currency: aberp::cli::CurrencyArg::Huf,
    };
    issue_invoice::run(&issue_args).expect("issue-invoice must succeed");

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
        .expect("InvoiceDraftCreated entry must exist");
    let issuance_idempotency_key = entries
        .iter()
        .find_map(|e| {
            if e.kind == EventKind::InvoiceDraftCreated {
                e.idempotency_key.clone()
            } else {
                None
            }
        })
        .expect("InvoiceDraftCreated must carry idempotency_key");
    drop(ledger_for_lookup);

    let submit_args = SubmitInvoiceArgs {
        invoice_xml: xml_path,
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax,
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        endpoint: NavEnv::Test,
    };
    submit_invoice::run(&submit_args).expect("submit-invoice must succeed");

    // Capture the prior txid (the submission_response's transaction_id)
    // so we can verify the marked-abandoned payload references it.
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
    drop(ledger_after_submit);

    // First mark-abandoned — must succeed.
    let abandon_args = MarkAbandonedArgs {
        invoice_id: invoice_id.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str,
        reason: "live conformance test — abandoned by operator".to_string(),
        // PR-43 / F49: this test runs against an invoice with no
        // prior InvoiceCheckPerformed entry; the guard does not
        // fire, so the override flag is irrelevant.
        force_despite_nav_exists: false,
    };
    mark_abandoned::run(&abandon_args).expect("mark-abandoned must succeed against stuck invoice");

    let ledger = Ledger::open(&db_path, tenant, binary_hash).expect("re-open ledger");
    let verified = ledger
        .verify_chain()
        .expect("chain still verifies after mark-abandoned");
    // 2 issuance + 2 submission + 1 marked_abandoned = ≥5.
    assert!(
        verified >= 5,
        "expected ≥5 entries (2 issuance + 2 submit + 1 marked_abandoned), got {verified}"
    );

    let entries = ledger.entries().expect("read entries");
    let abandoned: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceMarkedAbandoned)
        .collect();
    assert_eq!(
        abandoned.len(),
        1,
        "exactly one InvoiceMarkedAbandoned expected, got {}",
        abandoned.len()
    );

    let abandoned_payload: InvoiceMarkedAbandonedPayload =
        serde_json::from_slice(&abandoned[0].payload).expect("typed marked-abandoned decode");
    assert_eq!(abandoned_payload.invoice_id, invoice_id);
    // PR-19 / ADR-0032 §4: prior_transaction_id is now Option<String>.
    // The state-3 AwaitingAck path (this test) carries Some(txid).
    assert_eq!(
        abandoned_payload.prior_transaction_id.as_deref(),
        Some(prior_txid.as_str())
    );
    assert!(abandoned_payload.reason.contains("live conformance"));
    assert_eq!(
        abandoned_payload.idempotency_key, issuance_idempotency_key,
        "marked_abandoned idempotency_key must equal issuance idempotency_key (F8)"
    );
    assert_eq!(
        abandoned[0].idempotency_key,
        Some(issuance_idempotency_key),
        "marked_abandoned audit-row idempotency_key column must also equal issuance (F8)"
    );

    // Snapshot the tenant string before dropping the ledger handle —
    // the second mark-abandoned call opens its own DuckDB connection
    // and must not contend with this one.
    let tenant_str = ledger.tenant_id().as_str().to_string();
    drop(ledger);

    // Defence-in-depth: a second mark-abandoned on the same invoice
    // must loud-fail. The audit-ledger terminal-by-operator-decision
    // invariant is the load-bearing property here per ADR-0009 §5;
    // if a refactor lets a second abandon land, the ledger no longer
    // has a single point of truth for "operator stopped retrying."
    let abandon_again_args = MarkAbandonedArgs {
        invoice_id,
        db: db_path,
        tenant: tenant_str,
        reason: "second attempt".to_string(),
        // PR-43 / F49: same as above; the AlreadyAbandoned
        // precondition fires before the guard is reached.
        force_despite_nav_exists: false,
    };
    let err = mark_abandoned::run(&abandon_again_args)
        .expect_err("re-abandon must loud-fail with AlreadyAbandoned");
    assert!(
        err.to_string().contains("previously marked abandoned"),
        "expected AlreadyAbandoned message, got {err}"
    );
}
