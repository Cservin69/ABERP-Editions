//! NAV `manageAnnulment` operation per ADR-0009 §6 + ADR-0026 §3 —
//! the wire half of the technical-annulment surface. Structural
//! parallel to [`super::manage_invoice`] with three deltas:
//!
//!   1. Different endpoint (`/manageAnnulment` not `/manageInvoice`).
//!   2. Different envelope renderer
//!      ([`crate::soap::render_manage_annulment_request`]).
//!   3. Different per-item type ([`crate::soap::ManageAnnulmentItem`]
//!      — no `operation` field; the literal `"ANNUL"` is the only
//!      annulment operation per ADR-0026 §3 + §"Surfaced conflict 2").
//!
//! Flow (identical shape to `manage_invoice::call`):
//!
//!   1. Render the `<ManageAnnulmentRequest>` envelope via
//!      [`crate::soap::render_manage_annulment_request`].
//!   2. POST to `<endpoint base url>/manageAnnulment`.
//!   3. Capture the response body verbatim BEFORE parsing
//!      (ADR-0009 §8 — the audit evidence cannot be lost to a
//!      parser bug).
//!   4. On non-success HTTP status: loud-fail.
//!   5. Parse `<common:result>`. On `ERROR`, classify per
//!      [`super::is_non_retryable`] and surface as either
//!      [`NavTransportError::ManageAnnulmentNonRetryable`] (caller
//!      treats the annulment-wire path as terminal-stuck per
//!      ADR-0026 §5) or [`NavTransportError::ManageAnnulmentRetryable`]
//!      (caller MAY back off and retry; PR-13 does NOT loop — same
//!      posture PR-7-B-3 took for manageInvoice).
//!   6. On `OK`, extract `<transactionId>`. Return outcome with the
//!      transaction id and the verbatim bytes for audit.
//!
//! # Retry classification reuse
//!
//! Per ADR-0026 §5, the retry-classification set is shared across
//! operations via [`super::is_non_retryable`]. No annulment-specific
//! NAV codes are conjectured here; if the testbed surfaces a code
//! not currently in the allowlist (e.g.
//! `UNSUPPORTED_ANNULMENT_CODE` per ADR-0025 §"Surfaced conflict 2"),
//! the amendment is a one-line addition to the shared allowlist.

use crate::credentials::NavCredentials;
use crate::error::NavTransportError;
use crate::soap::{self, ManageAnnulmentItem};
use crate::NavTransport;

use super::{find_first_text, is_non_retryable, parse_result_block, NavResultBlock};

/// Successful manageAnnulment outcome. Mirrors
/// [`super::manage_invoice::ManageInvoiceOutcome`]'s shape so the
/// binary's audit-write code path is symmetric.
///
/// `transaction_id` is NAV's per-annulment tracking id; a future
/// `query-annulment-status` poll keys on it (ADR-0026 §"Follow-on
/// PRs unblocked"). Treated as opaque by ABERP; no shape parsing.
#[derive(Debug)]
pub struct ManageAnnulmentOutcome {
    /// NAV-assigned transaction id. Persisted into the audit-ledger
    /// by the binary
    /// (`InvoiceAnnulmentSubmissionResponsePayload::transaction_id`).
    pub transaction_id: String,

    /// Verbatim request bytes for the audit-ledger
    /// `InvoiceAnnulmentSubmissionAttemptPayload::request_xml`.
    pub request_xml: Vec<u8>,

    /// Verbatim response bytes for the audit-ledger
    /// `InvoiceAnnulmentSubmissionResponsePayload::response_xml`.
    pub response_xml: Vec<u8>,
}

/// Call `manageAnnulment` against `transport`. Async — same posture
/// as `manage_invoice::call`. The binary opens a tokio current-
/// thread runtime in `submit_annulment::run` and drives both
/// `tokenExchange` and this operation on it.
///
/// `exchange_token` is the **decrypted** token from a prior
/// `token_exchange::call` (the caller forwards
/// `outcome.decoded_token`).
///
/// `items` carries the per-index annulment list. PR-13's
/// `submit-annulment` orchestrator passes a single-item slice (one
/// invoice per command invocation, same shape as `submit-invoice`).
pub async fn call(
    transport: &NavTransport,
    credentials: &NavCredentials,
    tax_number_8: &str,
    exchange_token: &str,
    items: &[ManageAnnulmentItem<'_>],
) -> Result<ManageAnnulmentOutcome, NavTransportError> {
    let request_id = soap::parts::new_request_id();
    let request_timestamp = soap::parts::request_timestamp(time::OffsetDateTime::now_utc())?;

    let request_xml = soap::render_manage_annulment_request(
        credentials,
        tax_number_8,
        &request_id,
        &request_timestamp,
        exchange_token,
        items,
    )?;

    let url = format!("{}manageAnnulment", transport.endpoint().base_url());

    let response = transport
        .client()
        .post(&url)
        .header("Content-Type", "application/xml")
        .header("Accept", "application/xml")
        .body(request_xml.clone())
        .send()
        .await
        .map_err(NavTransportError::ManageAnnulmentHttp)?;

    let status = response.status();
    let response_xml = response
        .bytes()
        .await
        .map_err(NavTransportError::ManageAnnulmentHttp)?
        .to_vec();

    if !status.is_success() {
        return Err(NavTransportError::ManageAnnulmentHttpStatus {
            status: status.as_u16(),
        });
    }

    match parse_result_block(
        &response_xml,
        NavTransportError::ManageAnnulmentResponseParse,
    )? {
        NavResultBlock::Ok => {}
        NavResultBlock::Error { code, message } => {
            // ADR-0009 §5 split, shared via `is_non_retryable` per
            // ADR-0026 §5. Non-retryable is terminal for the wire
            // submission (operator escalation); retryable is the
            // operator's call (PR-13 does not auto-retry).
            if is_non_retryable(&code) {
                return Err(NavTransportError::ManageAnnulmentNonRetryable { code, message });
            }
            return Err(NavTransportError::ManageAnnulmentRetryable { code, message });
        }
    }

    let transaction_id = find_first_text(&response_xml, "transactionId")?.ok_or_else(|| {
        NavTransportError::ManageAnnulmentResponseParse(
            "OK response missing <transactionId>".to_string(),
        )
    })?;

    Ok(ManageAnnulmentOutcome {
        transaction_id,
        request_xml,
        response_xml,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0026 §5 retry-classification posture: manageAnnulment
    /// reuses [`super::is_non_retryable`]. Pin the canonical NAV
    /// codes route to the right buckets — defence-in-depth on the
    /// shared classifier behaviour (the full enumeration is
    /// exercised in `super::tests::non_retryable_classification…`).
    #[test]
    fn manage_annulment_inherits_shared_retry_classification() {
        assert!(is_non_retryable("INVALID_SECURITY_USER"));
        assert!(is_non_retryable("INVALID_REQUEST_SIGNATURE"));
        assert!(is_non_retryable("SCHEMA_VIOLATION"));
        // `OPERATION_FAILED` is retryable per ADR-0009 §5.
        assert!(!is_non_retryable("OPERATION_FAILED"));
    }

    /// Parse an `ERROR` result block via the manageAnnulment
    /// constructor — verifies the routing constructor lands in the
    /// `ManageAnnulmentResponseParse` variant on a malformed body
    /// (defence-in-depth on the shared parser).
    #[test]
    fn parse_error_block_routes_to_manage_annulment_variant_on_malformed() {
        let body = br#"<X><common:result/></X>"#;
        let err = parse_result_block(body, NavTransportError::ManageAnnulmentResponseParse)
            .expect_err("missing funcCode must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::ManageAnnulmentResponseParse(_)
        ));
    }

    /// On the OK happy-path, the response carries `<transactionId>`.
    /// Pinning the local-name parse keeps a future
    /// `find_first_text` refactor from silently breaking the
    /// extraction.
    #[test]
    fn find_first_text_extracts_annulment_transaction_id() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<ManageAnnulmentResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                         xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:header>
    <common:requestId>REQ-A1</common:requestId>
    <common:timestamp>20260521T120000Z</common:timestamp>
    <common:requestVersion>3.0</common:requestVersion>
    <common:headerVersion>1.0</common:headerVersion>
  </common:header>
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <transactionId>NAV-ANNUL-TXID-42</transactionId>
</ManageAnnulmentResponse>"#;
        let got = find_first_text(body, "transactionId")
            .expect("parse")
            .expect("element present");
        assert_eq!(got, "NAV-ANNUL-TXID-42");
    }

    /// PR-13 / ADR-0026 §3: an OK response missing `<transactionId>`
    /// loud-fails per CLAUDE.md rule 12 — the audit-payload's
    /// `transaction_id` field is load-bearing for the future
    /// `query-annulment-status` poll, and a silent missing-id would
    /// produce an unrecoverable audit gap. Mirror of
    /// `super::manage_invoice`'s same loud-fail surface.
    #[test]
    fn ok_response_missing_transaction_id_loud_fails() {
        let body = br#"<?xml version="1.0"?>
<ManageAnnulmentResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                         xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:result>
    <common:funcCode>OK</common:funcCode>
  </common:result>
</ManageAnnulmentResponse>"#;
        // Verify the result block parses to OK.
        let block = parse_result_block(body, NavTransportError::ManageAnnulmentResponseParse)
            .expect("parse");
        assert_eq!(block, NavResultBlock::Ok);
        // Verify transactionId is absent.
        let got = find_first_text(body, "transactionId").expect("parse");
        assert!(got.is_none(), "fixture must be missing transactionId");
        // The `call()` path would surface
        // `ManageAnnulmentResponseParse("OK response missing
        // <transactionId>")` here. We assert the precondition
        // (block is OK + transactionId is absent) directly; the
        // full call requires a live HTTP server.
    }
}
