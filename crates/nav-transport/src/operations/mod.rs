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

/// PR-59 / session-79 — one repeated `<technicalValidationMessages>`
/// block out of NAV's `GeneralErrorResponse`. NAV emits one of these
/// per validation rule that fired; a single 400 rejection typically
/// carries 3-10 of them. The four fields mirror NAV's OSA 3.0 schema
/// exactly:
///
///   - `result_code` — `<validationResultCode>`: `"ERROR"` or `"WARN"`.
///   - `error_code` — `<validationErrorCode>`: NAV's machine-readable
///     code (`SCHEMA_VIOLATION`, `SUPPLIER_ADDRESS`, etc.).
///   - `message` — NAV's Hungarian-localized human-readable message.
///   - `tag` — the XPath / element name the rule fired on.
///
/// All four are `Option` because NAV occasionally omits `tag` (for
/// envelope-level rules with no associated element) or `validationErrorCode`
/// (for terse `"WARN"`-class entries); the parser does not silently
/// substitute defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TechnicalValidation {
    pub result_code: Option<String>,
    pub error_code: Option<String>,
    pub message: Option<String>,
    pub tag: Option<String>,
}

/// PR-59 / session-79 — typed shape of a parsed NAV fault body. The
/// top-level `fault_code` / `fault_message` pair comes from the
/// `<result>` block (NAV-OSA) OR `<s:Fault>` block (SOAP fallback);
/// `technical_validations` is the per-rule list NAV emits inside
/// `<technicalValidationMessages>` elements (the actual diagnostic for
/// a 400 — `fault_code=INVALID_REQUEST` is just the generic wrapper).
/// `body_preview` is the operator's fallback evidence when NAV returns
/// a shape this parser does not recognise (HTML maintenance page,
/// non-XML response, etc.).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavFault {
    pub fault_code: Option<String>,
    pub fault_message: Option<String>,
    pub technical_validations: Vec<TechnicalValidation>,
    pub body_preview: String,
}

/// PR-58 / session-78 — best-effort parse of a NAV error body into a
/// [`NavFault`]. Used on the non-2xx-HTTP path where the response is
/// not guaranteed to be the OK-shape NAV envelope `parse_result_block`
/// expects.
///
/// Two top-level shapes are tolerated:
///
///   1. `GeneralErrorResponse` with `<common:errorCode>` + `<common:message>`
///      + repeated `<technicalValidationMessages>` (NAV's typical OSA
///      REST error shape — the technicalValidationMessages array carries
///      the actual per-rule diagnostic).
///   2. SOAP `<s:Fault>` with `<faultcode>` + `<faultstring>` (and
///      possibly a nested `<detail><GeneralExceptionResponse><errorCode>`).
///      No technicalValidationMessages on this path.
///
/// `find_first_text` is namespace-blind (local-name match), so this
/// helper picks up both shapes without an explicit XPath. PR-59 /
/// session-79 extends the prior tuple return with the parsed
/// technical_validations array + an embedded body_preview so the
/// downstream layers can render NAV's structured per-rule errors
/// instead of just the generic `INVALID_REQUEST` wrapper.
pub(crate) fn parse_nav_fault(xml: &[u8]) -> NavFault {
    let mut out = NavFault {
        body_preview: body_preview(xml),
        ..NavFault::default()
    };
    // Try the most-specific NAV-OSA shape first.
    let error_code = find_first_text(xml, "errorCode").ok().flatten();
    let message = find_first_text(xml, "message").ok().flatten();
    if error_code.is_some() || message.is_some() {
        out.fault_code = error_code;
        out.fault_message = message;
    } else {
        // Fall back to SOAP-fault shape — `faultcode` + `faultstring`.
        out.fault_code = find_first_text(xml, "faultcode").ok().flatten();
        out.fault_message = find_first_text(xml, "faultstring").ok().flatten();
    }
    // Per-rule technical validation list. Parse independently of the
    // top-level shape — NAV's `GeneralErrorResponse` carries both the
    // result block AND the array; the SOAP-fault shape carries neither
    // and this returns an empty Vec.
    out.technical_validations = find_all_technical_validations(xml).unwrap_or_default();
    out
}

/// PR-59 / session-79 — walk `xml` and collect every
/// `<technicalValidationMessages>` block into a typed list. NAV's
/// OSA 3.0 schema names this element as a repeating direct child of
/// `<GeneralErrorResponse>`; this parser is namespace-blind (local-name
/// match per [`local_name_matches`]) and tolerates the element appearing
/// at any depth. Returns `Ok(Vec::new())` for bodies that contain no
/// matching element.
///
/// Direct-children-only collection: text inside the outer block is
/// captured only when it is the direct content of a known sub-element
/// (`validationResultCode`, `validationErrorCode`, `message`, `tag`).
/// This prevents the sibling-block `<message>` (inside `<result>`) from
/// polluting the per-validation `message` field.
fn find_all_technical_validations(
    xml: &[u8],
) -> Result<Vec<TechnicalValidation>, NavTransportError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<TechnicalValidation> = Vec::new();

    // block_depth == 0: outside any technicalValidationMessages element.
    // block_depth == 1: inside the outer element (immediately before/
    //                   between/after sub-element children).
    // block_depth >= 2: inside a sub-element (text accumulates into
    //                   `active_sub` of `current`).
    let mut block_depth: u32 = 0;
    let mut current = TechnicalValidation::default();
    let mut active_sub: Option<&'static str> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qualified = e.name();
                let qualified = qualified.as_ref();
                if block_depth == 0 {
                    if local_name_matches(qualified, "technicalValidationMessages") {
                        block_depth = 1;
                        current = TechnicalValidation::default();
                        active_sub = None;
                    }
                } else if block_depth == 1 {
                    block_depth = 2;
                    active_sub = sub_field_for(qualified);
                } else {
                    // Already deeper than a direct child — text inside a
                    // grandchild does NOT accumulate into the per-
                    // validation field. NAV's schema does not name any
                    // grandchildren here, but the depth book-keeping
                    // keeps the parser tolerant of a future shape change.
                    block_depth += 1;
                }
            }
            Ok(Event::End(e)) => {
                let qualified = e.name();
                let qualified = qualified.as_ref();
                if block_depth == 1 && local_name_matches(qualified, "technicalValidationMessages")
                {
                    out.push(std::mem::take(&mut current));
                    block_depth = 0;
                    active_sub = None;
                } else if block_depth >= 2 {
                    block_depth -= 1;
                    if block_depth == 1 {
                        active_sub = None;
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                // Self-closing element like `<ns2:tag/>` — record an
                // empty-string value into the matching field so the
                // operator sees the element WAS present (vs. NAV omitting
                // it entirely, which leaves the field `None`).
                if block_depth == 1 {
                    let qualified = e.name();
                    let qualified = qualified.as_ref();
                    if let Some(field) = sub_field_for(qualified) {
                        assign_sub_field(&mut current, field, String::new());
                    }
                }
            }
            Ok(Event::Text(t)) if block_depth == 2 && active_sub.is_some() => {
                let unescaped = t
                    .unescape()
                    .map_err(|e| {
                        NavTransportError::TokenExchangeResponseParse(format!(
                            "XML text unescape failed in technicalValidationMessages: {e}"
                        ))
                    })?
                    .into_owned();
                let field = active_sub.expect("guarded by match");
                assign_sub_field(&mut current, field, unescaped);
            }
            Ok(Event::Eof) => return Ok(out),
            Err(e) => {
                return Err(NavTransportError::TokenExchangeResponseParse(format!(
                    "XML parse failed at position {} (technicalValidationMessages walk): {e}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
        buf.clear();
    }
}

fn sub_field_for(qualified: &[u8]) -> Option<&'static str> {
    if local_name_matches(qualified, "validationResultCode") {
        Some("result_code")
    } else if local_name_matches(qualified, "validationErrorCode") {
        Some("error_code")
    } else if local_name_matches(qualified, "message") {
        Some("message")
    } else if local_name_matches(qualified, "tag") {
        Some("tag")
    } else {
        None
    }
}

fn assign_sub_field(current: &mut TechnicalValidation, field: &'static str, value: String) {
    let slot = match field {
        "result_code" => &mut current.result_code,
        "error_code" => &mut current.error_code,
        "message" => &mut current.message,
        "tag" => &mut current.tag,
        _ => return,
    };
    match slot {
        Some(s) => s.push_str(&value),
        None => *slot = Some(value),
    }
}

/// PR-58 / session-78 — produce a log-safe preview of a NAV response
/// body. UTF-8-lossy decode + newline collapse so the value lands cleanly
/// on one tracing log line. PR-59 / session-79 — bump the cap from 500
/// to 4000 chars so NAV's repeated `<technicalValidationMessages>` blocks
/// fit in the preview (most NAV fault bodies are 1-2 KB; the 4000-char
/// ceiling covers ~20 technical validations comfortably). The audit-
/// ledger gets the full verbatim bytes separately per ADR-0009 §8.
pub(crate) fn body_preview(xml: &[u8]) -> String {
    let s = String::from_utf8_lossy(xml);
    let collapsed: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if collapsed.chars().count() <= 4000 {
        collapsed
    } else {
        collapsed.chars().take(4000).collect::<String>() + "…"
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

    // ── PR-58 / session-78 + PR-59 / session-79:
    //    parse_nav_fault + body_preview + technicalValidationMessages ──

    /// `GeneralErrorResponse`-shaped body — NAV's typical OSA REST
    /// error envelope. The parser must pull both `errorCode` and
    /// `message` out of the namespaced common prefix (local-name
    /// match, namespace-blind). No technicalValidationMessages on
    /// this fixture — that's pinned separately below.
    #[test]
    fn parse_nav_fault_extracts_general_error_shape() {
        let fault = parse_nav_fault(GENERAL_ERROR_BODY);
        assert_eq!(
            fault.fault_code.as_deref(),
            Some("INVALID_REQUEST_SIGNATURE")
        );
        assert_eq!(
            fault.fault_message.as_deref(),
            Some("The request signature does not match.")
        );
        assert!(fault.technical_validations.is_empty());
        assert!(fault.body_preview.contains("INVALID_REQUEST_SIGNATURE"));
    }

    /// SOAP fault-shaped body — `<s:Envelope><s:Fault><faultcode>` +
    /// `<faultstring>` with a nested `<GeneralExceptionResponse>`
    /// detail. NAV occasionally returns this shape for transport-level
    /// rejections; the parser falls back from the OSA-REST shape to
    /// the SOAP-fault shape via the `find_first_text` local-name match.
    /// The nested `<errorCode>` is picked up by the primary path; we
    /// pin the SOAP-only fallback case here.
    #[test]
    fn parse_nav_fault_falls_back_to_soap_fault_shape() {
        // Hungarian phrase exercised here verbatim — NAV's localized
        // diagnostics are Hungarian; the test guards against a future
        // contributor swapping in an ASCII-only string and losing the
        // UTF-8 round-trip pin.
        let body = "<?xml version=\"1.0\"?>\n\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\n\
  <s:Body>\n\
    <s:Fault>\n\
      <faultcode>s:Client</faultcode>\n\
      <faultstring>A kérés nem értelmezhető (malformed request)</faultstring>\n\
    </s:Fault>\n\
  </s:Body>\n\
</s:Envelope>";
        let fault = parse_nav_fault(body.as_bytes());
        assert_eq!(fault.fault_code.as_deref(), Some("s:Client"));
        assert!(fault.fault_message.as_deref().unwrap().contains("A kérés"));
        assert!(fault.technical_validations.is_empty());
    }

    /// Body the parser cannot extract anything from (HTML error page,
    /// plain text, etc.) returns a fault with all four NAV fields
    /// empty — the caller renders `body_preview` instead.
    #[test]
    fn parse_nav_fault_returns_none_for_unparseable_body() {
        let body = b"<html><body>500 Internal Server Error</body></html>";
        let fault = parse_nav_fault(body);
        assert!(fault.fault_code.is_none());
        assert!(fault.fault_message.is_none());
        assert!(fault.technical_validations.is_empty());
        assert!(fault.body_preview.contains("500 Internal Server Error"));
    }

    /// PR-59 / session-79 — the actual diagnostic for a NAV 400 lives
    /// inside the `<technicalValidationMessages>` array, NOT the
    /// top-level `<errorCode>` wrapper (which is just `INVALID_REQUEST`).
    /// Fixture mirrors NAV's verbatim shape — three repeating blocks
    /// at the same depth as `<result>`, namespaced `ns2:` prefix
    /// (which the parser strips via local-name match). The parser
    /// MUST collect all three with correct field-by-field values; a
    /// regression that returns 0 or 1 entries silently drops NAV's
    /// actual reject reason and is exactly the bug PR-59 closes.
    #[test]
    fn parse_nav_fault_extracts_three_technical_validation_messages() {
        // Hungarian text exercised verbatim — NAV's localized messages
        // are Hungarian and the parser must round-trip them losslessly.
        // Plain `&str` rather than a `b"..."` raw byte string because
        // the byte literal cannot carry non-ASCII characters.
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<GeneralErrorResponse xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\"\n\
                      xmlns:ns2=\"http://schemas.nav.gov.hu/NTCA/1.0/common\">\n\
  <ns2:result>\n\
    <ns2:funcCode>ERROR</ns2:funcCode>\n\
    <ns2:errorCode>INVALID_REQUEST</ns2:errorCode>\n\
    <ns2:message>Helytelen kérés!</ns2:message>\n\
  </ns2:result>\n\
  <technicalValidationMessages>\n\
    <ns2:validationResultCode>ERROR</ns2:validationResultCode>\n\
    <ns2:validationErrorCode>SCHEMA_VIOLATION</ns2:validationErrorCode>\n\
    <ns2:message>Hiányzó kötelező mező: invoiceNumber</ns2:message>\n\
    <ns2:tag>InvoiceData/invoiceNumber</ns2:tag>\n\
  </technicalValidationMessages>\n\
  <technicalValidationMessages>\n\
    <ns2:validationResultCode>ERROR</ns2:validationResultCode>\n\
    <ns2:validationErrorCode>SUPPLIER_ADDRESS</ns2:validationErrorCode>\n\
    <ns2:message>A szállító címe nem érvényes.</ns2:message>\n\
    <ns2:tag>invoiceMain/invoice/invoiceHead/supplierInfo/supplierAddress</ns2:tag>\n\
  </technicalValidationMessages>\n\
  <technicalValidationMessages>\n\
    <ns2:validationResultCode>WARN</ns2:validationResultCode>\n\
    <ns2:validationErrorCode>CUSTOMER_TAX_NUMBER</ns2:validationErrorCode>\n\
    <ns2:message>A vevő adószám ellenőrzése nem sikerült.</ns2:message>\n\
    <ns2:tag>invoiceMain/invoice/invoiceHead/customerInfo/customerTaxNumber</ns2:tag>\n\
  </technicalValidationMessages>\n\
</GeneralErrorResponse>";
        let fault = parse_nav_fault(body.as_bytes());
        // Top-level wrapper still parses out of <result>.
        assert_eq!(fault.fault_code.as_deref(), Some("INVALID_REQUEST"));
        assert_eq!(fault.fault_message.as_deref(), Some("Helytelen kérés!"));
        // All three per-rule blocks parsed verbatim.
        assert_eq!(fault.technical_validations.len(), 3);

        let v0 = &fault.technical_validations[0];
        assert_eq!(v0.result_code.as_deref(), Some("ERROR"));
        assert_eq!(v0.error_code.as_deref(), Some("SCHEMA_VIOLATION"));
        assert_eq!(
            v0.message.as_deref(),
            Some("Hiányzó kötelező mező: invoiceNumber")
        );
        assert_eq!(v0.tag.as_deref(), Some("InvoiceData/invoiceNumber"));

        let v1 = &fault.technical_validations[1];
        assert_eq!(v1.result_code.as_deref(), Some("ERROR"));
        assert_eq!(v1.error_code.as_deref(), Some("SUPPLIER_ADDRESS"));
        assert_eq!(v1.message.as_deref(), Some("A szállító címe nem érvényes."));
        assert_eq!(
            v1.tag.as_deref(),
            Some("invoiceMain/invoice/invoiceHead/supplierInfo/supplierAddress")
        );

        let v2 = &fault.technical_validations[2];
        assert_eq!(v2.result_code.as_deref(), Some("WARN"));
        assert_eq!(v2.error_code.as_deref(), Some("CUSTOMER_TAX_NUMBER"));
        assert_eq!(
            v2.message.as_deref(),
            Some("A vevő adószám ellenőrzése nem sikerült.")
        );
        assert_eq!(
            v2.tag.as_deref(),
            Some("invoiceMain/invoice/invoiceHead/customerInfo/customerTaxNumber")
        );
    }

    /// `body_preview` caps at 4000 chars + collapses newlines to spaces.
    /// PR-59 / session-79 — bumped from 500 to 4000 to fit NAV's
    /// repeated `<technicalValidationMessages>` array in the fallback.
    #[test]
    fn body_preview_caps_long_input_and_collapses_newlines() {
        let body = "x".repeat(5000);
        let p = body_preview(body.as_bytes());
        assert_eq!(p.chars().count(), 4001); // 4000 + the elision "…"
        assert!(p.ends_with('…'));

        // A 3000-char body fits whole; nothing elided.
        let body = "y".repeat(3000);
        let p = body_preview(body.as_bytes());
        assert_eq!(p.chars().count(), 3000);
        assert!(!p.ends_with('…'));

        let multiline = b"line1\nline2\r\nline3";
        let p = body_preview(multiline);
        assert_eq!(p, "line1 line2  line3");
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
