//! Env-gated negative test for ADR-0022's submit-time XSD invariant
//! check.
//!
//! ENV-GATED. Body runs only when `ABERP_NAV_LIVE_TEST=1` is set,
//! matching the pattern of `submit_invoice_live.rs` /
//! `retry_submission_live.rs` / `mark_abandoned_live.rs`. CI without
//! NAV creds does not exercise this.
//!
//! # What it pins
//!
//! ADR-0022 §"Wiring into the existing pipelines" §2:
//! *"`submit_invoice::run` — after `std::fs::read(&args.invoice_xml)`
//! and before any NAV call. If validation fails, no `tokenExchange`
//! happens and no audit entry lands."*
//!
//! The test:
//!
//!   1. Issues an invoice through the normal flow (writes valid XML
//!      to disk; the validator at the issuance call site has already
//!      run by this point).
//!   2. Captures the post-issuance audit-entry count.
//!   3. Hand-corrupts the on-disk XML (strips a required element).
//!   4. Invokes `submit_invoice::run`.
//!   5. Asserts the call returned `Err(_)`.
//!   6. Re-opens the audit ledger; asserts the entry count is
//!      UNCHANGED — no `InvoiceSubmissionAttempt`, no
//!      `InvoiceSubmissionResponse`. The validator caught the
//!      corruption before any NAV call would have produced a wire
//!      attempt.
//!
//! # Why env-gated when no NAV call happens on the failure path
//!
//! The setup path (issuance) still loads NAV credentials from the OS
//! keychain per ADR-0009 §4 — the test cannot run without keychain
//! material populated. The actual NAV submission attempt never
//! happens because the validator fires first.

use std::fs;
use std::path::PathBuf;

use aberp::cli::{IssueInvoiceArgs, NavEnv, SubmitInvoiceArgs};
use aberp::{issue_invoice, submit_invoice};
use aberp_audit_ledger::{BinaryHash, Ledger, TenantId};

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-submit-malformed-{}-{}-{:?}",
        std::process::id(),
        tag,
        std::thread::current().id(),
    ));
    p
}

const TEST_BINARY_HASH: BinaryHash = BinaryHash::from_bytes([0xCD; 32]);

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
    {{ "description": "Test widget", "quantity": 2, "unitPrice": 1000, "vatRatePercent": 27 }}
  ]
}}"#
    )
}

#[test]
fn malformed_xml_loud_fails_before_any_nav_call() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!("ABERP_NAV_LIVE_TEST=1 not set — skipping");
        return;
    }

    let tenant_id_str = std::env::var("ABERP_NAV_TENANT_ID")
        .expect("ABERP_NAV_TENANT_ID must be set when ABERP_NAV_LIVE_TEST=1");
    let tax_number = std::env::var("ABERP_NAV_TEST_TAX_NUMBER")
        .expect("ABERP_NAV_TEST_TAX_NUMBER must be set when ABERP_NAV_LIVE_TEST=1");

    let json_path = temp_path("input.json");
    let xml_path = temp_path("invoice.xml");
    let db_path = temp_path("aberp.duckdb");

    // 1. Issue an invoice through the normal flow.
    fs::write(&json_path, build_input_json(&tax_number)).expect("write input json");
    let issue_args = IssueInvoiceArgs {
        r#in: json_path.clone(),
        out: xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        series: "INV-malformed-test".to_string(),

        currency: aberp::cli::CurrencyArg::Huf,
    };
    issue_invoice::run(&issue_args).expect("issue-invoice must succeed");

    // 2. Capture post-issuance audit-entry count.
    let tenant =
        TenantId::new(tenant_id_str.clone()).expect("tenant id must be valid post-issuance");
    let pre_submit_count = {
        let ledger =
            Ledger::open(&db_path, tenant.clone(), TEST_BINARY_HASH).expect("open audit ledger");
        ledger.entries().expect("read audit ledger").len()
    };
    assert!(
        pre_submit_count >= 2,
        "issue-invoice must have written at least 2 audit entries, got {pre_submit_count}"
    );

    // 3. Hand-corrupt the on-disk XML by stripping <invoiceMain>'s
    //    opening tag — produces structurally malformed XML that the
    //    validator must catch before any NAV call.
    let xml = fs::read(&xml_path).expect("read issued xml");
    let xml_str = String::from_utf8(xml).expect("xml is utf-8");
    let corrupted = xml_str.replace("<invoiceMain>", "<wrongElementName>");
    fs::write(&xml_path, corrupted.as_bytes()).expect("rewrite corrupted xml");

    // 4–5. Submit must Err.
    let invoice_id = read_invoice_id_from_audit(&db_path, tenant.clone());
    let submit_args = SubmitInvoiceArgs {
        invoice_xml: xml_path.clone(),
        invoice_id,
        tax_number: tax_number.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        endpoint: NavEnv::Test,
    };
    let err = submit_invoice::run(&submit_args).expect_err(
        "submit-invoice must loud-fail on hand-corrupted XML — \
         validator fires before any NAV call",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invariant check")
            || msg.contains("InvoiceData")
            || msg.contains("unexpected"),
        "expected validator error message in chain, got: {msg}"
    );

    // 6. Audit-entry count unchanged.
    let post_submit_count = {
        let ledger =
            Ledger::open(&db_path, tenant, TEST_BINARY_HASH).expect("open audit ledger after");
        ledger.entries().expect("read audit ledger after").len()
    };
    assert_eq!(
        pre_submit_count, post_submit_count,
        "the validator must catch the corruption BEFORE any audit entry lands — \
         pre={pre_submit_count}, post={post_submit_count}"
    );
}

/// Pull the issued invoice id back out of the audit ledger. We could
/// also have plumbed it through `issue_invoice::run`'s return value,
/// but the audit ledger is already the source of truth — re-reading
/// it matches the production lookup pattern.
fn read_invoice_id_from_audit(db_path: &std::path::Path, tenant: TenantId) -> String {
    use aberp::audit_payloads::InvoiceDraftCreatedPayload;
    use aberp_audit_ledger::EventKind;
    let ledger =
        Ledger::open(db_path, tenant, TEST_BINARY_HASH).expect("open audit ledger for invoice id");
    let entries = ledger.entries().expect("read audit ledger entries");
    for entry in &entries {
        if entry.kind != EventKind::InvoiceDraftCreated {
            continue;
        }
        let parsed: InvoiceDraftCreatedPayload =
            serde_json::from_slice(&entry.payload).expect("InvoiceDraftCreated payload parses");
        return parsed.invoice_id;
    }
    panic!("no InvoiceDraftCreated entry found in test DB");
}
