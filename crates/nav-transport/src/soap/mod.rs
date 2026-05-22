//! NAV Online Számla v3.0 SOAP envelope assembly.
//!
//! Hand-rolled with `quick-xml` per ADR-0021 §A8 ("codegen-from-XSD
//! rejected for this scope"). The envelope is a thin wrapper around the
//! `crate::signatures` outputs and the `crate::credentials::NavCredentials`
//! values — there is no DSL, no macros, no derive. The shape this module
//! writes is the same shape NAV's v3.0 XSD accepts; conformance is
//! verified by golden fixtures in `crates/nav-transport/tests/`.
//!
//! # What lives here
//!
//!   - [`render_token_exchange_request`] — full `<TokenExchangeRequest>`
//!     XML body, ready to POST to `/tokenExchange`. PR-7-B-2 consumer.
//!   - [`render_manage_invoice_request`] — full `<ManageInvoiceRequest>`
//!     XML body, ready to POST to `/manageInvoice`. PR-7-B-3 consumer.
//!   - [`render_query_transaction_status_request`] — full
//!     `<QueryTransactionStatusRequest>` XML body, ready to POST to
//!     `/queryTransactionStatus`. PR-7-C-1 consumer.
//!   - [`render_manage_annulment_request`] — full
//!     `<ManageAnnulmentRequest>` XML body, ready to POST to
//!     `/manageAnnulment`. PR-13 / ADR-0026 consumer. Structural
//!     mirror of `render_manage_invoice_request` with three element-
//!     name renames per ADR-0026 §3:
//!     `invoiceOperations` → `annulmentOperations`,
//!     `invoiceOperation` (per-item value) → `annulmentOperation`,
//!     `invoiceData` → `invoiceAnnulment`. The wrapping shape
//!     (exchangeToken + common header + user + software) is
//!     identical to `manageInvoice` per ADR-0009 §4.
//!   - [`render_query_invoice_data_request`] — full
//!     `<QueryInvoiceDataRequest>` XML body, ready to POST to
//!     `/queryInvoiceData`. PR-15 / ADR-0028 consumer. Same
//!     non-`manageInvoice` request-signature shape as
//!     `queryTransactionStatus` (no per-invoice-index extension;
//!     keys on the invoice number, not a transaction id).
//!   - [`render_query_invoice_check_request`] — full
//!     `<QueryInvoiceCheckRequest>` XML body, ready to POST to
//!     `/queryInvoiceCheck`. PR-20 / ADR-0033 consumer. Structural
//!     parallel to `queryInvoiceData` — same non-`manageInvoice`
//!     request-signature shape and same `<invoiceNumberQuery>`
//!     body wrapper. Used by `retry-submission`'s state-2 Pending
//!     branch as the Layer-2 NAV-side disambiguation surface per
//!     ADR-0009 §5.
//!
//! Lower-level building blocks (header, user, software type, request-id
//! generation, timestamp formatting) live in [`parts`] so the unit tests
//! can exercise them in isolation.
//!
//! # Namespaces (constant per NAV v3.0)
//!
//!   - **API**     `http://schemas.nav.gov.hu/OSA/3.0/api`     — default
//!     namespace on the request root (`<TokenExchangeRequest>`,
//!     `<ManageInvoiceRequest>`, ...).
//!   - **Common**  `http://schemas.nav.gov.hu/NTCA/1.0/common` — prefix
//!     `common`. Carries header, user, software, generic-result.
//!
//! The `InvoiceData` payload itself uses a separate namespace
//! (`http://schemas.nav.gov.hu/OSA/3.0/data`); that is the responsibility
//! of `apps/aberp/src/nav_xml.rs` (PR-5) and is delivered to NAV
//! base64-encoded inside `<invoiceData>`. The SOAP envelope does NOT need
//! the data namespace.
//!
//! # What this module does NOT do
//!
//!   - It does not call NAV (see `crate::operations`).
//!   - It does not parse responses (see `crate::operations`).
//!   - It does not compute signatures (see `crate::signatures`).
//!   - It does not load credentials (see `crate::credentials`).
//!
//! The split keeps each piece unit-testable in isolation. A future XSD
//! validator (deferred per ADR-0021 §Items deferred) plugs in between
//! this module and `operations` — the bytes this module produces are the
//! ones that would be validated.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::credentials::NavCredentials;
use crate::error::NavTransportError;
use crate::signatures::{
    password_hash, request_signature, request_signature_manage, InvoiceSignatureInput,
};

pub mod parts;

/// Default namespace for NAV API request/response envelopes (v3.0).
pub const NAV_NS_API: &str = "http://schemas.nav.gov.hu/OSA/3.0/api";

/// Namespace bound to prefix `common` (header, user, software, results).
pub const NAV_NS_COMMON: &str = "http://schemas.nav.gov.hu/NTCA/1.0/common";

/// One per-invoice operation for `manageInvoice` per ADR-0009 §6.
///
/// Mirrors NAV v3.0's `ManageInvoiceOperationType`. CREATE / MODIFY /
/// STORNO map to the same `manageInvoice` endpoint; `manageAnnulment`
/// uses a separate enum (`ANNUL` only) and a different envelope shape
/// (PR-7-C scope, not exercised by PR-7-B-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceOperation {
    Create,
    Modify,
    Storno,
}

impl InvoiceOperation {
    /// NAV-facing string per the v3.0 XSD enumeration. Returned as
    /// `&'static str` so it can be passed straight to the signature
    /// helpers ([`InvoiceSignatureInput::operation`]) without an
    /// allocation. Order MUST match the per-invoice signature suffix
    /// (`operation || base64(invoiceData)` — the same string lands on
    /// the wire inside `<operation>` and inside the signature input).
    pub fn as_nav_str(self) -> &'static str {
        match self {
            InvoiceOperation::Create => "CREATE",
            InvoiceOperation::Modify => "MODIFY",
            InvoiceOperation::Storno => "STORNO",
        }
    }
}

/// One element of a `manageInvoice` request: an operation + the
/// `<InvoiceData>` XML bytes to submit at this index.
///
/// Index is assigned implicitly by position in the slice the caller
/// passes to [`render_manage_invoice_request`] — NAV requires `<index>`
/// to start at 1 and increment monotonically. The renderer enforces
/// that contract; the caller does not pass indices.
#[derive(Debug, Clone)]
pub struct ManageInvoiceItem<'a> {
    pub operation: InvoiceOperation,
    /// Raw bytes of the `<InvoiceData>` XML element (the output of
    /// `apps/aberp/src/nav_xml.rs::render_invoice_data`). Will be
    /// base64-encoded onto the wire.
    pub invoice_data_xml: &'a [u8],
}

/// Render a `<TokenExchangeRequest>` body, ready for HTTP POST.
///
/// `tax_number` is the 8-digit base of the taxpayer's tax number per
/// ADR-0009 §4 — NOT the dashed full form (`12345678-1-42`). The caller
/// extracts the first 8 digits before calling; passing the dashed form
/// produces `INVALID_SECURITY_USER` from NAV.
///
/// `request_id` and `request_timestamp` are typically minted via
/// [`parts::new_request_id`] and [`parts::request_timestamp`]; the
/// signatures are computed against the same inputs.
pub fn render_token_exchange_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
) -> Result<Vec<u8>, NavTransportError> {
    let signature = request_signature(request_id, request_timestamp, credentials.sign_key_bytes());
    render_request(
        "TokenExchangeRequest",
        credentials,
        tax_number_8,
        request_id,
        request_timestamp,
        &signature,
        |_w| {
            // tokenExchange has no body beyond the common header/user/software
            // blocks — the request itself IS the token request.
            Ok(())
        },
    )
}

/// Render a `<ManageInvoiceRequest>` body, ready for HTTP POST.
///
/// `exchange_token` is the **decrypted** token bytes (the result of
/// `crate::cipher::decrypt_exchange_token`) — UTF-8-decoded into the
/// `<exchangeToken>` element. NAV returns this token base64-encoded and
/// AES-128/ECB-encrypted; this function is downstream of the decrypt.
///
/// `items` is the per-index invoice list. Per NAV v3.0 the max length
/// is 100; this function ENFORCES the cap (loud-fails on
/// `NavTransportError::ManageInvoiceTooManyItems`) so the caller cannot
/// silently truncate.
pub fn render_manage_invoice_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
    exchange_token: &str,
    items: &[ManageInvoiceItem<'_>],
) -> Result<Vec<u8>, NavTransportError> {
    if items.is_empty() {
        return Err(NavTransportError::ManageInvoiceEmpty);
    }
    if items.len() > 100 {
        return Err(NavTransportError::ManageInvoiceTooManyItems { count: items.len() });
    }

    // Build the per-index inputs for the signature in the same order
    // they will appear on the wire. The signature MUST match the
    // payload byte-for-byte — that's why we compute both from the same
    // `items` slice rather than from two parallel structures.
    let signature_inputs: Vec<InvoiceSignatureInput<'_>> = items
        .iter()
        .map(|i| InvoiceSignatureInput {
            operation: i.operation.as_nav_str(),
            invoice_data_xml: i.invoice_data_xml,
        })
        .collect();
    let signature = request_signature_manage(
        request_id,
        request_timestamp,
        credentials.sign_key_bytes(),
        &signature_inputs,
    );

    render_request(
        "ManageInvoiceRequest",
        credentials,
        tax_number_8,
        request_id,
        request_timestamp,
        &signature,
        |w| {
            // <exchangeToken>...</exchangeToken>
            write_text_in_default_ns(w, "exchangeToken", exchange_token)?;
            // <invoiceOperations>
            w.write_event(Event::Start(BytesStart::new("invoiceOperations")))
                .map_err(envelope_io)?;
            write_text_in_default_ns(w, "compressedContent", "false")?;
            for (i, item) in items.iter().enumerate() {
                let index = (i + 1) as u32;
                w.write_event(Event::Start(BytesStart::new("invoiceOperation")))
                    .map_err(envelope_io)?;
                write_text_in_default_ns(w, "index", &index.to_string())?;
                write_text_in_default_ns(w, "invoiceOperation", item.operation.as_nav_str())?;
                let encoded = BASE64_STANDARD.encode(item.invoice_data_xml);
                write_text_in_default_ns(w, "invoiceData", &encoded)?;
                w.write_event(Event::End(BytesEnd::new("invoiceOperation")))
                    .map_err(envelope_io)?;
            }
            w.write_event(Event::End(BytesEnd::new("invoiceOperations")))
                .map_err(envelope_io)?;
            Ok(())
        },
    )
}

/// One element of a `manageAnnulment` request: an `<InvoiceAnnulment>`
/// XML bytes pointer (PR-13 / ADR-0026 §3).
///
/// Unlike [`ManageInvoiceItem`], this struct carries NO `operation`
/// field: NAV's annulment operation type has exactly one value
/// (`"ANNUL"`), so the operation is implicit at the envelope level
/// and the renderer hard-codes it. Adding a `ManageAnnulmentOperation`
/// enum with one variant would be the speculative abstraction
/// CLAUDE.md rule 2 names; ADR-0026 §3 commits to the literal
/// `"ANNUL"` form, with the named-trigger amendment surface in
/// `render_manage_annulment_request` if NAV ever introduces a second
/// annulment operation value.
///
/// Index is assigned implicitly by position in the slice the caller
/// passes to [`render_manage_annulment_request`] — NAV requires
/// `<index>` to start at 1 and increment monotonically, same
/// posture as [`ManageInvoiceItem`].
#[derive(Debug, Clone)]
pub struct ManageAnnulmentItem<'a> {
    /// Raw bytes of the `<InvoiceAnnulment>` XML element (the output
    /// of `apps/aberp/src/nav_xml.rs::render_annulment_data`). Will
    /// be base64-encoded onto the wire inside `<invoiceAnnulment>`.
    pub invoice_annulment_xml: &'a [u8],
}

/// The NAV `<annulmentOperation>` per-item operation literal per
/// ADR-0026 §3 + §"Surfaced conflict 2". Exposed as a `&'static str`
/// constant so the signature input and the wire element share one
/// source of truth — a future amendment that adds a second
/// operation value lives in one place.
pub const ANNULMENT_OPERATION_ANNUL: &str = "ANNUL";

/// Render a `<ManageAnnulmentRequest>` body, ready for HTTP POST
/// (PR-13 / ADR-0026 §3).
///
/// Structural mirror of [`render_manage_invoice_request`]. The
/// `exchange_token` is the **decrypted** token bytes (same flow as
/// manageInvoice). NAV's per-request cap of 100 items is enforced
/// loud per ADR-0009 §3 + ADR-0026 §3.
///
/// The per-invoice-index signature uses [`InvoiceSignatureInput`]
/// with the operation literal [`ANNULMENT_OPERATION_ANNUL`] and the
/// invoice_annulment_xml bytes — same suffix shape NAV's spec names
/// for both `manageInvoice` and `manageAnnulment` per ADR-0009 §4
/// (verified against the consulted clients per ADR-0026 §3).
pub fn render_manage_annulment_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
    exchange_token: &str,
    items: &[ManageAnnulmentItem<'_>],
) -> Result<Vec<u8>, NavTransportError> {
    if items.is_empty() {
        return Err(NavTransportError::ManageAnnulmentEmpty);
    }
    if items.len() > 100 {
        return Err(NavTransportError::ManageAnnulmentTooManyItems { count: items.len() });
    }

    // Per-index signature inputs: operation is the literal "ANNUL"
    // per ADR-0026 §3, payload is the raw <InvoiceAnnulment> bytes
    // (base64-encoded inside the signature input by `per_invoice_hex`
    // — same as manageInvoice).
    let signature_inputs: Vec<InvoiceSignatureInput<'_>> = items
        .iter()
        .map(|i| InvoiceSignatureInput {
            operation: ANNULMENT_OPERATION_ANNUL,
            invoice_data_xml: i.invoice_annulment_xml,
        })
        .collect();
    let signature = request_signature_manage(
        request_id,
        request_timestamp,
        credentials.sign_key_bytes(),
        &signature_inputs,
    );

    render_request(
        "ManageAnnulmentRequest",
        credentials,
        tax_number_8,
        request_id,
        request_timestamp,
        &signature,
        |w| {
            // <exchangeToken>...</exchangeToken>
            write_text_in_default_ns(w, "exchangeToken", exchange_token)?;
            // <annulmentOperations>
            w.write_event(Event::Start(BytesStart::new("annulmentOperations")))
                .map_err(envelope_io)?;
            // No <compressedContent> for annulment — NAV's
            // manageAnnulment envelope (per the consulted clients)
            // does not carry the compressed-content indicator that
            // manageInvoice does. If the testbed rejects, the
            // amendment is mechanical (add one write_text_in_default_ns
            // call here); the audit-payload contract is unaffected.
            for (i, item) in items.iter().enumerate() {
                let index = (i + 1) as u32;
                w.write_event(Event::Start(BytesStart::new("annulmentOperation")))
                    .map_err(envelope_io)?;
                write_text_in_default_ns(w, "index", &index.to_string())?;
                // Per-item operation element. NAV's manageInvoice
                // names both the wrapper and the inner element
                // `invoiceOperation`; manageAnnulment mirrors the
                // pattern with `annulmentOperation` for both per
                // ADR-0026 §3.
                write_text_in_default_ns(w, "annulmentOperation", ANNULMENT_OPERATION_ANNUL)?;
                let encoded = BASE64_STANDARD.encode(item.invoice_annulment_xml);
                write_text_in_default_ns(w, "invoiceAnnulment", &encoded)?;
                w.write_event(Event::End(BytesEnd::new("annulmentOperation")))
                    .map_err(envelope_io)?;
            }
            w.write_event(Event::End(BytesEnd::new("annulmentOperations")))
                .map_err(envelope_io)?;
            Ok(())
        },
    )
}

/// Render a `<QueryTransactionStatusRequest>` body, ready for HTTP POST.
///
/// PR-7-C-1 consumer. This is a **non-`manageInvoice`** call per ADR-0009
/// §4 — the request signature uses the plain three-input form
/// (`requestId || requestTimestamp || xmlSignKey`), NOT the per-invoice-
/// index extension that `manageInvoice` / `manageAnnulment` require.
///
/// `transaction_id` is the NAV-assigned tracking id returned by a prior
/// `manageInvoice` call (`ManageInvoiceOutcome.transaction_id`). Treated
/// as opaque; ABERP does not parse its shape.
///
/// `returnOriginalRequest` is sent as the literal `false` rather than
/// omitted: NAV's v3.0 XSD declares it `minOccurs=0` with default `false`,
/// but every consulted open-source client (pzs PHP, angro-kft Node) sends
/// it explicitly. Matching that habit avoids `INCORRECT_REQUEST_SCHEMA`
/// surprises when NAV tightens the schema in a future point release.
pub fn render_query_transaction_status_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
    transaction_id: &str,
) -> Result<Vec<u8>, NavTransportError> {
    let signature = request_signature(request_id, request_timestamp, credentials.sign_key_bytes());
    render_request(
        "QueryTransactionStatusRequest",
        credentials,
        tax_number_8,
        request_id,
        request_timestamp,
        &signature,
        |w| {
            // XSD-sequence order per NAV v3.0:
            //   1. transactionId
            //   2. returnOriginalRequest (optional, defaulted false, sent
            //      explicitly per the rationale above)
            write_text_in_default_ns(w, "transactionId", transaction_id)?;
            write_text_in_default_ns(w, "returnOriginalRequest", "false")?;
            Ok(())
        },
    )
}

/// NAV `<invoiceDirection>` enum per the v3.0 XSD
/// `InvoiceDirectionType`. Two values: `OUTBOUND` (caller is the
/// supplier) / `INBOUND` (caller is the customer).
///
/// PR-15 / ADR-0028 §3: receiver-confirmation observation is
/// supplier-side (ABERP is always the supplier for invoices it
/// issued), so the binary path passes [`InvoiceDirection::Outbound`]
/// explicitly. [`InvoiceDirection::Inbound`] is declared today
/// because it is part of NAV's v3.0 enumeration; a future PR
/// (Billingo-migrated reconciliation, per the deferred NAV
/// historical / reconciliation read-path ADR) will use it. Declaring
/// both at variant-declaration time is the same posture
/// [`crate::operations::query_transaction_status::ProcessingStatus`]
/// takes — name every NAV-side enum value the v3.0 XSD names,
/// parse-fail loud on unknowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceDirection {
    /// Caller is the supplier (the invoice was issued BY this
    /// taxpayer). ABERP's `observe-receiver-confirmation` path
    /// uses this variant.
    Outbound,
    /// Caller is the customer (the invoice was issued TO this
    /// taxpayer by another supplier). Not used by PR-15; named
    /// for the future reconciliation read-path PR per ADR-0028
    /// §3 + the deferred NAV historical read-path ADR.
    Inbound,
}

impl InvoiceDirection {
    /// NAV-facing string per the v3.0 XSD enumeration. Returned
    /// as `&'static str` so it can be passed straight to the
    /// envelope renderer without an allocation. `as_*` per the
    /// convention established by
    /// [`InvoiceOperation::as_nav_str`] /
    /// [`crate::operations::query_transaction_status::ProcessingStatus::as_nav_str`].
    pub fn as_nav_str(self) -> &'static str {
        match self {
            InvoiceDirection::Outbound => "OUTBOUND",
            InvoiceDirection::Inbound => "INBOUND",
        }
    }
}

/// Render a `<QueryInvoiceDataRequest>` body, ready for HTTP POST.
///
/// PR-15 / ADR-0028 §3 consumer. This is a
/// **non-`manageInvoice`** call per ADR-0009 §4 — the request
/// signature uses the plain three-input form
/// (`requestId || requestTimestamp || xmlSignKey`), NOT the
/// per-invoice-index extension that `manageInvoice` /
/// `manageAnnulment` require.
///
/// `invoice_number` is the BASE invoice's NAV-facing invoice
/// number string (e.g., `"INV-default/00042"`). The binary
/// constructs it from the base invoice's series code + sequence
/// number — same format every other `<invoiceNumber>` element
/// in ABERP-emitted bodies uses
/// (`apps/aberp/src/nav_xml.rs::render_invoice_data`).
///
/// `invoice_direction` is the typed enum
/// [`InvoiceDirection::Outbound`] for PR-15's supplier-side
/// observation path.
///
/// `batch_index` is the position within a multi-invoice batch
/// per NAV v3.0. ABERP submits single-invoice batches per
/// ADR-0009 §3, so PR-15's binary path passes `1`.
/// `<batchIndex>` is `xs:integer`, rendered as a decimal string.
pub fn render_query_invoice_data_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
    invoice_number: &str,
    invoice_direction: InvoiceDirection,
    batch_index: u32,
) -> Result<Vec<u8>, NavTransportError> {
    let signature = request_signature(request_id, request_timestamp, credentials.sign_key_bytes());
    let batch_index_str = batch_index.to_string();
    let direction_str = invoice_direction.as_nav_str();
    render_request(
        "QueryInvoiceDataRequest",
        credentials,
        tax_number_8,
        request_id,
        request_timestamp,
        &signature,
        |w| {
            // XSD-sequence body per NAV v3.0:
            //   <invoiceNumberQuery>
            //     <invoiceNumber>...</invoiceNumber>
            //     <invoiceDirection>OUTBOUND</invoiceDirection>
            //     <batchIndex>1</batchIndex>
            //   </invoiceNumberQuery>
            //
            // Wrapping element + child order verified at first
            // NAV-testbed run per ADR-0028 §3 + §"Open questions".
            // If the testbed rejects, the amendment is mechanical
            // (rename here); the audit-payload's `response_xml`
            // field carries the verbatim NAV-error response so a
            // wire-rejected attempt is still recorded.
            w.write_event(Event::Start(BytesStart::new("invoiceNumberQuery")))
                .map_err(envelope_io)?;
            write_text_in_default_ns(w, "invoiceNumber", invoice_number)?;
            write_text_in_default_ns(w, "invoiceDirection", direction_str)?;
            write_text_in_default_ns(w, "batchIndex", &batch_index_str)?;
            w.write_event(Event::End(BytesEnd::new("invoiceNumberQuery")))
                .map_err(envelope_io)?;
            Ok(())
        },
    )
}

/// Render a `<QueryInvoiceCheckRequest>` body, ready for HTTP POST.
///
/// PR-20 / ADR-0033 §3 consumer. Structurally parallel to
/// [`render_query_invoice_data_request`] — both are non-
/// `manageInvoice` query operations per ADR-0009 §4 (plain three-
/// input request signature, no per-invoice-index extension; no
/// exchangeToken). The body wrapper is the same `<invoiceNumberQuery>`
/// shape as queryInvoiceData per the structural-parallel posture
/// (ADR-0033 §3): `<invoiceNumber>` + `<invoiceDirection>` +
/// `<batchIndex>`.
///
/// The NAV-side OK response carries `<invoiceCheckResult>true|false</>`
/// which the operations-module parser extracts. Per ADR-0033 §3 +
/// §"Open questions", NAV-testbed verification is the named trigger
/// for amendment if NAV's actual request/response shape differs
/// from the modelled one.
pub fn render_query_invoice_check_request(
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
    invoice_number: &str,
    invoice_direction: InvoiceDirection,
    batch_index: u32,
) -> Result<Vec<u8>, NavTransportError> {
    let signature = request_signature(request_id, request_timestamp, credentials.sign_key_bytes());
    let batch_index_str = batch_index.to_string();
    let direction_str = invoice_direction.as_nav_str();
    render_request(
        "QueryInvoiceCheckRequest",
        credentials,
        tax_number_8,
        request_id,
        request_timestamp,
        &signature,
        |w| {
            // XSD-sequence body per NAV v3.0 (modelled — NAV-testbed
            // verification per ADR-0033 §"Open questions"):
            //   <invoiceNumberQuery>
            //     <invoiceNumber>...</invoiceNumber>
            //     <invoiceDirection>OUTBOUND</invoiceDirection>
            //     <batchIndex>1</batchIndex>
            //   </invoiceNumberQuery>
            //
            // The structural-parallel posture per ADR-0033 §3 mirrors
            // queryInvoiceData's body wrapper verbatim. If NAV-testbed
            // surfaces a different actual shape (e.g., a different
            // wrapping element name for queryInvoiceCheck), the
            // amendment is mechanical (rename here); the audit
            // payload's `response_xml` field carries the verbatim
            // NAV-error response so a wire-rejected attempt is still
            // recorded.
            w.write_event(Event::Start(BytesStart::new("invoiceNumberQuery")))
                .map_err(envelope_io)?;
            write_text_in_default_ns(w, "invoiceNumber", invoice_number)?;
            write_text_in_default_ns(w, "invoiceDirection", direction_str)?;
            write_text_in_default_ns(w, "batchIndex", &batch_index_str)?;
            w.write_event(Event::End(BytesEnd::new("invoiceNumberQuery")))
                .map_err(envelope_io)?;
            Ok(())
        },
    )
}

// ──────────────────────────────────────────────────────────────────────
// Internal: shared envelope shell
// ──────────────────────────────────────────────────────────────────────

/// Common envelope assembly for every NAV v3.0 request: XML decl, root
/// element with the two namespaces, `<common:header>`, `<common:user>`,
/// `<software>`, then the per-operation body via the closure.
///
/// Extracted so the five public renderers above (and any future
/// `queryInvoiceCheck` / `queryInvoiceDigest` / `queryInvoiceChainDigest`)
/// share one body and one set of element-ordering invariants. The
/// closure receives the writer positioned just after the `<software>`
/// close tag and just before the root close tag.
fn render_request<F>(
    root_name: &str,
    credentials: &NavCredentials,
    tax_number_8: &str,
    request_id: &str,
    request_timestamp: &str,
    signature_hex: &str,
    write_body: F,
) -> Result<Vec<u8>, NavTransportError>
where
    F: FnOnce(&mut Writer<&mut Vec<u8>>) -> Result<(), NavTransportError>,
{
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut w = Writer::new(&mut buf);

    // <?xml version="1.0" encoding="UTF-8"?>
    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(envelope_io)?;

    // Root element with the two NAV namespaces. Default namespace is
    // the API namespace so child elements (header, user, software, ...)
    // can be written with the `common:` prefix only when they're in the
    // common namespace.
    let mut root = BytesStart::new(root_name.to_string());
    root.push_attribute(("xmlns", NAV_NS_API));
    root.push_attribute(("xmlns:common", NAV_NS_COMMON));
    w.write_event(Event::Start(root)).map_err(envelope_io)?;

    parts::write_header(&mut w, request_id, request_timestamp)?;
    parts::write_user(
        &mut w,
        credentials.login(),
        &password_hash(credentials.password_bytes()),
        tax_number_8,
        signature_hex,
    )?;
    parts::write_software(&mut w)?;

    // Operation-specific body.
    write_body(&mut w)?;

    w.write_event(Event::End(BytesEnd::new(root_name.to_string())))
        .map_err(envelope_io)?;

    Ok(buf)
}

/// Write `<tag>value</tag>` in the default namespace. Common enough
/// inside the body closures that hoisting it avoids ~5 `write_event`
/// calls per element.
pub(crate) fn write_text_in_default_ns(
    w: &mut Writer<&mut Vec<u8>>,
    tag: &str,
    value: &str,
) -> Result<(), NavTransportError> {
    w.write_event(Event::Start(BytesStart::new(tag.to_string())))
        .map_err(envelope_io)?;
    w.write_event(Event::Text(BytesText::new(value)))
        .map_err(envelope_io)?;
    w.write_event(Event::End(BytesEnd::new(tag.to_string())))
        .map_err(envelope_io)?;
    Ok(())
}

/// Map a `quick_xml::Error` from a writer call into our typed error
/// surface. Writer errors are I/O against an in-memory `Vec<u8>` and
/// should be effectively unreachable, but we treat them loud rather
/// than swallow.
pub(crate) fn envelope_io(e: quick_xml::Error) -> NavTransportError {
    NavTransportError::EnvelopeWriteFailed(e.to_string())
}

#[cfg(test)]
mod tests {
    //! Envelope-level tests. The per-part tests live in `parts.rs`; the
    //! end-to-end golden tests against fixed inputs live in
    //! `crates/nav-transport/tests/soap_golden.rs`.

    use super::*;

    fn fixture_credentials() -> NavCredentials {
        NavCredentials::from_parts(
            "test-tenant",
            "TECHNICAL_LOGIN",
            "tech-password",
            "SIGN-KEY-32BYTES-OF-FAKE-MATERIAL",
            "1234567890ABCDEF",
        )
    }

    #[test]
    fn token_exchange_request_contains_required_blocks() {
        let xml = render_token_exchange_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
        )
        .expect("envelope renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");

        // Sanity: root + namespaces + each common block present.
        assert!(s.contains("<TokenExchangeRequest"));
        assert!(s.contains("xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\""));
        assert!(s.contains("xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\""));
        assert!(s.contains("<common:header>"));
        assert!(s.contains("<common:requestId>REQ12345ABCDEFG</common:requestId>"));
        assert!(s.contains("<common:timestamp>20260520T120000Z</common:timestamp>"));
        assert!(s.contains("<common:user>"));
        assert!(s.contains("<common:login>TECHNICAL_LOGIN</common:login>"));
        assert!(s.contains("<common:taxNumber>12345678</common:taxNumber>"));
        assert!(s.contains("cryptoType=\"SHA-512\""));
        assert!(s.contains("cryptoType=\"SHA3-512\""));
        assert!(s.contains("<software>"));
        // Plaintext credentials must NEVER appear in the rendered envelope.
        assert!(
            !s.contains("tech-password"),
            "plaintext password leaked into envelope: {s}"
        );
        assert!(
            !s.contains("SIGN-KEY-32BYTES-OF-FAKE-MATERIAL"),
            "plaintext sign key leaked into envelope: {s}"
        );
        assert!(
            !s.contains("1234567890ABCDEF"),
            "plaintext change key leaked into envelope: {s}"
        );
    }

    #[test]
    fn manage_invoice_request_orders_indices_from_one() {
        let invoice_xml = b"<InvoiceData>placeholder</InvoiceData>";
        let xml = render_manage_invoice_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &[
                ManageInvoiceItem {
                    operation: InvoiceOperation::Create,
                    invoice_data_xml: invoice_xml,
                },
                ManageInvoiceItem {
                    operation: InvoiceOperation::Create,
                    invoice_data_xml: invoice_xml,
                },
            ],
        )
        .expect("envelope renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");
        assert!(s.contains("<index>1</index>"));
        assert!(s.contains("<index>2</index>"));
        // base64("<InvoiceData>placeholder</InvoiceData>") begins "PEludm9p…"
        // — the leading bytes are stable across base64 implementations and a
        // shape-only check is enough; full byte equality is in the golden
        // fixture test.
        assert!(s.contains("<invoiceData>PEludm9pY2VEYXRhPnBsYWNlaG9sZGVy"));
        assert!(s.contains("<exchangeToken>decrypted-token</exchangeToken>"));
    }

    #[test]
    fn manage_invoice_request_empty_loud_fails() {
        let err = render_manage_invoice_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &[],
        )
        .expect_err("empty list must loud-fail");
        assert!(matches!(err, NavTransportError::ManageInvoiceEmpty));
    }

    #[test]
    fn query_transaction_status_request_contains_required_blocks() {
        let xml = render_query_transaction_status_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "TXID-from-NAV-1234",
        )
        .expect("envelope renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");

        // Root + namespaces.
        assert!(s.contains("<QueryTransactionStatusRequest"));
        assert!(s.contains("xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\""));
        assert!(s.contains("xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\""));

        // The four common-block invariants.
        assert!(s.contains("<common:requestId>REQ12345ABCDEFG</common:requestId>"));
        assert!(s.contains("<common:timestamp>20260520T120000Z</common:timestamp>"));
        assert!(s.contains("<common:login>TECHNICAL_LOGIN</common:login>"));
        assert!(s.contains("<common:taxNumber>12345678</common:taxNumber>"));
        assert!(s.contains("cryptoType=\"SHA-512\""));
        assert!(s.contains("cryptoType=\"SHA3-512\""));

        // The two operation-specific elements, in XSD-sequence order.
        let r_txid = s.find("<transactionId>").expect("transactionId present");
        let r_ret = s
            .find("<returnOriginalRequest>")
            .expect("returnOriginalRequest present");
        assert!(
            r_txid < r_ret,
            "transactionId must precede returnOriginalRequest: {s}"
        );
        assert!(s.contains("<transactionId>TXID-from-NAV-1234</transactionId>"));
        assert!(s.contains("<returnOriginalRequest>false</returnOriginalRequest>"));

        // Plaintext credentials MUST NOT appear.
        assert!(
            !s.contains("tech-password"),
            "plaintext password leaked into envelope"
        );
        assert!(
            !s.contains("SIGN-KEY-32BYTES-OF-FAKE-MATERIAL"),
            "plaintext sign key leaked into envelope"
        );
        assert!(
            !s.contains("1234567890ABCDEF"),
            "plaintext change key leaked into envelope"
        );
    }

    // ── PR-13 / ADR-0026 §3: manageAnnulment envelope tests ────────

    /// Happy-path: a one-item manageAnnulment envelope carries the
    /// required blocks + the per-index entry shape. Same load-bearing
    /// shape-only check as
    /// `manage_invoice_request_orders_indices_from_one`; full byte
    /// equality lives in the golden fixture file (added in PR-13's
    /// integration test suite).
    #[test]
    fn manage_annulment_request_contains_required_blocks() {
        let annulment_xml = b"<InvoiceAnnulment>placeholder</InvoiceAnnulment>";
        let xml = render_manage_annulment_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &[ManageAnnulmentItem {
                invoice_annulment_xml: annulment_xml,
            }],
        )
        .expect("envelope renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");

        // Root + namespaces.
        assert!(s.contains("<ManageAnnulmentRequest"));
        assert!(s.contains("xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\""));
        assert!(s.contains("xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\""));

        // Common-block invariants.
        assert!(s.contains("<common:requestId>REQ12345ABCDEFG</common:requestId>"));
        assert!(s.contains("<common:timestamp>20260520T120000Z</common:timestamp>"));
        assert!(s.contains("<common:login>TECHNICAL_LOGIN</common:login>"));
        assert!(s.contains("<common:taxNumber>12345678</common:taxNumber>"));

        // Operation-specific shape per ADR-0026 §3.
        assert!(s.contains("<exchangeToken>decrypted-token</exchangeToken>"));
        assert!(s.contains("<annulmentOperations>"));
        assert!(s.contains("<annulmentOperation>"));
        assert!(s.contains("<index>1</index>"));
        // The per-item operation literal is "ANNUL" per
        // ADR-0026 §3 / ADR-0025 §"Surfaced conflict 2".
        assert!(s.contains("<annulmentOperation>ANNUL</annulmentOperation>"));
        // base64("<InvoiceAnnulment>placeholder</InvoiceAnnulment>")
        // — the leading bytes are stable across base64
        // implementations; shape-only check is enough.
        assert!(s.contains("<invoiceAnnulment>PEludm9pY2VBbm51bG1lbnQ+"));

        // Plaintext credentials MUST NOT leak.
        assert!(
            !s.contains("tech-password"),
            "plaintext password leaked into annulment envelope: {s}"
        );
        assert!(
            !s.contains("SIGN-KEY-32BYTES-OF-FAKE-MATERIAL"),
            "plaintext sign key leaked: {s}"
        );
        assert!(
            !s.contains("1234567890ABCDEF"),
            "plaintext change key leaked: {s}"
        );
    }

    /// Empty `items` slice must loud-fail per ADR-0026 §3 — same
    /// posture as `ManageInvoiceEmpty`. CLAUDE.md rule 12.
    #[test]
    fn manage_annulment_request_empty_loud_fails() {
        let err = render_manage_annulment_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &[],
        )
        .expect_err("empty list must loud-fail");
        assert!(matches!(err, NavTransportError::ManageAnnulmentEmpty));
    }

    /// 100-item cap mirrors `manageInvoice`. 101 items must loud-fail.
    #[test]
    fn manage_annulment_request_over_cap_loud_fails() {
        let annulment_xml = b"<InvoiceAnnulment>x</InvoiceAnnulment>";
        let items: Vec<ManageAnnulmentItem<'_>> = (0..101)
            .map(|_| ManageAnnulmentItem {
                invoice_annulment_xml: annulment_xml,
            })
            .collect();
        let err = render_manage_annulment_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &items,
        )
        .expect_err("over-cap must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::ManageAnnulmentTooManyItems { count: 101 }
        ));
    }

    /// Per-index indices increment from 1 — same shape as
    /// `manage_invoice_request_orders_indices_from_one`.
    #[test]
    fn manage_annulment_request_orders_indices_from_one() {
        let annulment_xml = b"<InvoiceAnnulment>x</InvoiceAnnulment>";
        let xml = render_manage_annulment_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &[
                ManageAnnulmentItem {
                    invoice_annulment_xml: annulment_xml,
                },
                ManageAnnulmentItem {
                    invoice_annulment_xml: annulment_xml,
                },
            ],
        )
        .expect("envelope renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");
        assert!(s.contains("<index>1</index>"));
        assert!(s.contains("<index>2</index>"));
    }

    #[test]
    fn manage_invoice_request_over_cap_loud_fails() {
        let invoice_xml = b"<InvoiceData>x</InvoiceData>";
        let items: Vec<ManageInvoiceItem<'_>> = (0..101)
            .map(|_| ManageInvoiceItem {
                operation: InvoiceOperation::Create,
                invoice_data_xml: invoice_xml,
            })
            .collect();
        let err = render_manage_invoice_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "decrypted-token",
            &items,
        )
        .expect_err("over-cap must loud-fail");
        assert!(matches!(
            err,
            NavTransportError::ManageInvoiceTooManyItems { count: 101 }
        ));
    }

    // ── PR-15 / ADR-0028 §3: queryInvoiceData envelope tests ───────

    /// `InvoiceDirection::as_nav_str` round-trips against the two
    /// values NAV v3.0's XSD names. Same shape as
    /// `InvoiceOperation::as_nav_str` /
    /// `ProcessingStatus::as_nav_str` — if a future contributor
    /// adds a third value without updating the as_nav_str arm,
    /// the missing arm is a compile error; this test pins the
    /// canonical wire forms.
    #[test]
    fn invoice_direction_as_nav_str_matches_xsd_enumeration() {
        assert_eq!(InvoiceDirection::Outbound.as_nav_str(), "OUTBOUND");
        assert_eq!(InvoiceDirection::Inbound.as_nav_str(), "INBOUND");
    }

    /// Happy-path: a queryInvoiceData envelope carries the required
    /// blocks + the operation-specific `<invoiceNumberQuery>` shape
    /// per ADR-0028 §3. Same load-bearing shape-only check as the
    /// `query_transaction_status_request_contains_required_blocks`
    /// test (the closest existing template).
    #[test]
    fn query_invoice_data_request_contains_required_blocks() {
        let xml = render_query_invoice_data_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "INV-default/00042",
            InvoiceDirection::Outbound,
            1,
        )
        .expect("envelope renders");
        let s = std::str::from_utf8(&xml).expect("UTF-8");

        // Root + namespaces.
        assert!(s.contains("<QueryInvoiceDataRequest"));
        assert!(s.contains("xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\""));
        assert!(s.contains("xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\""));

        // Common-block invariants (same shape as
        // queryTransactionStatus — non-`manageInvoice` signature
        // form).
        assert!(s.contains("<common:requestId>REQ12345ABCDEFG</common:requestId>"));
        assert!(s.contains("<common:timestamp>20260520T120000Z</common:timestamp>"));
        assert!(s.contains("<common:login>TECHNICAL_LOGIN</common:login>"));
        assert!(s.contains("<common:taxNumber>12345678</common:taxNumber>"));

        // Operation-specific shape per ADR-0028 §3 + ADR-0028
        // §"Open questions" — element names verified at first
        // NAV-testbed run.
        assert!(s.contains("<invoiceNumberQuery>"));
        assert!(s.contains("<invoiceNumber>INV-default/00042</invoiceNumber>"));
        assert!(s.contains("<invoiceDirection>OUTBOUND</invoiceDirection>"));
        assert!(s.contains("<batchIndex>1</batchIndex>"));

        // XSD-sequence order pin per ADR-0028 §3: the three
        // children of <invoiceNumberQuery> must appear in
        // invoiceNumber → invoiceDirection → batchIndex order.
        let r_num = s
            .find("<invoiceNumber>")
            .expect("invoiceNumber present");
        let r_dir = s
            .find("<invoiceDirection>")
            .expect("invoiceDirection present");
        let r_bat = s.find("<batchIndex>").expect("batchIndex present");
        assert!(
            r_num < r_dir && r_dir < r_bat,
            "invoiceNumber → invoiceDirection → batchIndex order required: {s}"
        );

        // Plaintext credentials MUST NOT leak.
        assert!(
            !s.contains("tech-password"),
            "plaintext password leaked into queryInvoiceData envelope: {s}"
        );
        assert!(
            !s.contains("SIGN-KEY-32BYTES-OF-FAKE-MATERIAL"),
            "plaintext sign key leaked into queryInvoiceData envelope: {s}"
        );
    }

    /// `InvoiceDirection::Inbound` round-trips through the
    /// renderer cleanly even though PR-15's binary path does not
    /// use it. Declaring both variants at the type level (per
    /// ADR-0028 §3) means the renderer must accept either; a
    /// future reconciliation-side caller passing `Inbound`
    /// produces a valid envelope without re-touching this code.
    #[test]
    fn query_invoice_data_request_supports_inbound_direction() {
        let xml = render_query_invoice_data_request(
            &fixture_credentials(),
            "12345678",
            "REQ12345ABCDEFG",
            "20260520T120000Z",
            "SUPPLIER-INV-99",
            InvoiceDirection::Inbound,
            1,
        )
        .expect("envelope renders for INBOUND direction");
        let s = std::str::from_utf8(&xml).expect("UTF-8");
        assert!(s.contains("<invoiceDirection>INBOUND</invoiceDirection>"));
    }
}
