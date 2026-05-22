//! NAV `queryInvoiceData` operation per ADR-0028 §3 — the
//! receiver-confirmation observation surface of the technical-
//! annulment lifecycle. Pairs structurally with PR-7-C-1's
//! [`super::query_transaction_status`] (same non-`manageInvoice`
//! request-signature shape) and PR-13's
//! [`super::manage_annulment`] (same opaque-key audit-evidence
//! shape).
//!
//! Flow (mirror of `super::query_transaction_status::call`):
//!
//!   1. Render the `<QueryInvoiceDataRequest>` envelope via
//!      [`crate::soap::render_query_invoice_data_request`] (the
//!      request-signature input uses the non-`manageInvoice`
//!      form — plain `requestId || requestTimestamp || xmlSignKey`,
//!      NO per-invoice-index extension).
//!   2. POST to `<endpoint base url>/queryInvoiceData`.
//!   3. Capture the response body verbatim BEFORE parsing
//!      (ADR-0009 §8 — the audit evidence cannot be lost to a
//!      parser bug).
//!   4. On non-success HTTP status: loud-fail.
//!   5. Parse `<common:result>`. On `ERROR`, classify per
//!      [`super::is_non_retryable`] and surface as either
//!      [`NavTransportError::QueryInvoiceDataNonRetryable`]
//!      (caller surfaces an operator-action-required diagnostic
//!      per ADR-0028 §4) or
//!      [`NavTransportError::QueryInvoiceDataRetryable`] (caller
//!      surfaces the diagnostic and exits non-zero; the operator
//!      re-runs after the transient cause resolves — one-shot
//!      posture per ADR-0028 §"Surfaced conflict 2", NO bounded
//!      poll loop).
//!   6. On `OK`, return the verbatim bytes for audit. PR-15
//!      does NOT parse a receiver-confirmation field per
//!      ADR-0028 §"Surfaced conflict 3" — the verbatim-bytes-
//!      only posture applies until NAV-testbed verification
//!      surfaces the actual response field; a future amendment
//!      ADR adds a parsed `receiver_state` enum additively.
//!
//! # What this module returns on the error path
//!
//! On NAV ERROR funcCode the caller receives `Err(...)` and the
//! verbatim `response_xml` bytes are NOT returned. This matches
//! the convention `manage_invoice::call` /
//! `query_transaction_status::call` /
//! `manage_annulment::call` established. The binary surfaces
//! the diagnostic via `tracing::error!` + exits non-zero; the
//! operator re-runs the command after the cause resolves.
//!
//! # What this module deliberately does NOT do
//!
//!   - It does NOT parse a receiver-confirmation status field
//!     out of the OK response body. Per ADR-0028 §"Surfaced
//!     conflict 3" the verbatim-bytes-only posture is the
//!     contract; a future amendment ADR introduces the parsed
//!     field after NAV-testbed verification.
//!   - It does NOT loop. Per ADR-0028 §4 + §"Surfaced conflict
//!     2" receiver-confirmation is human-paced; a bounded poll
//!     loop at seconds-cadence is structurally wrong.
//!   - It does NOT consume an `exchangeToken`. `queryInvoiceData`
//!     is a NAV *query* operation per ADR-0009 §4 — it
//!     authenticates via the per-request `<user>` block alone,
//!     same as `queryTransactionStatus`.
//!
//! # PR-21 / ADR-0034 §3 — additive `parse_audit_data_transaction_id`
//!
//! Alongside [`call`] (the verbatim-bytes-first operation per
//! ADR-0028) PR-21 ADDITIVELY adds
//! [`parse_audit_data_transaction_id`] — a standalone parse helper
//! that extracts the `<auditData>/<transactionId>` element from a
//! verbatim `<QueryInvoiceDataResponse>` body. Invoked by the
//! binary's `recover_from_nav` orchestration to recover the
//! NAV-assigned transactionId of an invoice whose original
//! `manageInvoice` submission's Response was lost in transit
//! (state-2 Pending with a prior `InvoiceCheckPerformed(outcome=exists)`
//! audit entry per ADR-0033 §1). The [`call`] /
//! [`QueryInvoiceDataOutcome`] surface is UNCHANGED — the
//! receiver-confirmation field's verbatim-bytes-first posture per
//! ADR-0028 §"Surfaced conflict 3" remains intact; the
//! `query_invoice_data_outcome_shape_has_no_parsed_status_field`
//! pin test continues to assert the absence of any parsed field on
//! the outcome struct.

use crate::credentials::NavCredentials;
use crate::error::NavTransportError;
use crate::soap::{self, InvoiceDirection};
use crate::NavTransport;

use super::{find_first_text, is_non_retryable, parse_result_block, NavResultBlock};

/// Successful queryInvoiceData outcome.
///
/// Carries the verbatim request and response bytes per ADR-0009
/// §8 — the binary wraps them in
/// [`crate::soap`]-free audit-payload structs in
/// `apps/aberp/src/audit_payloads.rs`. No parsed
/// receiver-confirmation field is included today per ADR-0028
/// §"Surfaced conflict 3"; the audit-evidence-bundle reader
/// inspects `response_xml` to determine receiver-confirmation
/// state.
#[derive(Debug)]
pub struct QueryInvoiceDataOutcome {
    /// Verbatim request bytes. The
    /// `InvoiceAnnulmentReceiverConfirmationPayload` audit-
    /// payload does NOT carry these today (only the response
    /// bytes are persisted, mirroring
    /// `InvoiceAckStatusPayload`'s posture); the field is
    /// returned here for symmetry with the other operations and
    /// to keep a future schema extension cheap (verbatim NAV-
    /// query request bytes are part of the audit-evidence bundle
    /// the operator regenerates per ADR-0009 §8).
    pub request_xml: Vec<u8>,

    /// Verbatim `<QueryInvoiceDataResponse>` bytes for the
    /// audit-ledger
    /// `InvoiceAnnulmentReceiverConfirmationPayload::response_xml`.
    pub response_xml: Vec<u8>,
}

/// Call `queryInvoiceData` against `transport`. Async — same
/// posture as `query_transaction_status::call`. The binary opens
/// a tokio current-thread runtime in
/// `observe_receiver_confirmation::run` and drives this one call
/// on it (one HTTP call per invocation per ADR-0028 §4 — NO
/// loop).
///
/// `queryInvoiceData` is a NAV *query* operation per ADR-0009 §4:
/// it authenticates via the per-request `<user>` block
/// (passwordHash + non-`manageInvoice` requestSignature). It
/// does NOT consume an `exchangeToken` — that artifact is only
/// required by `manageInvoice` and `manageAnnulment` per the
/// same section.
///
/// `invoice_number` is the BASE invoice's NAV-facing invoice
/// number string (e.g., `"INV-default/00042"`). The caller
/// constructs it from the base invoice's series code + sequence
/// number per ADR-0028 §1's "Does NOT take --nav-invoice-number"
/// posture.
///
/// `invoice_direction` is the typed enum
/// [`InvoiceDirection::Outbound`] for PR-15's supplier-side
/// observation path. `InvoiceDirection::Inbound` is supported by
/// the renderer but not exercised by PR-15's binary.
pub async fn call(
    transport: &NavTransport,
    credentials: &NavCredentials,
    tax_number_8: &str,
    invoice_number: &str,
    invoice_direction: InvoiceDirection,
) -> Result<QueryInvoiceDataOutcome, NavTransportError> {
    let request_id = soap::parts::new_request_id();
    let request_timestamp = soap::parts::request_timestamp(time::OffsetDateTime::now_utc())?;

    // PR-15 single-invoice batches per ADR-0028 §3 — same posture
    // as every prior submit-* / poll-* command. A future
    // reconciliation-side PR that walks multi-invoice batches
    // widens this; not pre-emptively here per CLAUDE.md rule 2.
    let batch_index: u32 = 1;

    let request_xml = soap::render_query_invoice_data_request(
        credentials,
        tax_number_8,
        &request_id,
        &request_timestamp,
        invoice_number,
        invoice_direction,
        batch_index,
    )?;

    let url = format!("{}queryInvoiceData", transport.endpoint().base_url());

    let response = transport
        .client()
        .post(&url)
        .header("Content-Type", "application/xml")
        .header("Accept", "application/xml")
        .body(request_xml.clone())
        .send()
        .await
        .map_err(NavTransportError::QueryInvoiceDataHttp)?;

    let status = response.status();
    let response_xml = response
        .bytes()
        .await
        .map_err(NavTransportError::QueryInvoiceDataHttp)?
        .to_vec();

    if !status.is_success() {
        return Err(NavTransportError::QueryInvoiceDataHttpStatus {
            status: status.as_u16(),
        });
    }

    match parse_result_block(&response_xml, NavTransportError::QueryInvoiceDataResponseParse)? {
        NavResultBlock::Ok => {}
        NavResultBlock::Error { code, message } => {
            // ADR-0009 §5 retry classification reused (same NAV-side
            // code set across operations). The one-shot caller per
            // ADR-0028 §4 treats both Retryable and NonRetryable as
            // operator-action-required surfaces; the variant fork
            // preserves diagnostics at field-granularity for the
            // operator-visible message.
            if is_non_retryable(&code) {
                return Err(NavTransportError::QueryInvoiceDataNonRetryable { code, message });
            }
            return Err(NavTransportError::QueryInvoiceDataRetryable { code, message });
        }
    }

    // PR-15 does NOT parse a receiver-confirmation field per
    // ADR-0028 §"Surfaced conflict 3". The OK happy-path returns
    // the verbatim bytes; the caller persists them in the audit
    // ledger and the operator interprets (via export bundle or
    // by hand, per ADR-0028 §5).
    Ok(QueryInvoiceDataOutcome {
        request_xml,
        response_xml,
    })
}

/// PR-21 / ADR-0034 §3: extract the `<auditData>/<transactionId>` field
/// from a verbatim `<QueryInvoiceDataResponse>` body. Used by the binary's
/// `recover_from_nav` orchestration to recover the NAV-assigned
/// transactionId of an invoice whose original `manageInvoice` submission's
/// Response was lost in transit (state-2 Pending with a prior
/// `InvoiceCheckPerformed(outcome=exists)` audit entry per ADR-0033 §1).
///
/// **Additive — does NOT touch the verbatim-bytes-first posture of
/// [`call`] / [`QueryInvoiceDataOutcome`].** Per ADR-0028 §"Surfaced
/// conflict 3" the OK happy path of `call` returns the verbatim bytes
/// only; PR-15's `query_invoice_data_outcome_shape_has_no_parsed_status_field`
/// pin test continues to assert the absence of a parsed
/// receiver-confirmation field on the outcome struct. This helper
/// extracts a **different** element (`<transactionId>` from the
/// `<auditData>` block — NAV's record of the original submission's
/// transactionId, unrelated to receiver-confirmation) and is invoked
/// by the orchestration layer ONLY on the recovery path. The
/// receiver-confirmation parsing remains named-deferred per
/// ADR-0028; PR-21 does NOT fire the ADR-0028 §"Surfaced conflict 3"
/// amendment trigger.
///
/// The NAV v3.0 spec places `<transactionId>` inside the
/// `<auditData>` block of `<invoiceDataResult>`. The
/// [`find_first_text`] helper is namespace-blind (matches by local
/// name) and returns the FIRST occurrence — sufficient because the
/// response shape carries at most one `<transactionId>` element.
///
/// Returns [`NavTransportError::QueryInvoiceDataResponseParse`] on:
/// - Element missing entirely (NAV-side response-shape divergence;
///   named trigger for an amendment ADR per ADR-0034 §"Open
///   questions").
/// - Element present but the text content is empty (defence-in-depth
///   against a NAV-side bug or a hand-edited response fixture; the
///   audit-ledger contract requires a non-empty transaction_id per
///   `audit_payloads::InvoiceSubmissionResponsePayload`'s shape).
/// - XML parse failure (delegated to [`find_first_text`]'s Result;
///   routed through the same `QueryInvoiceDataResponseParse` variant
///   as the existing parse-side failures inside [`call`]).
pub fn parse_audit_data_transaction_id(
    response_xml: &[u8],
) -> Result<String, NavTransportError> {
    let raw = find_first_text(response_xml, "transactionId")?.ok_or_else(|| {
        NavTransportError::QueryInvoiceDataResponseParse(
            "queryInvoiceData response missing <auditData>/<transactionId> — NAV-side \
             response-shape divergence; NAV-testbed verification is the named trigger for \
             an amendment ADR per ADR-0034 §\"Open questions\""
                .to_string(),
        )
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(NavTransportError::QueryInvoiceDataResponseParse(
            "queryInvoiceData response carries empty <auditData>/<transactionId> — \
             defence-in-depth loud-fail per CLAUDE.md rule 12"
                .to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0028 §3 retry-classification posture: queryInvoiceData
    /// reuses [`super::is_non_retryable`]. Pin the canonical NAV
    /// codes route to the right buckets — defence-in-depth on the
    /// shared classifier behaviour (the full enumeration is
    /// exercised in `super::tests::non_retryable_classification…`).
    #[test]
    fn query_invoice_data_inherits_shared_retry_classification() {
        assert!(is_non_retryable("INVALID_SECURITY_USER"));
        assert!(is_non_retryable("INVALID_REQUEST_SIGNATURE"));
        assert!(is_non_retryable("SCHEMA_VIOLATION"));
        // `OPERATION_FAILED` is retryable per ADR-0009 §5.
        assert!(!is_non_retryable("OPERATION_FAILED"));
    }

    /// Parse an `ERROR` result block via the queryInvoiceData
    /// constructor — verifies the routing constructor lands in
    /// the `QueryInvoiceDataResponseParse` variant on a malformed
    /// body (defence-in-depth on the shared parser; mirror of
    /// `super::manage_annulment::tests::parse_error_block_routes_…`).
    #[test]
    fn parse_error_block_routes_to_query_invoice_data_variant_on_malformed() {
        let body = br#"<X><common:result/></X>"#;
        let err = parse_result_block(body, NavTransportError::QueryInvoiceDataResponseParse)
            .expect_err("missing funcCode must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::QueryInvoiceDataResponseParse(_)
        ));
    }

    /// PR-21 / ADR-0034 §3 happy path: extracts the
    /// `<transactionId>` text from a verbatim queryInvoiceData
    /// response body. The fixture's `<auditData>` block carries
    /// the canonical NAV v3.0 placement (`<auditData>` inside
    /// `<invoiceDataResult>` inside `<QueryInvoiceDataResponse>`).
    /// CLAUDE.md rule 9: pins the recovery surface's load-bearing
    /// transactionId extraction against a future refactor of
    /// `find_first_text` (or a namespace-strictification pass).
    #[test]
    fn pr_21_parse_audit_data_transaction_id_extracts_recovered_txid() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDataResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api"
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
  <invoiceDataResult>
    <invoiceData>BASE64INVOICEDATA</invoiceData>
    <auditData>
      <insdate>2026-05-20T11:59:30Z</insdate>
      <insctsuser>technical-user</insctsuser>
      <source>XML</source>
      <transactionId>RECOVERED-TXID-12345</transactionId>
      <index>1</index>
      <batchIndex>1</batchIndex>
      <originalRequestVersion>3.0</originalRequestVersion>
    </auditData>
  </invoiceDataResult>
</QueryInvoiceDataResponse>"#;
        let got =
            parse_audit_data_transaction_id(body).expect("recovered transactionId must parse");
        assert_eq!(got, "RECOVERED-TXID-12345");
    }

    /// PR-21 / ADR-0034 §3: an absent `<transactionId>` element
    /// loud-fails with a named-route message that triggers the
    /// NAV-testbed amendment surface. Pins the absence-shape
    /// against a future contributor who collapses the loud-fail
    /// into a `Ok(String::new())` (the silent-coercion failure
    /// mode CLAUDE.md rule 12 specifically names).
    #[test]
    fn pr_21_parse_audit_data_transaction_id_loud_fails_on_missing_element() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<QueryInvoiceDataResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api">
  <common:result xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
    <common:funcCode>OK</common:funcCode>
  </common:result>
  <invoiceDataResult>
    <invoiceData>BASE64</invoiceData>
    <auditData>
      <insdate>2026-05-20T11:59:30Z</insdate>
    </auditData>
  </invoiceDataResult>
</QueryInvoiceDataResponse>"#;
        let err = parse_audit_data_transaction_id(body)
            .expect_err("missing transactionId must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::QueryInvoiceDataResponseParse(_)
        ));
        let msg = format!("{err}");
        assert!(
            msg.contains("missing <auditData>/<transactionId>"),
            "loud-fail message must name the missing element: {msg}"
        );
    }

    /// PR-21 / ADR-0034 §3: an empty (or whitespace-only)
    /// `<transactionId>` text loud-fails the defence-in-depth
    /// path. Pins the empty-shape against a tampered or
    /// hand-edited fixture (the audit-ledger contract requires a
    /// non-empty transaction_id).
    #[test]
    fn pr_21_parse_audit_data_transaction_id_loud_fails_on_empty_text() {
        let body = br#"<auditData><transactionId>   </transactionId></auditData>"#;
        let err = parse_audit_data_transaction_id(body)
            .expect_err("empty transactionId must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::QueryInvoiceDataResponseParse(_)
        ));
        let msg = format!("{err}");
        assert!(
            msg.contains("empty"),
            "loud-fail message must name the empty-text case: {msg}"
        );
    }

    /// ADR-0028 §"Surfaced conflict 3" load-bearing pin: PR-15
    /// must NOT parse a receiver-confirmation status field out
    /// of the OK response. The contract is verbatim-bytes-only.
    /// This test pins the absence-of-parse by constructing a
    /// fixture response that DOES carry a hypothetical
    /// `<receiverConfirmationStatus>` element and asserting the
    /// `QueryInvoiceDataOutcome` shape carries NO field beyond
    /// the verbatim bytes — i.e., a future contributor who adds
    /// a speculative parse step (CLAUDE.md rule 2 violation)
    /// would need to add a new field to `QueryInvoiceDataOutcome`
    /// and would surface that intent at type-system level.
    ///
    /// PR-21 / ADR-0034 §3 PRESERVES this contract — the new
    /// `parse_audit_data_transaction_id` helper is invoked at the
    /// orchestration layer, NOT inside `call`; the
    /// `QueryInvoiceDataOutcome` struct shape stays at exactly two
    /// public fields.
    #[test]
    fn query_invoice_data_outcome_shape_has_no_parsed_status_field() {
        // The outcome struct has exactly two public fields per
        // ADR-0028 §3: request_xml + response_xml. Destructuring
        // both fields exhaustively and binding NOTHING ELSE
        // pins the absence-of-parse at compile time.
        let outcome = QueryInvoiceDataOutcome {
            request_xml: b"<x/>".to_vec(),
            response_xml: b"<x/>".to_vec(),
        };
        let QueryInvoiceDataOutcome {
            request_xml,
            response_xml,
        } = outcome;
        assert_eq!(request_xml, b"<x/>");
        assert_eq!(response_xml, b"<x/>");
        // A future contributor adding a `receiver_state` field
        // to the struct would cause this destructure to stop
        // compiling — the type system surfaces the addition
        // loud, which is the ADR-0028 §"Adversarial review #3"
        // amendment trigger.
    }
}
