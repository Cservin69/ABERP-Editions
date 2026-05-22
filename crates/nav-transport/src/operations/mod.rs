//! Typed NAV operations: `tokenExchange` (PR-7-B-2), `manageInvoice`
//! (PR-7-B-3), `queryTransactionStatus` (PR-7-C-1),
//! `manageAnnulment` (PR-13 / ADR-0026 §3), `queryInvoiceData`
//! (PR-15 / ADR-0028 §3), and `queryInvoiceCheck` (PR-20 /
//! ADR-0033 §3).
//!
//! All three operations share the same flow shape:
//!
//!   1. Render the SOAP envelope (`crate::soap`).
//!   2. POST it to the endpoint-specific URL via the pinned-trust
//!      `crate::NavTransport` reqwest client.
//!   3. On non-success HTTP status: loud-fail.
//!   4. Parse the response body (verbatim bytes captured by the caller
//!      for the audit-ledger per ADR-0009 §8 BEFORE parse so a
//!      parser-side bug cannot drop the evidence).
//!   5. If the result block is `ERROR`, map the NAV error code to the
//!      retryable / non-retryable bucket per ADR-0009 §5.
//!
//! Each operation lives in its own file because the response shape and
//! the success-path return type differ; the shared bits (response-body
//! capture, result-block parse) live here in `mod.rs`.
//!
//! # What this module returns to callers
//!
//!   - `token_exchange::call` returns a `TokenExchangeOutcome` whose
//!     `decoded_token` is wrapped in `Zeroizing<String>` (secret) and
//!     whose `request_xml` / `response_xml` are `Vec<u8>` for the
//!     binary's audit-ledger entries.
//!   - `manage_invoice::call` returns a `ManageInvoiceOutcome` whose
//!     `transaction_id` is the NAV-assigned tracking id and whose
//!     `request_xml` / `response_xml` are again `Vec<u8>`.
//!   - `query_transaction_status::call` returns a
//!     `QueryTransactionStatusOutcome` whose `processing_status` is the
//!     parsed `<invoiceStatus>` enum (`RECEIVED` / `PROCESSING` /
//!     `SAVED` / `ABORTED`) and whose `request_xml` / `response_xml`
//!     carry the verbatim bytes for the audit-ledger
//!     `InvoiceAckStatus` entry the poll-loop emits per attempt.
//!   - `manage_annulment::call` returns a `ManageAnnulmentOutcome`
//!     whose `transaction_id` is NAV's annulment-side tracking id
//!     (consumed by `poll_annulment_ack` per ADR-0027) and whose
//!     `request_xml` / `response_xml` carry the verbatim bytes for
//!     the audit-ledger
//!     `InvoiceAnnulmentSubmissionAttempt` /
//!     `InvoiceAnnulmentSubmissionResponse` payloads.
//!   - `query_invoice_data::call` returns a
//!     `QueryInvoiceDataOutcome` whose `request_xml` /
//!     `response_xml` carry the verbatim bytes for the audit-
//!     ledger `InvoiceAnnulmentReceiverConfirmationPayload`. NO
//!     parsed receiver-confirmation field is included today per
//!     ADR-0028 §"Surfaced conflict 3"; the audit-evidence-bundle
//!     reader inspects `response_xml` to determine receiver-
//!     confirmation state. PR-21 / ADR-0034 §3 ADDITIVELY adds
//!     `query_invoice_data::parse_audit_data_transaction_id` —
//!     a standalone parse helper that extracts the
//!     `<auditData>/<transactionId>` element from a verbatim
//!     `<QueryInvoiceDataResponse>` body. Invoked by the binary's
//!     `recover_from_nav` orchestration on the chain-reconstruction
//!     path; the `call` / `QueryInvoiceDataOutcome` surface is
//!     UNCHANGED (the verbatim-bytes-first posture for the
//!     receiver-confirmation field remains intact).
//!   - `query_invoice_check::build_request` /
//!     `query_invoice_check::send_built_request` are the
//!     `build_request` + `send_built_request` split for
//!     `queryInvoiceCheck` per ADR-0033 §3 (no backward-compat
//!     `call` wrapper because this is a brand-new operation
//!     with no pre-existing callers). The `send_built_request`
//!     outcome carries a parsed boolean `check_result` (which
//!     the binary's `retry-submission` state-2 branch maps to
//!     `QueryInvoiceCheckOutcome::Exists` / `Absent` and
//!     records on the new `InvoiceCheckPerformedPayload`)
//!     plus the verbatim response bytes for the audit ledger.
//!
//! None of these operations write to the audit ledger directly — the
//! binary is responsible for that per ADR-0008 §Storage. These
//! operations return verbatim bytes; the caller wraps them in typed
//! audit-payload structs in `apps/aberp/src/audit_payloads.rs`.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::NavTransportError;

pub mod manage_annulment;
pub mod manage_invoice;
pub mod query_invoice_check;
pub mod query_invoice_data;
pub mod query_transaction_status;
pub mod token_exchange;

/// Outcome of `<common:result>` parsing — every NAV response carries
/// this block; the `funcCode` is `OK` on success and `ERROR` on error.
/// Operation-specific success fields (`encodedExchangeToken` for
/// tokenExchange, `transactionId` for manageInvoice) are parsed by the
/// per-operation modules; this enum just carries the shared shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NavResultBlock {
    Ok,
    Error {
        /// `INVALID_SECURITY_USER`, `OPERATION_FAILED`, etc.
        code: String,
        /// NAV's human-readable diagnostic.
        message: String,
    },
}

/// Search a response body for the first text-content of an element with
/// the given local name. Returns `None` if the element does not appear.
///
/// **Local-name match, namespace-blind.** NAV's namespaces are stable
/// and pinned in `crate::soap`; the parser does not attempt to validate
/// the namespace URI on every element. A future stricter pass can layer
/// on top — for PR-7-B-2/3 the local-name match is enough to extract
/// `funcCode`, `errorCode`, `message`, `encodedExchangeToken`,
/// `transactionId`.
///
/// Returns the FIRST occurrence. NAV's response shapes used by PR-7-B
/// have at most one of each target element; if a future operation needs
/// multiple values for the same local name (e.g., a list of validation
/// errors), this helper is too narrow and a per-element collector
/// belongs in the per-operation module.
pub(crate) fn find_first_text(
    xml: &[u8],
    target_local_name: &str,
) -> Result<Option<String>, NavTransportError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut inside_target = false;
    let mut collected = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            // Match-guard form per clippy's `collapsible_match`: the
            // inner `if local_name_matches(...)` collapses into the arm
            // pattern itself, so the branch body is just the state
            // change with no nested `if`.
            Ok(Event::Start(e)) if local_name_matches(e.name().as_ref(), target_local_name) => {
                inside_target = true;
            }
            Ok(Event::End(e))
                if inside_target && local_name_matches(e.name().as_ref(), target_local_name) =>
            {
                return Ok(Some(collected));
            }
            Ok(Event::Text(t)) if inside_target => {
                // unescape() returns Cow<'_, str>; the borrowed form
                // does not outlive `t`, so own it via .into_owned().
                let unescaped = t
                    .unescape()
                    .map_err(|e| {
                        NavTransportError::TokenExchangeResponseParse(format!(
                            "XML text unescape failed: {e}"
                        ))
                    })?
                    .into_owned();
                collected.push_str(&unescaped);
            }
            Ok(Event::Eof) => return Ok(None),
            Err(e) => {
                return Err(NavTransportError::TokenExchangeResponseParse(format!(
                    "XML parse failed at position {}: {e}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
        buf.clear();
    }
}

/// Parse the `<common:result>` block out of a NAV response body. Used
/// by both `token_exchange::call` and `manage_invoice::call` to
/// distinguish success from error before pulling operation-specific
/// fields. Returns `NavTransportError::*ResponseParse` if neither
/// `funcCode` is present (means the body is not a NAV v3.0 response at
/// all — surface loud).
///
/// The `parse_err` constructor lets the caller route the parse-shape
/// error into the operation-specific variant (Token vs ManageInvoice).
pub(crate) fn parse_result_block(
    xml: &[u8],
    parse_err: fn(String) -> NavTransportError,
) -> Result<NavResultBlock, NavTransportError> {
    let func_code = find_first_text(xml, "funcCode")?
        .ok_or_else(|| parse_err("response body missing <funcCode>".to_string()))?;
    match func_code.as_str() {
        "OK" => Ok(NavResultBlock::Ok),
        "ERROR" => {
            let code = find_first_text(xml, "errorCode")?.unwrap_or_else(|| "UNKNOWN".to_string());
            let message =
                find_first_text(xml, "message")?.unwrap_or_else(|| "<no message>".to_string());
            Ok(NavResultBlock::Error { code, message })
        }
        other => Err(parse_err(format!(
            "response body has unexpected funcCode `{other}` (expected OK or ERROR)"
        ))),
    }
}

/// NAV error codes that ADR-0009 §5 names as **non-retryable**. Mapped
/// in one place so both operations agree on the bucket. The caller's
/// state-machine transition (`SubmissionStuck` for non-retryable)
/// depends on this mapping being authoritative.
///
/// Anything not in this list falls into the "retryable" bucket by
/// default. ADR-0009 §5 names HTTP 504, `OPERATION_FAILED`, connection
/// reset, and DNS failure as retryable; the connection/DNS variants are
/// at the transport layer (caught as `*Http(...)`), so the remaining
/// application-layer retryable case here is `OPERATION_FAILED` and
/// anything else NAV invents that we have not yet seen.
pub(crate) fn is_non_retryable(code: &str) -> bool {
    matches!(
        code,
        "INVALID_SECURITY_USER"
            | "INVALID_REQUEST_SIGNATURE"
            | "INCORRECT_REQUEST_SCHEMA"
            | "SCHEMA_VIOLATION"
            | "INVOICE_NUMBER_NOT_UNIQUE"
    )
}

/// Local-name match against a quick-xml `name()` which is the full
/// qualified name (`common:funcCode`). We split on `:` and compare the
/// suffix; if there is no prefix, the whole name is the local name.
fn local_name_matches(qualified: &[u8], target: &str) -> bool {
    let local = match qualified.iter().rposition(|&b| b == b':') {
        Some(i) => &qualified[i + 1..],
        None => qualified,
    };
    local == target.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_OK_BODY: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<TokenExchangeResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                       xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:header>
    <common:requestId>REQ-1</common:requestId>
    <common:timestamp>20260520T120000Z</common:timestamp>
    <common:requestVersion>3.0</common:requestVersion>
    <common:headerVersion>1.0</common:headerVersion>
  </common:header>
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <software>
    <softwareId>ABERP000000000001</softwareId>
  </software>
  <encodedExchangeToken>QUJDREVGR0g=</encodedExchangeToken>
  <tokenValidityFrom>2026-05-20T12:00:00Z</tokenValidityFrom>
  <tokenValidityTo>2026-05-20T12:05:00Z</tokenValidityTo>
</TokenExchangeResponse>"#;

    const GENERAL_ERROR_BODY: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<GeneralErrorResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                      xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:result>
    <common:funcCode>ERROR</common:funcCode>
    <common:errorCode>INVALID_REQUEST_SIGNATURE</common:errorCode>
    <common:message>The request signature does not match.</common:message>
  </common:result>
</GeneralErrorResponse>"#;

    #[test]
    fn find_first_text_extracts_encoded_token() {
        let got = find_first_text(TOKEN_OK_BODY, "encodedExchangeToken").expect("parse");
        assert_eq!(got, Some("QUJDREVGR0g=".to_string()));
    }

    #[test]
    fn find_first_text_handles_common_prefix() {
        // The element is `common:funcCode` in the body but the caller
        // asks for `funcCode` — local-name match must strip the prefix.
        let got = find_first_text(TOKEN_OK_BODY, "funcCode").expect("parse");
        assert_eq!(got, Some("OK".to_string()));
    }

    #[test]
    fn find_first_text_returns_none_for_absent_element() {
        let got = find_first_text(TOKEN_OK_BODY, "thisElementIsNotPresent").expect("parse");
        assert!(got.is_none());
    }

    #[test]
    fn parse_result_block_returns_ok_on_ok_func_code() {
        let got = parse_result_block(TOKEN_OK_BODY, NavTransportError::TokenExchangeResponseParse)
            .expect("parse");
        assert_eq!(got, NavResultBlock::Ok);
    }

    #[test]
    fn parse_result_block_returns_error_with_code_and_message() {
        let got = parse_result_block(
            GENERAL_ERROR_BODY,
            NavTransportError::TokenExchangeResponseParse,
        )
        .expect("parse");
        match got {
            NavResultBlock::Error { code, message } => {
                assert_eq!(code, "INVALID_REQUEST_SIGNATURE");
                assert_eq!(message, "The request signature does not match.");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_block_loud_fails_on_unknown_func_code() {
        let body = br#"<?xml version="1.0"?>
<X xmlns:common="x">
  <common:result>
    <common:funcCode>SURPRISE</common:funcCode>
  </common:result>
</X>"#;
        let err = parse_result_block(body, NavTransportError::TokenExchangeResponseParse)
            .expect_err("must loud-fail");
        match err {
            NavTransportError::TokenExchangeResponseParse(msg) => {
                assert!(msg.contains("SURPRISE"), "diagnostic must name code: {msg}");
            }
            other => panic!("expected TokenExchangeResponseParse, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_block_loud_fails_on_missing_func_code() {
        let body = br#"<X><common:result/></X>"#;
        let err = parse_result_block(body, NavTransportError::ManageInvoiceResponseParse)
            .expect_err("must loud-fail");
        // The constructor lets us route this into the manageInvoice
        // variant — verify the routing actually happened.
        assert!(matches!(
            err,
            NavTransportError::ManageInvoiceResponseParse(_)
        ));
    }

    #[test]
    fn non_retryable_classification_matches_adr_0009_section_5() {
        assert!(is_non_retryable("INVALID_SECURITY_USER"));
        assert!(is_non_retryable("INVALID_REQUEST_SIGNATURE"));
        assert!(is_non_retryable("INCORRECT_REQUEST_SCHEMA"));
        assert!(is_non_retryable("SCHEMA_VIOLATION"));
        assert!(is_non_retryable("INVOICE_NUMBER_NOT_UNIQUE"));
        assert!(!is_non_retryable("OPERATION_FAILED"));
        assert!(!is_non_retryable("UNKNOWN_NEW_CODE_FROM_NAV"));
    }
}
