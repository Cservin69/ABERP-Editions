//! End-to-end NAV observe-receiver-confirmation conformance test
//! (PR-15, ADR-0028).
//!
//! ENV-GATED. Body runs only when `ABERP_NAV_LIVE_TEST=1` is set;
//! otherwise the test returns early. Matches the existing PR-7-A /
//! PR-7-B-2 / PR-7-B-3 / PR-8-2 / PR-13 / PR-14 live-test pattern
//! so CI does not need NAV creds and offline contributors do not
//! have a flaky-by-design test.
//!
//! Required environment when ABERP_NAV_LIVE_TEST=1 — same set as
//! `poll_annulment_ack_live.rs`:
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
//!   4. `observe-receiver-confirmation` — this PR's load-bearing
//!      surface. ONE `queryInvoiceData` call against the BASE
//!      invoice's NAV-facing invoice number (NOT a bounded poll
//!      loop per ADR-0028 §4 + §"Surfaced conflict 2").
//!   5. Re-open the audit ledger and verify post-state:
//!      - `verify_chain()` succeeds.
//!      - At least one `InvoiceAnnulmentReceiverConfirmation`
//!        entry exists.
//!      - The entry's `nav_invoice_number` is constructed from
//!        the base's `series.code + sequence_number` per
//!        ADR-0028 §1 / §8.
//!      - The entry's `annulment_transaction_id` matches the
//!        annulment-side wire txid from PR-13's
//!        `InvoiceAnnulmentSubmissionResponse` (NOT the base's
//!        invoice-submission transactionId — load-bearing per
//!        ADR-0028 §2).
//!      - The entry's `invoice_id` matches the base invoice id.
//!      - The entry's `idempotency_key` matches the annulment-
//!        request's idempotency key (F8 lineage per ADR-0028
//!        §7).
//!      - The entry's `response_xml` is non-empty (verbatim NAV
//!        bytes per ADR-0009 §8).

use std::fs;
use std::path::PathBuf;

use aberp::audit_payloads::{
    InvoiceAnnulmentReceiverConfirmationPayload, InvoiceAnnulmentSubmissionResponsePayload,
    InvoiceTechnicalAnnulmentRequestedPayload,
};
use aberp::cli::{
    AnnulmentCode, IssueInvoiceArgs, NavEnv, ObserveReceiverConfirmationArgs,
    RequestTechnicalAnnulmentArgs, SubmitAnnulmentArgs, SubmitInvoiceArgs,
};
use aberp::{
    issue_invoice, observe_receiver_confirmation, request_technical_annulment, submit_annulment,
    submit_invoice,
};
use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-observe-receiver-confirmation-live-{}-{}-{:?}",
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
      "description": "Live receiver-confirmation observation test widget",
      "quantity": 1,
      "unitPrice": 1000,
      "vatRatePercent": 27
    }}
  ]
}}"#
    )
}

#[test]
fn observe_receiver_confirmation_against_api_test_end_to_end() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping observe_receiver_confirmation_against_api_test_end_to_end \
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

    let series_code = "INV-RECVR-CONFIRM-LIVE-TEST";
    let issue_args = IssueInvoiceArgs {
        r#in: json_path.clone(),
        out: invoice_xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
        series: series_code.to_string(),
        currency: aberp::cli::CurrencyArg::Huf,
    };
    issue_invoice::run(&issue_args).expect("issue-invoice must succeed");

    let tenant = TenantId::new(tenant_id_str.clone()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    let ledger = Ledger::open(&db_path, tenant.clone(), binary_hash).expect("open ledger");
    let entries = ledger.entries().expect("read entries");

    // Pick the issued invoice id + its sequence number from the
    // freshly-written draft-created entry. Both fields are
    // load-bearing for the post-state assertion that the
    // nav_invoice_number on the receiver-confirmation entry is
    // constructed correctly per ADR-0028 §1 / §8.
    let (invoice_id, base_seq_for_nav_number) = entries
        .iter()
        .find_map(|e| {
            if e.kind == EventKind::InvoiceSequenceReserved {
                let v: serde_json::Value = serde_json::from_slice(&e.payload).ok()?;
                let id = v.get("invoice_id").and_then(|x| x.as_str())?.to_string();
                let seq = v.get("seq").and_then(|x| x.as_u64())?;
                Some((id, seq))
            } else {
                None
            }
        })
        .expect(
            "InvoiceSequenceReserved entry must exist after issue-invoice \
             (carries both invoice_id and sequence number)",
        );
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
        reason: "live conformance test — observing receiver-confirmation".to_string(),
        out: annulment_xml_path.clone(),
        db: db_path.clone(),
        tenant: tenant_id_str.clone(),
    };
    request_technical_annulment::run(&request_args)
        .expect("request-technical-annulment must succeed");

    // 3. submit-annulment — records the annulment-side
    //    transactionId in the audit ledger; observe-receiver-
    //    confirmation walks back to this entry per ADR-0028 §6.
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

    // Capture the annulment-side wire transactionId + the
    // annulment-request's idempotency key for post-state
    // assertions.
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
    let annulment_request_payload: InvoiceTechnicalAnnulmentRequestedPayload = entries
        .iter()
        .rev()
        .find_map(|e| {
            if e.kind == EventKind::InvoiceTechnicalAnnulmentRequested {
                serde_json::from_slice(&e.payload).ok()
            } else {
                None
            }
        })
        .expect(
            "InvoiceTechnicalAnnulmentRequested entry must exist after \
             request-technical-annulment",
        );
    let annulment_request_idem = annulment_request_payload.idempotency_key.clone();
    drop(ledger);

    // 4. observe-receiver-confirmation — the PR-15 load-bearing
    //    surface. ONE queryInvoiceData call (NOT a bounded
    //    loop).
    let observe_args = ObserveReceiverConfirmationArgs {
        invoice_id: invoice_id.clone(),
        tax_number: supplier_tax,
        db: db_path.clone(),
        tenant: tenant_id_str,
        endpoint: NavEnv::Test,
    };
    observe_receiver_confirmation::run(&observe_args)
        .expect("observe-receiver-confirmation must succeed against api-test");

    // 5. Re-open the ledger; verify post-state.
    let ledger = Ledger::open(&db_path, tenant, binary_hash).expect("re-open ledger");
    let verified = ledger
        .verify_chain()
        .expect("chain still verifies after observe-receiver-confirmation");
    assert!(
        verified >= 8,
        "expected >=8 entries (2 issuance + 2 invoice-submit + 1 annulment-request + 2 annulment-submit + >=1 receiver-confirmation), got {verified}"
    );

    let entries = ledger.entries().expect("read entries");
    let confirmations: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceAnnulmentReceiverConfirmation)
        .collect();
    assert!(
        !confirmations.is_empty(),
        "expected at least one InvoiceAnnulmentReceiverConfirmation entry \
         after observe-receiver-confirmation"
    );

    // ADR-0028 §4 + §"Surfaced conflict 2": one-shot posture
    // means exactly ONE new entry per invocation (NOT a poll
    // loop's worth). This pins that the binary did not
    // accidentally introduce a hidden loop.
    assert_eq!(
        confirmations.len(),
        1,
        "expected exactly one InvoiceAnnulmentReceiverConfirmation entry per invocation \
         (one-shot posture per ADR-0028 §4 + §\"Surfaced conflict 2\"); got {}",
        confirmations.len()
    );

    let expected_nav_number = format!("{}/{:05}", series_code, base_seq_for_nav_number);
    let payload: InvoiceAnnulmentReceiverConfirmationPayload =
        serde_json::from_slice(&confirmations[0].payload)
            .expect("typed receiver-confirmation payload decode");

    // Field-by-field pin per CLAUDE.md rule 9 + ADR-0028 §2.
    assert_eq!(
        payload.invoice_id, invoice_id,
        "receiver-confirmation entry must reference the BASE invoice id"
    );
    assert_eq!(
        payload.nav_invoice_number, expected_nav_number,
        "receiver-confirmation entry's nav_invoice_number must be {{series}}/{{seq:05}} \
         per ADR-0028 §1 / §8"
    );
    assert_eq!(
        payload.annulment_transaction_id, annulment_wire_txid,
        "receiver-confirmation entry must reference the annulment-side wire transactionId \
         (NOT the base invoice's submission txid — ADR-0028 §2)"
    );
    assert_eq!(
        payload.idempotency_key, annulment_request_idem,
        "receiver-confirmation entry must carry the annulment-request's idempotency key \
         (F8 lineage per ADR-0028 §7); a future contributor copy-pasting from poll_ack's \
         None posture would surface here"
    );
    assert!(
        !payload.response_xml.is_empty(),
        "receiver-confirmation entry response_xml must carry verbatim NAV response bytes \
         (ADR-0009 §8 + ADR-0028 §\"Surfaced conflict 3\")"
    );
}
