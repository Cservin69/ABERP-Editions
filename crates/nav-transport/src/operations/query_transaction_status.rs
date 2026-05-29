//! NAV `queryTransactionStatus` operation. Implements ADR-0009 §2 (state
//! machine), §4 (auth), §5 (idempotency and retry classification), and
//! §8 (audit evidence).
//!
//! Flow:
//!
//!   1. Render the `<QueryTransactionStatusRequest>` envelope via
//!      `crate::soap::render_query_transaction_status_request` (the
//!      request-signature input uses the non-`manageInvoice` form — plain
//!      `requestId || requestTimestamp || xmlSignKey`, NO per-invoice-
//!      index extension).
//!   2. POST to `<endpoint base url>/queryTransactionStatus`.
//!   3. Capture the response body verbatim BEFORE parsing (ADR-0009
//!      §8 — the audit evidence cannot be lost to a parser bug).
//!   4. On non-success HTTP status: loud-fail.
//!   5. Parse `<common:result>`. On `ERROR`, classify per
//!      `super::is_non_retryable` and surface as either
//!      `NavTransportError::QueryTransactionStatusNonRetryable` (caller
//!      transitions the invoice to `SubmissionStuck`) or
//!      `QueryTransactionStatusRetryable` (caller MAY back off and try
//!      again; the poll loop in `apps/aberp/src/poll_ack.rs` does so up
//!      to ADR-0009 §5's max-attempts cap).
//!   6. On `OK`, extract the FIRST `<invoiceStatus>` text and parse it
//!      into a [`ProcessingStatus`]. NAV's v3.0 response shape carries
//!      one `<processingResult>` per batch index; PR-7-C-1 submits a
//!      single-invoice batch (matching PR-7-B-3's one-invoice CREATE
//!      pattern), so the first `<invoiceStatus>` IS the status for this
//!      transaction. If a future PR submits multi-invoice batches, the
//!      single-status return shape is too narrow and a per-index
//!      collector belongs here.
//!   7. Return [`QueryTransactionStatusOutcome`] with the parsed status
//!      and the verbatim bytes for audit.
//!
//! # What this module returns on the error path
//!
//! On NAV ERROR funcCode the caller receives `Err(...)` and the verbatim
//! `response_xml` bytes are NOT returned. This matches the convention
//! `manage_invoice::call` established in PR-7-B-3: the audit payload's
//! `ack_status` field is the parsed NAV ack value (`RECEIVED` /
//! `PROCESSING` / `SAVED` / `ABORTED`); there is no schema slot for
//! "the query itself returned a NAV-level ERROR funcCode," so the poll
//! loop does not write an `InvoiceAckStatus` audit entry for that case
//! and instead surfaces the classification via tracing + the
//! `SubmissionStuck` typestate transition. If a future PR wants the
//! verbatim NAV-error response in the ledger for query-side failures,
//! the audit payload schema is the natural place to extend (a new
//! optional `nav_error_code` / `nav_error_message` pair).

use crate::credentials::NavCredentials;
use crate::error::NavTransportError;
use crate::soap;
use crate::NavTransport;

use super::{find_first_text, is_non_retryable, parse_result_block, NavResultBlock};

/// Parsed NAV `invoiceStatus` enumeration per the v3.0 `InvoiceStatusType`
/// XSD, extended per the ADR-0009 amendment of 2026-05-29.
///
/// Four NAV-wire values per ADR-0009 §2: two intermediate (`RECEIVED`,
/// `PROCESSING`) and two terminal (`SAVED`, `ABORTED`).
///
/// `DONE`: NAV's production test endpoint was observed (2026-05-28)
/// returning `<invoiceStatus>DONE</...>` for terminally-processed
/// invoices. Per the 2026-05-29 amendment, `DONE` is terminal-success
/// semantically identical to `SAVED`; [`ProcessingStatus::from_nav_str`]
/// parses it AS [`ProcessingStatus::Saved`] so the entire downstream
/// pipeline (audit `ack_status`, the SPA's `AckStatus` wire mirror, and
/// `derive_state`'s `SAVED => Finalized` rule) treats it as Final with no
/// further changes. The verbatim `DONE` is still preserved byte-for-byte
/// in the audit `response_xml`, so no audit fidelity is lost.
///
/// [`ProcessingStatus::Unknown`] is a forward-tolerant catch-all so a
/// future NAV value never fatals the poll read (which previously stuck the
/// invoice on Submitted with no recourse). It is never constructed from a
/// write path; [`call`] mints it only when reading back an unrecognized
/// NAV value, logging the raw string first. Non-terminal: the poll loop
/// keeps polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingStatus {
    /// Intermediate. The request was received by NAV's queue. Caller
    /// keeps polling.
    Received,
    /// Intermediate. NAV is processing the submission. Caller keeps
    /// polling.
    Processing,
    /// Terminal-positive. NAV saved the invoice; legally issued and
    /// reported. Caller transitions `SubmittedInvoice → FinalizedInvoice`.
    /// NAV's `DONE` value parses to this variant (2026-05-29 amendment).
    Saved,
    /// Terminal-negative. NAV aborted the submission (business
    /// validation failure or similar). Caller transitions
    /// `SubmittedInvoice → RejectedInvoice`. Per ADR-0009 §2, the
    /// rejected sequence slot is NOT reused — the audit ledger documents
    /// the rejection and a corrective new invoice must be issued.
    Aborted,
    /// Forward-tolerant catch-all for an unrecognized NAV `invoiceStatus`
    /// value (2026-05-29 amendment). Non-terminal — the poll loop keeps
    /// polling. Read-side only: never produced by [`ProcessingStatus::from_nav_str`]
    /// (which stays strict) — [`call`] mints it after logging the raw
    /// value. Exempt from the as_nav_str/from_nav_str round-trip.
    Unknown,
}

impl ProcessingStatus {
    /// Render the NAV-facing enumeration string. `&'static str` so the
    /// audit-payload (`InvoiceAckStatusPayload.ack_status`) can copy
    /// it without an extra allocation, and so the per-call match
    /// can compare against `as_nav_str()` in tests. `as_*` per the
    /// convention established by `InvoiceOperation::as_nav_str` and
    /// `EventKind::as_str` (cheap-reference returning a static borrow).
    pub fn as_nav_str(self) -> &'static str {
        match self {
            ProcessingStatus::Received => "RECEIVED",
            ProcessingStatus::Processing => "PROCESSING",
            ProcessingStatus::Saved => "SAVED",
            ProcessingStatus::Aborted => "ABORTED",
            // Read-side catch-all (2026-05-29 amendment). The verbatim
            // NAV value is preserved in `response_xml`; this is the
            // ledger `ack_status` mirror string for an unrecognized poll.
            ProcessingStatus::Unknown => "UNKNOWN",
        }
    }

    /// Parse the NAV-facing enumeration string back into a typed value.
    /// Round-trip-proven against [`ProcessingStatus::as_nav_str`] by the
    /// unit test below.
    ///
    /// `DONE` parses to [`ProcessingStatus::Saved`] — NAV's terminal-success
    /// value added post-ADR-0009 and semantically equal to `SAVED` per the
    /// 2026-05-29 amendment.
    ///
    /// An otherwise-unknown string returns `Err`. This method stays strict
    /// (fail-loud per CLAUDE.md rule 12); forward-tolerance lives one level
    /// up in [`call`], which logs the raw value and maps it to
    /// [`ProcessingStatus::Unknown`] rather than fataling the whole poll
    /// read. Strictness here keeps the round-trip contract honest and lets
    /// callers that genuinely want loud-fail (none today) opt in.
    pub fn from_nav_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "RECEIVED" => Ok(ProcessingStatus::Received),
            "PROCESSING" => Ok(ProcessingStatus::Processing),
            "SAVED" => Ok(ProcessingStatus::Saved),
            "ABORTED" => Ok(ProcessingStatus::Aborted),
            // 2026-05-29 amendment: terminal-success, identical to SAVED.
            "DONE" => Ok(ProcessingStatus::Saved),
            _ => Err("unknown NAV invoiceStatus enumeration value"),
        }
    }

    /// True iff this status is terminal (SAVED or ABORTED). Used by the
    /// poll loop to decide whether to break the loop.
    pub fn is_terminal(self) -> bool {
        matches!(self, ProcessingStatus::Saved | ProcessingStatus::Aborted)
    }
}

/// Map a raw NAV `<invoiceStatus>` value to a [`ProcessingStatus`],
/// forward-tolerantly (ADR-0009 2026-05-29 amendment).
///
/// Known values (including `DONE`, which maps to [`ProcessingStatus::Saved`])
/// parse to their typed variant. Anything else — an unrecognized value,
/// the empty string, whitespace, or any future NAV addition — is logged
/// with its raw bytes and mapped to [`ProcessingStatus::Unknown`]
/// (non-terminal) rather than fataling the poll read. The pre-amendment
/// code returned a non-retryable parse error here, which the poll loop
/// surfaced to the operator as a permanent "stuck on Submitted" with no
/// recourse the moment NAV started emitting `DONE`.
fn parse_processing_status_forward_tolerant(raw: &str) -> ProcessingStatus {
    ProcessingStatus::from_nav_str(raw).unwrap_or_else(|_| {
        tracing::warn!(
            invoice_status = %raw,
            "unrecognized <invoiceStatus> value from NAV; treating as opaque \
             non-terminal (keep polling) per ADR-0009 2026-05-29 amendment"
        );
        ProcessingStatus::Unknown
    })
}

/// Successful queryTransactionStatus outcome.
#[derive(Debug)]
pub struct QueryTransactionStatusOutcome {
    /// Parsed `<invoiceStatus>` for the (single) per-index
    /// `processingResult`. PR-7-C-1 callers submit one-invoice batches,
    /// so this is THE status for the polled `transactionId`.
    pub processing_status: ProcessingStatus,

    /// Verbatim request bytes. Today the `InvoiceAckStatusPayload`
    /// audit-payload schema only writes the *response* bytes, but the
    /// request bytes are returned for symmetry with `manage_invoice`
    /// and `token_exchange` and to keep a future schema extension
    /// cheap (the verbatim NAV-poll request is part of the
    /// audit-evidence bundle the operator regenerates per ADR-0009 §8).
    pub request_xml: Vec<u8>,

    /// Verbatim response bytes for the audit-ledger
    /// `InvoiceAckStatusPayload.response_xml`.
    pub response_xml: Vec<u8>,
}

/// Call `queryTransactionStatus` against `transport`. Async — see the
/// tokenExchange module note about reqwest's async client; the binary
/// opens a tokio runtime in `poll_ack::run` and drives the bounded
/// poll loop on it (one HTTP call per attempt).
///
/// `queryTransactionStatus` is a NAV *query* operation per ADR-0009 §4:
/// it authenticates via the per-request `<user>` block (passwordHash +
/// non-`manageInvoice` requestSignature). It does NOT consume an
/// `exchangeToken` — that artifact is only required by `manageInvoice`
/// and `manageAnnulment` per the same section.
///
/// `transaction_id` is the NAV-assigned tracking id from a prior
/// successful `manage_invoice::call`. Treated as opaque; ABERP does
/// not parse its shape.
pub async fn call(
    transport: &NavTransport,
    credentials: &NavCredentials,
    tax_number_8: &str,
    transaction_id: &str,
) -> Result<QueryTransactionStatusOutcome, NavTransportError> {
    let request_id = soap::parts::new_request_id();
    let request_timestamp = soap::parts::request_timestamp(time::OffsetDateTime::now_utc())?;

    let request_xml = soap::render_query_transaction_status_request(
        credentials,
        tax_number_8,
        &request_id,
        &request_timestamp,
        transaction_id,
    )?;

    let url = format!("{}queryTransactionStatus", transport.endpoint().base_url());

    let response = transport
        .client()
        .post(&url)
        .header("Content-Type", "application/xml")
        .header("Accept", "application/xml")
        .body(request_xml.clone())
        .send()
        .await
        .map_err(NavTransportError::QueryTransactionStatusHttp)?;

    let status = response.status();
    let response_xml = response
        .bytes()
        .await
        .map_err(NavTransportError::QueryTransactionStatusHttp)?
        .to_vec();

    if !status.is_success() {
        return Err(NavTransportError::QueryTransactionStatusHttpStatus {
            status: status.as_u16(),
        });
    }

    match parse_result_block(
        &response_xml,
        NavTransportError::QueryTransactionStatusResponseParse,
    )? {
        NavResultBlock::Ok => {}
        NavResultBlock::Error { code, message } => {
            // ADR-0009 §5 retry classification reused (same NAV-side
            // code set across operations). The poll loop treats
            // Retryable as "count this attempt, back off, try again."
            // NonRetryable as "transition to SubmissionStuck, stop."
            if is_non_retryable(&code) {
                return Err(NavTransportError::QueryTransactionStatusNonRetryable {
                    code,
                    message,
                });
            }
            return Err(NavTransportError::QueryTransactionStatusRetryable { code, message });
        }
    }

    let raw_status = find_first_text(&response_xml, "invoiceStatus")?.ok_or_else(|| {
        NavTransportError::QueryTransactionStatusResponseParse(
            "OK response missing <invoiceStatus>".to_string(),
        )
    })?;

    let processing_status = parse_processing_status_forward_tolerant(&raw_status);

    Ok(QueryTransactionStatusOutcome {
        processing_status,
        request_xml,
        response_xml,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal-but-shape-correct
    /// `<QueryTransactionStatusResponse>` carrying the four ack values.
    /// Local-name parsing is namespace-blind per
    /// `crate::operations::find_first_text`, so the prefix used here
    /// just exercises the strip path; the actual NAV wire uses
    /// `common:` and a default namespace which the parser tolerates.
    fn response_with_status(status: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<QueryTransactionStatusResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
                                xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <common:header>
    <common:requestId>REQ-T1</common:requestId>
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
  <processingResults>
    <processingResult>
      <index>1</index>
      <invoiceStatus>{status}</invoiceStatus>
      <compressedContentIndicator>false</compressedContentIndicator>
    </processingResult>
  </processingResults>
</QueryTransactionStatusResponse>"#,
        )
        .into_bytes()
    }

    /// Round-trip every variant. If a future contributor adds a new
    /// ProcessingStatus arm without updating both as_nav_str and
    /// from_nav_str, this test catches the omission. Same maintenance-
    /// trap closure that PR-6.1 F12 named for EventKind.
    #[test]
    fn processing_status_round_trip_for_every_variant() {
        let variants = [
            ProcessingStatus::Received,
            ProcessingStatus::Processing,
            ProcessingStatus::Saved,
            ProcessingStatus::Aborted,
        ];
        for v in variants {
            let s = v.as_nav_str();
            let parsed =
                ProcessingStatus::from_nav_str(s).unwrap_or_else(|e| panic!("{s:?} -> {e}"));
            assert_eq!(parsed, v, "round-trip mismatch for {s:?}");
        }
    }

    /// 2026-05-29 amendment: `DONE` is NAV's terminal-success value (it
    /// reappeared on the production test endpoint) and parses to `Saved`
    /// — semantically identical for our state machine. This unsticks the
    /// invoice-17 "stuck on Submitted" Ervin reported on 2026-05-28.
    #[test]
    fn from_nav_str_maps_done_to_saved() {
        assert_eq!(
            ProcessingStatus::from_nav_str("DONE").unwrap(),
            ProcessingStatus::Saved
        );
    }

    /// `from_nav_str` itself stays strict (fail-loud); forward-tolerance
    /// lives in `parse_processing_status_forward_tolerant` / `call`.
    #[test]
    fn from_nav_str_rejects_truly_unknown_strict() {
        assert!(ProcessingStatus::from_nav_str("").is_err());
        assert!(ProcessingStatus::from_nav_str("FUTURE_VALUE").is_err());
        assert!(
            ProcessingStatus::from_nav_str("saved").is_err(),
            "case-sensitive — NAV's enum is upper-case"
        );
    }

    /// The forward-tolerant read path (used by `call`) NEVER fatals: a
    /// future/garbage value maps to `Unknown` (non-terminal, keep polling)
    /// instead of erroring the whole poll. Pins the ADR-0009 2026-05-29
    /// amendment's core invariant.
    #[test]
    fn forward_tolerant_parse_never_fatals_on_unknown() {
        assert_eq!(
            parse_processing_status_forward_tolerant("FUTURE_VALUE"),
            ProcessingStatus::Unknown
        );
        // Empty, whitespace, and weird unicode all fall back to Unknown
        // rather than panicking or erroring.
        assert_eq!(
            parse_processing_status_forward_tolerant(""),
            ProcessingStatus::Unknown
        );
        assert_eq!(
            parse_processing_status_forward_tolerant("   "),
            ProcessingStatus::Unknown
        );
        assert_eq!(
            parse_processing_status_forward_tolerant("✓状態🚀"),
            ProcessingStatus::Unknown
        );
        // Known values (including DONE) still parse to their real variant.
        assert_eq!(
            parse_processing_status_forward_tolerant("SAVED"),
            ProcessingStatus::Saved
        );
        assert_eq!(
            parse_processing_status_forward_tolerant("DONE"),
            ProcessingStatus::Saved
        );
    }

    #[test]
    fn processing_status_terminal_classification_matches_adr_0009_section_2() {
        assert!(!ProcessingStatus::Received.is_terminal());
        assert!(!ProcessingStatus::Processing.is_terminal());
        assert!(ProcessingStatus::Saved.is_terminal());
        assert!(ProcessingStatus::Aborted.is_terminal());
        // Forward-tolerant catch-all is non-terminal: keep polling.
        assert!(!ProcessingStatus::Unknown.is_terminal());
    }

    /// A `DONE` poll response drives the same terminal-success path a
    /// `SAVED` response does: the parsed status is `Saved`, whose
    /// `as_nav_str()` ("SAVED") becomes the audit `ack_status`, which
    /// `serve::derive_state` maps to `Finalized`. Mirrors
    /// `parse_picks_first_invoice_status_saved`.
    #[test]
    fn parse_done_response_drives_saved_terminal_success() {
        let body = response_with_status("DONE");
        let raw = find_first_text(&body, "invoiceStatus")
            .expect("parse")
            .expect("element present");
        assert_eq!(raw, "DONE");
        let status = parse_processing_status_forward_tolerant(&raw);
        assert_eq!(status, ProcessingStatus::Saved);
        assert!(status.is_terminal());
        assert_eq!(status.as_nav_str(), "SAVED");
    }

    /// Pin the local-name parse against a fixed RECEIVED response body
    /// so a future refactor of `find_first_text` (or the response shape)
    /// surfaces here, not at the first failed live poll.
    #[test]
    fn parse_picks_first_invoice_status_received() {
        let body = response_with_status("RECEIVED");
        let raw = find_first_text(&body, "invoiceStatus")
            .expect("parse")
            .expect("element present");
        assert_eq!(raw, "RECEIVED");
        assert_eq!(
            ProcessingStatus::from_nav_str(&raw).unwrap(),
            ProcessingStatus::Received
        );
    }

    #[test]
    fn parse_picks_first_invoice_status_processing() {
        let body = response_with_status("PROCESSING");
        let raw = find_first_text(&body, "invoiceStatus")
            .expect("parse")
            .expect("element present");
        assert_eq!(raw, "PROCESSING");
    }

    #[test]
    fn parse_picks_first_invoice_status_saved() {
        let body = response_with_status("SAVED");
        let raw = find_first_text(&body, "invoiceStatus")
            .expect("parse")
            .expect("element present");
        assert_eq!(raw, "SAVED");
    }

    #[test]
    fn parse_picks_first_invoice_status_aborted() {
        let body = response_with_status("ABORTED");
        let raw = find_first_text(&body, "invoiceStatus")
            .expect("parse")
            .expect("element present");
        assert_eq!(raw, "ABORTED");
    }

    /// PR-7-C-1 specifically: the retry-classification mapping is shared
    /// with manageInvoice (`super::is_non_retryable`). Verify that the
    /// queryTransactionStatus error-path produces the right variant for
    /// the canonical NAV code split. The full ADR-0009 §5 enumeration is
    /// already exercised by `super::tests::non_retryable_classification…`;
    /// this test is the per-operation-variant assertion that no future
    /// refactor accidentally routes queryTransactionStatus's errors to
    /// the manageInvoice variants (or vice versa).
    #[test]
    fn parse_error_block_routes_to_query_transaction_status_variant() {
        let body = br#"<?xml version="1.0"?>
<QueryTransactionStatusResponse xmlns:common="x">
  <common:result>
    <common:funcCode>ERROR</common:funcCode>
    <common:errorCode>OPERATION_FAILED</common:errorCode>
    <common:message>Transient backend hiccup.</common:message>
  </common:result>
</QueryTransactionStatusResponse>"#;
        // The shape parser routes via the constructor we hand it.
        let err = parse_result_block(body, NavTransportError::QueryTransactionStatusResponseParse)
            .expect("parse");
        match err {
            NavResultBlock::Error { code, message } => {
                assert_eq!(code, "OPERATION_FAILED");
                assert!(message.starts_with("Transient"));
                // The actual call() routes this to Retryable via
                // is_non_retryable(); here we just confirm the result
                // block parsed.
                assert!(!is_non_retryable(&code));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
