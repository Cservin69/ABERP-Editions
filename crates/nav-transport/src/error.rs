//! [`NavTransportError`] — the public failure surface of this crate.
//!
//! Every error variant is loud per CLAUDE.md rule 12. There is no
//! "silent fallback" path — a missing keychain item is an error, a
//! malformed embedded PEM is an error, a TLS handshake failure is an
//! error, an unparseable NAV response is an error. None of these
//! resolve to a default.
//!
//! # Variant grouping
//!
//! The variants are grouped (with the file's section comments matching)
//! so a `match err {}` is legible:
//!
//!   1. Trust-store construction (PR-7-A).
//!   2. Reqwest client construction (PR-7-A).
//!   3. Keychain access (PR-7-A).
//!   4. SOAP envelope construction (PR-7-B-1).
//!   5. tokenExchange operation (PR-7-B-2).
//!   6. manageInvoice operation (PR-7-B-3).
//!   7. queryTransactionStatus operation (PR-7-C-1).
//!   8. manageAnnulment operation (PR-13).
//!   9. queryInvoiceData operation (PR-15).
//!  10. queryInvoiceCheck operation (PR-20 / ADR-0033).
//!
//! Each variant carries enough context for an audit-ledger entry without
//! leaking secret material — credential errors deliberately do NOT
//! include secret values in `Display`, and NAV response-parse errors
//! carry the parser's diagnostic but not the raw bytes (the raw bytes
//! live in the audit-ledger verbatim-store path per ADR-0009 §8).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NavTransportError {
    // ── 1. Trust-store construction (PR-7-A) ────────────────────────
    /// The vendored PEM that ships with the binary failed to parse.
    /// This is a build-time invariant — if it fires at runtime, the
    /// binary itself is malformed. ADR-0020 §1 names the pinned
    /// issuing root as part of the build provenance.
    #[error("embedded NAV trust anchor PEM failed to parse: {0}")]
    EmbeddedPemMalformed(String),

    /// rustls rejected a parsed certificate as a trust anchor (e.g.,
    /// the DER decoded but is not a valid CA certificate). Same
    /// severity as `EmbeddedPemMalformed` — this is the binary-is-
    /// malformed path, surfaced separately because the failure reason
    /// differs. The wrapped `String` carries rustls's diagnostic.
    #[error("rustls rejected embedded NAV trust anchor: {0}")]
    EmbeddedCertificateRejected(String),

    // ── 2. Reqwest client construction (PR-7-A) ─────────────────────
    /// `reqwest::ClientBuilder::build()` failed. The most common
    /// proximate cause is a TLS-backend configuration mismatch; the
    /// `#[from]` lets callers inspect the inner error.
    #[error("failed to build reqwest::Client: {0}")]
    ClientBuild(#[source] reqwest::Error),

    // ── 3. Keychain access (PR-7-A) ─────────────────────────────────
    /// A required keychain item is missing. The variant names the item
    /// (login / password / sign-key / change-key) so the operator sees
    /// which one to populate — but never includes the value itself.
    /// Per ADR-0009 §4 + ADR-0020 §3, all four items are required;
    /// partial loading is refused (CLAUDE.md rule 12).
    #[error("NAV credential `{item}` not found in OS keychain for tenant `{tenant_id}`")]
    KeychainItemMissing {
        tenant_id: String,
        item: &'static str,
    },

    /// The keychain backend itself failed (locked keychain, permission
    /// denied, unsupported platform). Distinct from `KeychainItemMissing`
    /// — that one is a populated-keychain-but-missing-entry case, this
    /// one is a keychain-itself-failed case. The `#[source]` preserves
    /// the underlying `keyring::Error` for triage.
    #[error("keychain backend failure for item `{item}`: {source}")]
    KeychainBackend {
        item: &'static str,
        #[source]
        source: keyring::Error,
    },

    // ── 4. SOAP envelope construction (PR-7-B-1) ────────────────────
    /// `quick_xml::Writer` returned an error while assembling the
    /// envelope. The writer targets an in-memory `Vec<u8>`, so a real
    /// I/O failure is effectively unreachable — but if upstream
    /// quick-xml adds new error paths (e.g., depth limit), this is the
    /// surface they bubble through.
    #[error("SOAP envelope write failed: {0}")]
    EnvelopeWriteFailed(String),

    /// `render_manage_invoice_request` was called with an empty
    /// `items` slice. NAV rejects empty invoice operations with
    /// `INCORRECT_REQUEST_SCHEMA`; we catch it at the envelope-
    /// construction boundary so the test that constructs a malformed
    /// request fails before any HTTP call.
    #[error("manageInvoice envelope cannot be built without at least one invoice operation")]
    ManageInvoiceEmpty,

    /// `render_manage_invoice_request` was called with more than the
    /// NAV v3.0 per-request cap of 100 invoice operations. Per ADR-0009
    /// §3 the allocator is per-tenant single-writer, and the
    /// operational pattern is one-invoice-per-call anyway; the cap is
    /// included for defence-in-depth.
    #[error(
        "manageInvoice envelope cannot carry {count} invoice operations (NAV v3.0 cap is 100)"
    )]
    ManageInvoiceTooManyItems { count: usize },

    // ── 5. tokenExchange operation (PR-7-B-2) ───────────────────────
    /// The tokenExchange HTTP call failed at the transport layer (DNS,
    /// connection reset, TLS handshake — caught here, retried only by
    /// the policy in ADR-0009 §5 which is the caller's responsibility).
    /// PR-7-B-2 surfaces this loud; PR-7-C may add a typed retry layer.
    #[error("tokenExchange HTTP call failed: {0}")]
    TokenExchangeHttp(#[source] reqwest::Error),

    /// NAV returned a non-success HTTP status to tokenExchange. The
    /// status is preserved for the audit-ledger entry; the response
    /// body itself is captured separately by the caller (verbatim) per
    /// ADR-0009 §8 and not included in this variant.
    #[error("tokenExchange returned non-success HTTP status: {status}")]
    TokenExchangeHttpStatus { status: u16 },

    /// The tokenExchange response body could not be parsed against the
    /// expected `<TokenExchangeResponse>` shape (missing
    /// `encodedExchangeToken`, malformed XML, unexpected root, etc.).
    /// Loud per CLAUDE.md rule 12 — silent acceptance of a malformed
    /// token is exactly the failure mode we refuse.
    #[error("tokenExchange response parse failed: {0}")]
    TokenExchangeResponseParse(String),

    /// The exchangeToken returned by NAV was not valid base64. NAV
    /// always returns the token base64-encoded per ADR-0020 §2; this
    /// firing means NAV itself sent malformed data (unlikely) or the
    /// parser pulled the wrong element (likelier — investigate the
    /// xpath in `crate::operations::token_exchange`).
    #[error("tokenExchange returned token that is not valid base64: {0}")]
    TokenExchangeBase64Decode(String),

    /// The decrypted exchangeToken ciphertext length is not a multiple
    /// of the AES block size (16 bytes). NAV always pads correctly; if
    /// this fires, either the base64 decode pulled the wrong field or
    /// NAV sent malformed data. Loud-fail rather than truncate.
    #[error("tokenExchange ciphertext length {len} is not a multiple of AES block size 16")]
    TokenExchangeBadCiphertextLength { len: usize },

    /// AES-128/ECB decryption of the exchangeToken failed. The most
    /// common cause is a wrong `xmlChangeKey` (operator populated the
    /// wrong tenant's key, or the technical user was rotated and the
    /// keychain was not updated). Loud — the next step is operator
    /// triage, not retry.
    #[error("tokenExchange AES-128/ECB decryption failed: {0}")]
    TokenExchangeDecryptFailed(String),

    // ── 6. manageInvoice operation (PR-7-B-3) ───────────────────────
    /// HTTP-layer failure on manageInvoice. Same shape as
    /// `TokenExchangeHttp`; held distinct so the audit entry can
    /// distinguish the two operations without reaching for the request
    /// URL.
    #[error("manageInvoice HTTP call failed: {0}")]
    ManageInvoiceHttp(#[source] reqwest::Error),

    /// NAV returned a non-success HTTP status to manageInvoice. The
    /// body itself is captured by the caller per ADR-0009 §8.
    #[error("manageInvoice returned non-success HTTP status: {status}")]
    ManageInvoiceHttpStatus { status: u16 },

    /// The manageInvoice response body could not be parsed against the
    /// expected `<ManageInvoiceResponse>` shape. Loud per CLAUDE.md
    /// rule 12.
    #[error("manageInvoice response parse failed: {0}")]
    ManageInvoiceResponseParse(String),

    /// NAV responded with a non-retryable application-layer error
    /// (`INVALID_SECURITY_USER`, `INVALID_REQUEST_SIGNATURE`,
    /// `INCORRECT_REQUEST_SCHEMA`, `SCHEMA_VIOLATION` per ADR-0009 §5).
    /// The caller transitions the invoice to `SubmissionStuck`; no
    /// automatic retry. The `code` is the NAV error code; `message` is
    /// NAV's human-readable string (which the caller relays to the
    /// operator alert).
    #[error("manageInvoice non-retryable error: {code} — {message}")]
    ManageInvoiceNonRetryable { code: String, message: String },

    /// NAV responded with a retryable application-layer error
    /// (HTTP 504, `OPERATION_FAILED` per ADR-0009 §5). PR-7-B-3 surfaces
    /// this loud; the retry loop with exponential backoff lands in
    /// PR-7-C (the ack-poll PR has the same shape).
    #[error("manageInvoice retryable error: {code} — {message}")]
    ManageInvoiceRetryable { code: String, message: String },

    // ── 7. queryTransactionStatus operation (PR-7-C-1) ──────────────
    /// HTTP-layer failure on queryTransactionStatus. Same shape as
    /// `TokenExchangeHttp` / `ManageInvoiceHttp`; held distinct so the
    /// audit-evidence bundle can distinguish a transport-layer failure
    /// against each operation without inspecting the request URL.
    #[error("queryTransactionStatus HTTP call failed: {0}")]
    QueryTransactionStatusHttp(#[source] reqwest::Error),

    /// NAV returned a non-success HTTP status to queryTransactionStatus.
    /// The body itself is captured by the caller per ADR-0009 §8 (the
    /// poll loop in `apps/aberp/src/poll_ack.rs` does NOT write an
    /// audit entry for this case — there is no parsed `ack_status` to
    /// emit. Operator triage proceeds via the tracing event and the
    /// `SubmissionStuck` typestate transition.).
    #[error("queryTransactionStatus returned non-success HTTP status: {status}")]
    QueryTransactionStatusHttpStatus { status: u16 },

    /// The queryTransactionStatus response body could not be parsed
    /// against the expected `<QueryTransactionStatusResponse>` shape —
    /// missing `<invoiceStatus>`, unknown enumeration value, malformed
    /// XML, unexpected root element. Loud per CLAUDE.md rule 12; the
    /// alternative (treat-as-retry or treat-as-terminal) would either
    /// loop the poll forever or transition to the wrong terminal state.
    #[error("queryTransactionStatus response parse failed: {0}")]
    QueryTransactionStatusResponseParse(String),

    /// NAV responded with a non-retryable application-layer error
    /// (`INVALID_SECURITY_USER`, `INVALID_REQUEST_SIGNATURE`,
    /// `INCORRECT_REQUEST_SCHEMA`, `SCHEMA_VIOLATION` per ADR-0009 §5).
    /// The poll loop transitions the invoice to `SubmissionStuck`; no
    /// further poll attempts.
    #[error("queryTransactionStatus non-retryable error: {code} — {message}")]
    QueryTransactionStatusNonRetryable { code: String, message: String },

    /// NAV responded with a retryable application-layer error
    /// (`OPERATION_FAILED` per ADR-0009 §5). The poll loop treats this
    /// the same as an intermediate ack-status: count the attempt, back
    /// off, and try again until terminal or attempts exhausted.
    #[error("queryTransactionStatus retryable error: {code} — {message}")]
    QueryTransactionStatusRetryable { code: String, message: String },

    // ── 8. manageAnnulment operation (PR-13) ────────────────────────
    /// `render_manage_annulment_request` was called with an empty
    /// `items` slice. Same loud-fail-at-envelope-construction
    /// posture as `ManageInvoiceEmpty`; the failure mode catches a
    /// malformed call before any HTTP request goes out.
    #[error("manageAnnulment envelope cannot be built without at least one annulment operation")]
    ManageAnnulmentEmpty,

    /// `render_manage_annulment_request` was called with more than
    /// the NAV v3.0 per-request cap of 100 annulment operations.
    /// Same cap + defence-in-depth posture as
    /// `ManageInvoiceTooManyItems` per ADR-0009 §3.
    #[error(
        "manageAnnulment envelope cannot carry {count} annulment operations (NAV v3.0 cap is 100)"
    )]
    ManageAnnulmentTooManyItems { count: usize },

    /// HTTP-layer failure on manageAnnulment (DNS, connection reset,
    /// TLS handshake). Same shape as `ManageInvoiceHttp` /
    /// `QueryTransactionStatusHttp`; held distinct so the audit
    /// entry can distinguish operations without reaching for the
    /// URL.
    #[error("manageAnnulment HTTP call failed: {0}")]
    ManageAnnulmentHttp(#[source] reqwest::Error),

    /// NAV returned a non-success HTTP status to manageAnnulment.
    /// The body itself is captured by the caller per ADR-0009 §8
    /// (the audit payload's `response_xml` field carries the
    /// verbatim bytes regardless of HTTP status).
    #[error("manageAnnulment returned non-success HTTP status: {status}")]
    ManageAnnulmentHttpStatus { status: u16 },

    /// The manageAnnulment response body could not be parsed
    /// against the expected `<ManageAnnulmentResponse>` shape
    /// (missing `<transactionId>`, malformed XML, unexpected
    /// root, etc.). Loud per CLAUDE.md rule 12.
    #[error("manageAnnulment response parse failed: {0}")]
    ManageAnnulmentResponseParse(String),

    /// NAV responded with a non-retryable application-layer error
    /// against manageAnnulment. The classification set is shared
    /// across operations per ADR-0009 §5 + ADR-0026 §5; the caller
    /// loud-fails the operator and does not retry.
    #[error("manageAnnulment non-retryable error: {code} — {message}")]
    ManageAnnulmentNonRetryable { code: String, message: String },

    /// NAV responded with a retryable application-layer error
    /// against manageAnnulment (`OPERATION_FAILED`, HTTP 504 per
    /// ADR-0009 §5). PR-13 surfaces this loud per ADR-0026 §5; an
    /// automatic retry loop (mirror of PR-8's retry-submission) is
    /// the named trigger if the operational pattern calls for it.
    #[error("manageAnnulment retryable error: {code} — {message}")]
    ManageAnnulmentRetryable { code: String, message: String },

    // ── 9. queryInvoiceData operation (PR-15 / ADR-0028) ───────────
    /// HTTP-layer failure on queryInvoiceData (DNS, connection
    /// reset, TLS handshake). Same shape as
    /// `QueryTransactionStatusHttp` / `ManageAnnulmentHttp`; held
    /// distinct so the audit entry can distinguish operations
    /// without reaching for the URL.
    #[error("queryInvoiceData HTTP call failed: {0}")]
    QueryInvoiceDataHttp(#[source] reqwest::Error),

    /// NAV returned a non-success HTTP status to queryInvoiceData.
    /// The body itself is captured by the caller per ADR-0009 §8
    /// (the audit payload's `response_xml` field carries the
    /// verbatim bytes regardless of HTTP status).
    #[error("queryInvoiceData returned non-success HTTP status: {status}")]
    QueryInvoiceDataHttpStatus { status: u16 },

    /// The queryInvoiceData response body could not be parsed
    /// against the expected `<QueryInvoiceDataResponse>` shape
    /// (missing `<funcCode>`, malformed XML, unexpected root,
    /// etc.). Loud per CLAUDE.md rule 12. NOTE per ADR-0028 §
    /// "Surfaced conflict 3" the verbatim-bytes-only posture
    /// applies: PR-15 does NOT attempt to parse a receiver-
    /// confirmation field, so this variant fires only on
    /// envelope-shape failures, not on
    /// receiver-confirmation-field absence.
    #[error("queryInvoiceData response parse failed: {0}")]
    QueryInvoiceDataResponseParse(String),

    /// NAV responded with a non-retryable application-layer
    /// error against queryInvoiceData. The classification set is
    /// shared across operations per ADR-0009 §5; the caller
    /// surfaces loud and the operator escalates (credentials /
    /// signature failures dominate this bucket).
    #[error("queryInvoiceData non-retryable error: {code} — {message}")]
    QueryInvoiceDataNonRetryable { code: String, message: String },

    /// NAV responded with a retryable application-layer error
    /// against queryInvoiceData (`OPERATION_FAILED`, HTTP 504 per
    /// ADR-0009 §5). PR-15 surfaces this loud per ADR-0028 §4 —
    /// the one-shot posture means the operator re-runs after the
    /// transient cause resolves (NOT an automatic retry loop;
    /// receiver-confirmation is human-paced and a fixed-cadence
    /// loop is structurally wrong per ADR-0028 §"Surfaced
    /// conflict 2").
    #[error("queryInvoiceData retryable error: {code} — {message}")]
    QueryInvoiceDataRetryable { code: String, message: String },

    // ── 10. queryInvoiceCheck operation (PR-20 / ADR-0033) ─────────
    /// HTTP-layer failure on queryInvoiceCheck (DNS, connection
    /// reset, TLS handshake). Same shape as
    /// `QueryInvoiceDataHttp` / `QueryTransactionStatusHttp`; held
    /// distinct so the audit entry can distinguish operations
    /// without reaching for the URL. PR-20 / ADR-0033 §4.
    ///
    /// `retry-submission`'s state-2 Layer-2 branch routes this
    /// into the new `InvoiceCheckPerformed` audit entry with
    /// `outcome = "failure"` and `failure_class = "transport"`
    /// per ADR-0033 §1's three-phase posture (Phase 0 aborts the
    /// retry on any queryInvoiceCheck failure; the operator
    /// re-runs later).
    #[error("queryInvoiceCheck HTTP call failed: {0}")]
    QueryInvoiceCheckHttp(#[source] reqwest::Error),

    /// NAV returned a non-success HTTP status to queryInvoiceCheck.
    /// The body itself is captured by the caller per ADR-0009 §8
    /// (the `InvoiceCheckPerformedPayload.response_xml` field
    /// carries the verbatim bytes regardless of HTTP status).
    /// PR-20 / ADR-0033 §4.
    #[error("queryInvoiceCheck returned non-success HTTP status: {status}")]
    QueryInvoiceCheckHttpStatus { status: u16 },

    /// The queryInvoiceCheck response body could not be parsed
    /// against the expected `<QueryInvoiceCheckResponse>` shape
    /// (missing `<funcCode>`, missing or non-boolean
    /// `<invoiceCheckResult>`, malformed XML, unexpected root
    /// element, etc.). Loud per CLAUDE.md rule 12 — the boolean
    /// parse refuses silent coercion to either truthiness on
    /// unknown values (`"1"`, `"yes"`, etc.) so a NAV-side
    /// schema change surfaces at the boundary, not as a wrong-
    /// branch retry decision. PR-20 / ADR-0033 §3.
    #[error("queryInvoiceCheck response parse failed: {0}")]
    QueryInvoiceCheckResponseParse(String),

    /// NAV responded with a non-retryable application-layer error
    /// against queryInvoiceCheck (`INVALID_SECURITY_USER`,
    /// `INVALID_REQUEST_SIGNATURE`, etc. per ADR-0009 §5). The
    /// classification set is shared across operations per
    /// ADR-0009 §5; `retry-submission`'s Phase 0 routes this into
    /// the `InvoiceCheckPerformed` audit entry with
    /// `outcome = "failure"` + `failure_class = "application"` and
    /// aborts the retry. PR-20 / ADR-0033 §4.
    #[error("queryInvoiceCheck non-retryable error: {code} — {message}")]
    QueryInvoiceCheckNonRetryable { code: String, message: String },

    /// NAV responded with a retryable application-layer error
    /// against queryInvoiceCheck (`OPERATION_FAILED`, HTTP 504
    /// per ADR-0009 §5). PR-20's Phase 0 surfaces this loud per
    /// ADR-0033 §"Surfaced conflict 1" — the retry aborts on
    /// any queryInvoiceCheck failure (Reading A) regardless of
    /// retryable/non-retryable distinction; the operator re-runs
    /// later. The `InvoiceCheckPerformed` audit entry's
    /// `failure_class = "retryable_application"` keeps the
    /// inspector-visible distinction for triage.
    #[error("queryInvoiceCheck retryable error: {code} — {message}")]
    QueryInvoiceCheckRetryable { code: String, message: String },
}
