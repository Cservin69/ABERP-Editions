//! NAV `manageInvoice` operation per ADR-0009 §4 + §5 (idempotency + retry
//! classification) + §6 (storno / modification chain operation enum).
//!
//! # PR-7-B-3 single-call flow (retained as backward-compat wrapper)
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
//!
//! # PR-19 / ADR-0032 §3 — split into [`build_request`] + [`send_built_request`]
//!
//! The two-tx Attempt-before-call posture per ADR-0032 §1 needs the
//! envelope bytes in hand BEFORE the wire send so TX1 can commit the
//! `InvoiceSubmissionAttempt` audit entry pointing at the exact bytes
//! that will be POSTed. PR-19 splits the [`call`] surface into:
//!
//!   - [`build_request`] — phases 1 above only; returns the
//!     `<ManageInvoiceRequest>` bytes for the caller to pass to TX1.
//!   - [`send_built_request`] — phases 2–6 above; takes the pre-
//!     rendered bytes and returns a [`SendBuiltRequestOutcome`].
//!
//! The existing [`call`] is retained verbatim as a thin wrapper around
//! the two new helpers — no migration required for callers that don't
//! need the split.

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

/// PR-19 / ADR-0032 §3: outcome of a [`send_built_request`] call.
/// Carries everything the caller needs to write the TX2
/// `InvoiceSubmissionResponse` audit entry (the parsed
/// `transaction_id` + the verbatim response bytes). Does NOT carry
/// the request bytes — those live with the caller after the
/// `build_request` step.
#[derive(Debug)]
pub struct SendBuiltRequestOutcome {
    /// NAV-assigned transaction id. Treated as opaque.
    pub transaction_id: String,
    /// Verbatim response bytes for the audit-ledger
    /// `InvoiceSubmissionResponsePayload.response_xml`.
    pub response_xml: Vec<u8>,
}

/// PR-19 / ADR-0032 §3: render the `<ManageInvoiceRequest>` envelope
/// bytes without any wire activity. Used by the two-tx
/// Attempt-before-call posture: the caller writes TX1 (carrying the
/// returned bytes verbatim) BEFORE invoking [`send_built_request`].
///
/// Surfaces every existing envelope-construction error
/// (`ManageInvoiceEmpty`, `ManageInvoiceTooManyItems`,
/// `EnvelopeWriteFailed`). Identical signature shape and inputs to
/// the original [`call`] minus the `transport` parameter (no wire,
/// no transport needed).
pub fn build_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    exchange_token: &str,
    items: &[ManageInvoiceItem<'_>],
) -> Result<Vec<u8>, NavTransportError> {
    let request_id = soap::parts::new_request_id();
    let request_timestamp = soap::parts::request_timestamp(time::OffsetDateTime::now_utc())?;
    soap::render_manage_invoice_request(
        credentials,
        tax_number_8,
        &request_id,
        &request_timestamp,
        exchange_token,
        items,
    )
}

/// PR-19 / ADR-0032 §3: POST a pre-rendered `<ManageInvoiceRequest>`
/// envelope to NAV, capture the response verbatim, parse the result.
/// Used by the two-tx Attempt-before-call posture: the caller invokes
/// this AFTER committing TX1's `InvoiceSubmissionAttempt` audit
/// entry.
///
/// `request_xml` is the bytes returned by a prior [`build_request`]
/// call — pinned by the audit-ledger TX1 commit so the bytes that
/// went on the wire are the bytes the audit record claims.
///
/// Surfaces every existing send-path error (`ManageInvoiceHttp`,
/// `ManageInvoiceHttpStatus`, `ManageInvoiceResponseParse`,
/// `ManageInvoiceNonRetryable`, `ManageInvoiceRetryable`).
pub async fn send_built_request(
    transport: &NavTransport,
    request_xml: &[u8],
) -> Result<SendBuiltRequestOutcome, NavTransportError> {
    let url = format!("{}manageInvoice", transport.endpoint().base_url());

    let response = transport
        .client()
        .post(&url)
        .header("Content-Type", "application/xml")
        .header("Accept", "application/xml")
        .body(request_xml.to_vec())
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

    Ok(SendBuiltRequestOutcome {
        transaction_id,
        response_xml,
    })
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
///
/// PR-19 / ADR-0032 §3: backward-compat wrapper around the new
/// [`build_request`] + [`send_built_request`] split. Callers that
/// need the Attempt-before-call posture use the split helpers
/// directly; everyone else keeps using `call` unchanged.
pub async fn call(
    transport: &NavTransport,
    credentials: &NavCredentials,
    tax_number_8: &str,
    exchange_token: &str,
    items: &[ManageInvoiceItem<'_>],
) -> Result<ManageInvoiceOutcome, NavTransportError> {
    let request_xml = build_request(credentials, tax_number_8, exchange_token, items)?;
    let outcome = send_built_request(transport, &request_xml).await?;
    Ok(ManageInvoiceOutcome {
        transaction_id: outcome.transaction_id,
        request_xml,
        response_xml: outcome.response_xml,
    })
}
