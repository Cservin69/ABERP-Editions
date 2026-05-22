//! NAV `queryInvoiceCheck` operation per ADR-0033 §3 — the
//! Layer-2 NAV-side existence-check surface ADR-0009 §5
//! named-deferred and ADR-0032 §"Open questions" lifted as
//! F44. Structurally parallel to
//! [`super::query_invoice_data`] (PR-15 / ADR-0028 §3): same
//! non-`manageInvoice` request-signature shape, same
//! `<invoiceNumberQuery>` body wrapper, same one-shot
//! posture (no poll loop). Differs in the return shape:
//! queryInvoiceCheck parses a boolean `<invoiceCheckResult>`
//! from the OK response, where queryInvoiceData returns
//! verbatim bytes only.
//!
//! # PR-20 / ADR-0033 §3 — [`build_request`] + [`send_built_request`] split
//!
//! Follows the ADR-0032 §3 split posture from day one (no
//! backward-compat `call` wrapper — `queryInvoiceCheck` is a
//! brand-new operation with no pre-existing callers). The two
//! helpers are:
//!
//!   - [`build_request`] — renders the
//!     `<QueryInvoiceCheckRequest>` envelope bytes via
//!     [`crate::soap::render_query_invoice_check_request`]. No
//!     wire.
//!   - [`send_built_request`] — POSTs the pre-rendered envelope,
//!     captures the response verbatim, parses both
//!     `<common:result>` and the `<invoiceCheckResult>` boolean,
//!     classifies NAV-side errors per [`super::is_non_retryable`].
//!
//! Callers that want the audit-write of the verbatim request
//! bytes BEFORE the wire send (the
//! `InvoiceCheckPerformed`-as-TX0 posture per ADR-0033 §1) use
//! the two helpers directly; a future operator-facing
//! `aberp check-invoice` command would consume the same helpers.
//!
//! # Flow (mirror of `super::query_invoice_data::call`)
//!
//!   1. Render the `<QueryInvoiceCheckRequest>` envelope via
//!      [`build_request`] (the request-signature input uses
//!      the non-`manageInvoice` form — plain
//!      `requestId || requestTimestamp || xmlSignKey`, NO
//!      per-invoice-index extension).
//!   2. POST to `<endpoint base url>/queryInvoiceCheck`.
//!   3. Capture the response body verbatim BEFORE parsing
//!      (ADR-0009 §8 — the audit evidence cannot be lost to
//!      a parser bug).
//!   4. On non-success HTTP status: loud-fail via
//!      [`NavTransportError::QueryInvoiceCheckHttpStatus`].
//!   5. Parse `<common:result>`. On `ERROR`, classify per
//!      [`super::is_non_retryable`] and surface as either
//!      [`NavTransportError::QueryInvoiceCheckNonRetryable`] or
//!      [`NavTransportError::QueryInvoiceCheckRetryable`]. Per
//!      ADR-0033 §"Surfaced conflict 1 Reading A", the caller
//!      treats BOTH retryable and non-retryable as Phase 0
//!      aborts (the `retry-submission` orchestration re-runs
//!      the operator-driven command later).
//!   6. On `OK`, extract the FIRST `<invoiceCheckResult>` text
//!      and parse it strictly: `"true"` → `Ok(true)`,
//!      `"false"` → `Ok(false)`, anything else → loud-fail
//!      via [`NavTransportError::QueryInvoiceCheckResponseParse`]
//!      per CLAUDE.md rule 12 (silent coercion of unknown
//!      boolean encodings would mask NAV schema drift).
//!
//! # What this module returns on the error path
//!
//! Identical posture to
//! [`super::query_invoice_data::call`]: on NAV ERROR funcCode
//! the caller receives `Err(...)` and the verbatim
//! `response_xml` bytes are NOT returned. The binary's
//! `retry-submission` orchestration captures the typed-error
//! shape via [`crate::submission_queue::classify_attempt_failure`]
//! (which extends to cover the five new
//! `QueryInvoiceCheck*` variants per ADR-0033 §5) and writes
//! the `InvoiceCheckPerformed` audit entry with
//! `outcome = "failure"` + the typed `failure_class` /
//! `failure_code` / `failure_message`.
//!
//! # What this module deliberately does NOT do
//!
//!   - It does NOT consume an `exchangeToken`. `queryInvoiceCheck`
//!     is a NAV *query* operation per ADR-0009 §4 — it
//!     authenticates via the per-request `<user>` block alone,
//!     same as `queryTransactionStatus` and `queryInvoiceData`.
//!   - It does NOT loop. ADR-0033 §1's three-phase posture
//!     calls this operation exactly once per retry; loop
//!     behaviour belongs in the orchestration, not the
//!     operations module.
//!   - It does NOT fetch the chain on `Exists`. The
//!     post-positive-check NAV-side state recovery
//!     (`queryInvoiceData` + local-state reconstruction per
//!     ADR-0009 §5's full intent) is named-deferred as F48.

use crate::credentials::NavCredentials;
use crate::error::NavTransportError;
use crate::soap::{self, InvoiceDirection};
use crate::NavTransport;

use super::{find_first_text, is_non_retryable, parse_result_block, NavResultBlock};

/// PR-20 / ADR-0033 §3: outcome of a [`send_built_request`]
/// call. Carries the parsed boolean existence-check result +
/// the verbatim response bytes the caller persists in the
/// `InvoiceCheckPerformed` audit entry.
#[derive(Debug)]
pub struct SendBuiltRequestOutcome {
    /// `true` iff NAV returned `<invoiceCheckResult>true</>`.
    /// `false` iff NAV returned `<invoiceCheckResult>false</>`.
    /// Anything else surfaces as
    /// [`NavTransportError::QueryInvoiceCheckResponseParse`].
    pub check_result: bool,
    /// Verbatim `<QueryInvoiceCheckResponse>` bytes for the
    /// audit-ledger
    /// `InvoiceCheckPerformedPayload::response_xml`.
    pub response_xml: Vec<u8>,
}

/// PR-20 / ADR-0033 §1: high-level outcome the binary's
/// `retry-submission` orchestration consumes on the OK path.
/// The two variants map directly to ADR-0033 §1's `Exists` /
/// `Absent` outcomes; the orchestration writes the
/// `InvoiceCheckPerformed` audit entry with
/// `outcome = "exists"` or `outcome = "absent"` accordingly.
///
/// `Failure` is NOT a variant of this enum — failures surface
/// via `Result<..., NavTransportError>` per the same convention
/// every other operations module uses. The orchestration maps
/// the typed error into `outcome = "failure"` at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryInvoiceCheckOutcome {
    /// NAV has the invoice (`<invoiceCheckResult>true</>`).
    /// Retry SKIPS the manageInvoice re-POST per ADR-0033 §1.
    Exists,
    /// NAV does not have the invoice
    /// (`<invoiceCheckResult>false</>`). Retry PROCEEDS to the
    /// manageInvoice re-POST per ADR-0033 §1.
    Absent,
}

impl QueryInvoiceCheckOutcome {
    /// String form for the
    /// `InvoiceCheckPerformedPayload.outcome` field. Returned
    /// as `&'static str` so the audit-payload constructor can
    /// copy it without an extra allocation. `as_*` per the
    /// convention established by [`InvoiceOperation::as_nav_str`]
    /// / [`InvoiceDirection::as_nav_str`] /
    /// [`super::query_transaction_status::ProcessingStatus::as_nav_str`]
    /// (cheap-reference returning a static borrow).
    pub fn as_audit_str(self) -> &'static str {
        match self {
            QueryInvoiceCheckOutcome::Exists => "exists",
            QueryInvoiceCheckOutcome::Absent => "absent",
        }
    }

    /// Map the parsed `<invoiceCheckResult>` boolean to the
    /// outcome. Deterministic per CLAUDE.md rule 5.
    pub fn from_check_result(check_result: bool) -> Self {
        if check_result {
            QueryInvoiceCheckOutcome::Exists
        } else {
            QueryInvoiceCheckOutcome::Absent
        }
    }
}

/// PR-20 / ADR-0033 §3: render the
/// `<QueryInvoiceCheckRequest>` envelope bytes without any
/// wire activity. Mirror of the ADR-0032 §3 split posture for
/// `manage_invoice::build_request`; the caller can write a TX0
/// audit entry carrying the returned bytes verbatim BEFORE
/// invoking [`send_built_request`].
///
/// `nav_invoice_number` is the NAV-facing invoice number string
/// (e.g., `"INV-default/00042"`). The caller constructs it from
/// the base invoice's series code + sequence number — same
/// posture as `queryInvoiceData`'s `invoice_number` parameter
/// per ADR-0028 §1.
///
/// `invoice_direction` is the typed enum [`InvoiceDirection`].
/// PR-20's `retry-submission` orchestration always uses
/// [`InvoiceDirection::Outbound`] (ABERP is the supplier);
/// `Inbound` is supported by the renderer but not exercised
/// by PR-20's binary.
pub fn build_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    nav_invoice_number: &str,
    invoice_direction: InvoiceDirection,
) -> Result<Vec<u8>, NavTransportError> {
    let request_id = soap::parts::new_request_id();
    let request_timestamp = soap::parts::request_timestamp(time::OffsetDateTime::now_utc())?;
    // PR-20 single-invoice batches per ADR-0033 §3 — same
    // posture as queryInvoiceData per ADR-0028 §3. A future
    // bulk-check operator command would widen this; not
    // pre-emptively here per CLAUDE.md rule 2.
    let batch_index: u32 = 1;
    soap::render_query_invoice_check_request(
        credentials,
        tax_number_8,
        &request_id,
        &request_timestamp,
        nav_invoice_number,
        invoice_direction,
        batch_index,
    )
}

/// PR-20 / ADR-0033 §3: POST a pre-rendered
/// `<QueryInvoiceCheckRequest>` envelope to NAV, capture the
/// response verbatim, parse the result block + the boolean.
/// Mirror of the ADR-0032 §3 split posture for
/// `manage_invoice::send_built_request`.
///
/// `request_xml` is the bytes returned by a prior
/// [`build_request`] call. Carrying the bytes through the
/// caller (rather than re-rendering them here) is what lets the
/// orchestration audit the exact bytes that went on the wire
/// before the wire fires.
pub async fn send_built_request(
    transport: &NavTransport,
    request_xml: &[u8],
) -> Result<SendBuiltRequestOutcome, NavTransportError> {
    let url = format!("{}queryInvoiceCheck", transport.endpoint().base_url());

    let response = transport
        .client()
        .post(&url)
        .header("Content-Type", "application/xml")
        .header("Accept", "application/xml")
        .body(request_xml.to_vec())
        .send()
        .await
        .map_err(NavTransportError::QueryInvoiceCheckHttp)?;

    let status = response.status();
    let response_xml = response
        .bytes()
        .await
        .map_err(NavTransportError::QueryInvoiceCheckHttp)?
        .to_vec();

    if !status.is_success() {
        return Err(NavTransportError::QueryInvoiceCheckHttpStatus {
            status: status.as_u16(),
        });
    }

    match parse_result_block(&response_xml, NavTransportError::QueryInvoiceCheckResponseParse)? {
        NavResultBlock::Ok => {}
        NavResultBlock::Error { code, message } => {
            // ADR-0009 §5 retry classification reused (same NAV-
            // side code set across operations). Per ADR-0033 §
            // "Surfaced conflict 1 Reading A", the orchestration
            // treats BOTH variants as Phase 0 aborts; the
            // variant fork preserves diagnostics at field-
            // granularity for the operator-visible message + the
            // `InvoiceCheckPerformedPayload.failure_class` field.
            if is_non_retryable(&code) {
                return Err(NavTransportError::QueryInvoiceCheckNonRetryable { code, message });
            }
            return Err(NavTransportError::QueryInvoiceCheckRetryable { code, message });
        }
    }

    let raw_result = find_first_text(&response_xml, "invoiceCheckResult")?.ok_or_else(|| {
        NavTransportError::QueryInvoiceCheckResponseParse(
            "OK response missing <invoiceCheckResult>".to_string(),
        )
    })?;

    // Strict boolean parse per CLAUDE.md rule 12. Silent
    // coercion of unknown encodings (`"1"`, `"yes"`, `"TRUE"`,
    // etc.) into either branch would mask NAV-side schema
    // drift. NAV-testbed verification per ADR-0033 §"Open
    // questions" is the named trigger for amendment if the
    // actual encoding differs from the modelled `"true"` /
    // `"false"` strings.
    let check_result = match raw_result.as_str() {
        "true" => true,
        "false" => false,
        other => {
            return Err(NavTransportError::QueryInvoiceCheckResponseParse(format!(
                "unexpected <invoiceCheckResult> value `{other}` \
                 (expected `true` or `false` per the NAV v3.0 boolean encoding; \
                 NAV-testbed verification is the named trigger for amendment per ADR-0033)"
            )));
        }
    };

    Ok(SendBuiltRequestOutcome {
        check_result,
        response_xml,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal-but-shape-correct
    /// `<QueryInvoiceCheckResponse>` carrying the requested
    /// boolean. Local-name parsing is namespace-blind per
    /// `crate::operations::find_first_text`, so the prefix used
    /// here just exercises the strip path; the actual NAV wire
    /// uses `common:` and a default namespace which the parser
    /// tolerates.
    fn response_with_check_result(value: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceCheckResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                           xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:header>
    <common:requestId>REQ-Q1</common:requestId>
    <common:timestamp>20260522T120000Z</common:timestamp>
    <common:requestVersion>3.0</common:requestVersion>
    <common:headerVersion>1.0</common:headerVersion>
  </common:header>
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <software>
    <softwareId>ABERP000000000001</softwareId>
  </software>
  <invoiceCheckResult>{value}</invoiceCheckResult>
</QueryInvoiceCheckResponse>"#,
        )
        .into_bytes()
    }

    /// PR-20 / ADR-0033 §3: the OK happy-path with
    /// `<invoiceCheckResult>true</>` parses to
    /// `check_result = true`. Pin against a regression that
    /// drops the strict-boolean parse and silently coerces
    /// non-`true`/`false` strings.
    #[test]
    fn parse_picks_invoice_check_result_true() {
        let body = response_with_check_result("true");
        let raw = find_first_text(&body, "invoiceCheckResult")
            .expect("parse")
            .expect("element present");
        assert_eq!(raw, "true");
    }

    /// PR-20 / ADR-0033 §3: same pin for the `false` branch.
    /// Both variants must parse cleanly via the local-name
    /// matcher.
    #[test]
    fn parse_picks_invoice_check_result_false() {
        let body = response_with_check_result("false");
        let raw = find_first_text(&body, "invoiceCheckResult")
            .expect("parse")
            .expect("element present");
        assert_eq!(raw, "false");
    }

    /// PR-20 / ADR-0033 §3: outcome enum maps boolean →
    /// Exists/Absent without ambiguity. Pins the
    /// `from_check_result` contract.
    #[test]
    fn outcome_from_check_result_maps_boolean() {
        assert_eq!(
            QueryInvoiceCheckOutcome::from_check_result(true),
            QueryInvoiceCheckOutcome::Exists
        );
        assert_eq!(
            QueryInvoiceCheckOutcome::from_check_result(false),
            QueryInvoiceCheckOutcome::Absent
        );
    }

    /// PR-20 / ADR-0033 §2: the audit-payload string form is
    /// the discriminator the `InvoiceCheckPerformedPayload.outcome`
    /// field carries. Pin both arms against a regression that
    /// reshapes the strings (the audit ledger's stored values
    /// are immutable per ADR-0008 — a rename would diverge new
    /// entries from existing ones).
    #[test]
    fn outcome_as_audit_str_matches_adr_0033_section_2_enumeration() {
        assert_eq!(QueryInvoiceCheckOutcome::Exists.as_audit_str(), "exists");
        assert_eq!(QueryInvoiceCheckOutcome::Absent.as_audit_str(), "absent");
    }

    /// PR-20 / ADR-0033 §3: ADR-0009 §5 retry classification
    /// reused. Pin the canonical NAV codes route to the right
    /// buckets — defence-in-depth on the shared classifier
    /// behaviour (the full enumeration is exercised in
    /// `super::tests::non_retryable_classification_matches_adr_0009_section_5`).
    #[test]
    fn query_invoice_check_inherits_shared_retry_classification() {
        assert!(is_non_retryable("INVALID_SECURITY_USER"));
        assert!(is_non_retryable("INVALID_REQUEST_SIGNATURE"));
        assert!(is_non_retryable("SCHEMA_VIOLATION"));
        // `OPERATION_FAILED` is retryable per ADR-0009 §5; the
        // queryInvoiceCheck variant fork preserves the
        // distinction for inspector triage even though
        // `retry-submission`'s Phase 0 aborts on both per
        // ADR-0033 §"Surfaced conflict 1 Reading A".
        assert!(!is_non_retryable("OPERATION_FAILED"));
    }

    /// PR-20 / ADR-0033 §3: parse an `ERROR` result block via
    /// the queryInvoiceCheck constructor — verifies the routing
    /// constructor lands in the `QueryInvoiceCheckResponseParse`
    /// variant on a malformed body (defence-in-depth on the
    /// shared parser; mirror of
    /// `super::query_invoice_data::tests::parse_error_block_routes_…`).
    #[test]
    fn parse_error_block_routes_to_query_invoice_check_variant_on_malformed() {
        let body = br#"<X><common:result/></X>"#;
        let err = parse_result_block(body, NavTransportError::QueryInvoiceCheckResponseParse)
            .expect_err("missing funcCode must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::QueryInvoiceCheckResponseParse(_)
        ));
    }
}
