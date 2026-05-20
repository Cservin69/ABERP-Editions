//! NAV `manageInvoice` operation per ADR-0009 §4 + §5 (idempotency + retry
//! classification) + §6 (storno / modification chain operation enum).
//!
//! Flow:
//!
//!   1. Render the `<ManageInvoiceRequest>` envelope via
//!      `crate::soap::render_manage_invoice_request` (signed inputs use
//!      a fresh requestId + timestamp; the per-invoice signature suffix
//!      is built from the same `items` slice that lands on the wire).
//!   2. POST to `<endpoint base url>/manageInvoice`.
//!   3. Capture the response body verbatim BEFORE parsing (ADR-0009
//!      §8 — the audit evidence cannot be lost to a parser bug).
//!   4. On non-success HTTP status: loud-fail.
//!   5. Parse `<common:result>`. On `ERROR`, classify per
//!      `super::is_non_retryable` and surface as either
//!      `NavTransportError::ManageInvoiceNonRetryable` (caller
//!      transitions the invoice to `SubmissionStuck`) or
//!      `ManageInvoiceRetryable` (caller MAY back off and retry per
//!      ADR-0009 §5; PR-7-B-3 does NOT retry — the retry loop lands in
//!      PR-7-C alongside the ack poll).
//!   6. On `OK`, extract `<transactionId>`. Return outcome with the
//!      transaction id and the verbatim bytes for audit.

use crate::credentials::NavCredentials;
use crate::error::NavTransportError;
use crate::soap::{self, ManageInvoiceItem};
use crate::NavTransport;

use super::{find_first_text, is_non_retryable, parse_result_block, NavResultBlock};

/// Successful manageInvoice outcome. The `transaction_id` is NAV's
/// per-submission tracking id; ABERP polls `queryTransactionStatus`
/// against it to learn the terminal `SAVED` / `ABORTED` outcome (PR-7-C
/// scope).
#[derive(Debug)]
pub struct ManageInvoiceOutcome {
    /// NAV-assigned transaction id. Persisted into the audit-ledger by
    /// the binary (`InvoiceSubmissionResponsePayload.transaction_id`).
    /// Treated as opaque; ABERP does not parse its shape.
    pub transaction_id: String,

    /// Verbatim request bytes for the audit-ledger
    /// `InvoiceSubmissionAttemptPayload.request_xml`.
    pub request_xml: Vec<u8>,

    /// Verbatim response bytes for the audit-ledger
    /// `InvoiceSubmissionResponsePayload.response_xml`.
    pub response_xml: Vec<u8>,
}

/// Call `manageInvoice` against `transport`. Async — see the
/// tokenExchange module note about reqwest's async client; the binary
/// opens a tokio runtime in `submit_invoice::run` and drives both
/// operations on it.
///
/// `exchange_token` is the **decrypted** token from a prior
/// `token_exchange::call` (the caller forwards `outcome.decoded_token`).
///
/// `items` carries the per-index invoice list (PR-7-B-3 happy path uses
/// exactly one CREATE entry; storno / modify lands later).
pub async fn call(
    transport: &NavTransport,
    credentials: &NavCredentials,
    tax_number_8: &str,
    exchange_token: &str,
    items: &[ManageInvoiceItem<'_>],
) -> Result<ManageInvoiceOutcome, NavTransportError> {
    let request_id = soap::parts::new_request_id();
    let request_timestamp = soap::parts::request_timestamp(time::OffsetDateTime::now_utc())?;

    let request_xml = soap::render_manage_invoice_request(
        credentials,
        tax_number_8,
        &request_id,
        &request_timestamp,
        exchange_token,
        items,
    )?;

    let url = format!("{}manageInvoice", transport.endpoint().base_url());

    let response = transport
        .client()
        .post(&url)
        .header("Content-Type", "application/xml")
        .header("Accept", "application/xml")
        .body(request_xml.clone())
        .send()
        .await
        .map_err(NavTransportError::ManageInvoiceHttp)?;

    let status = response.status();
    let response_xml = response
        .bytes()
        .await
        .map_err(NavTransportError::ManageInvoiceHttp)?
        .to_vec();

    if !status.is_success() {
        return Err(NavTransportError::ManageInvoiceHttpStatus {
            status: status.as_u16(),
        });
    }

    match parse_result_block(&response_xml, NavTransportError::ManageInvoiceResponseParse)? {
        NavResultBlock::Ok => {}
        NavResultBlock::Error { code, message } => {
            // ADR-0009 §5 split: non-retryable → SubmissionStuck;
            // retryable → caller MAY retry (PR-7-B-3 does not loop).
            if is_non_retryable(&code) {
                return Err(NavTransportError::ManageInvoiceNonRetryable { code, message });
            }
            return Err(NavTransportError::ManageInvoiceRetryable { code, message });
        }
    }

    let transaction_id = find_first_text(&response_xml, "transactionId")?.ok_or_else(|| {
        NavTransportError::ManageInvoiceResponseParse(
            "OK response missing <transactionId>".to_string(),
        )
    })?;

    Ok(ManageInvoiceOutcome {
        transaction_id,
        request_xml,
        response_xml,
    })
}
