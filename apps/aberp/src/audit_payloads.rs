//! Typed audit-ledger payload schemas for events the binary writes.
//!
//! # Why typed payloads, not `format!`-built JSON
//!
//! PR-5 wrote audit payloads via ad-hoc string interpolation:
//!
//! ```ignore
//! format!("{{\"invoice_id\":\"{}\",\"seq\":{},...}}", ...)
//! ```
//!
//! This was fine for the values PR-5 interpolated (Crockford-base32
//! ULIDs and unsigned integers — no characters that JSON would need
//! to escape). The trap is that PR-7's NAV submission path puts
//! verbatim NAV XML response bodies into audit payloads
//! (ADR-0009 §8), and any quote / backslash / control character in
//! the body produces malformed JSON inside an opaque `BLOB` column
//! with no SQL error, no log, no test failure until something
//! downstream tries to parse the column back.
//!
//! PR-6.1 (Fortnightly review F9) closes the trap at the source:
//! every payload the binary writes goes through `serde_json::to_vec`
//! on a typed struct defined here. The audit-ledger crate's surface
//! remains `Vec<u8>`-shaped — discipline lives at the call site.
//!
//! # Schema versioning
//!
//! Each payload type carries an implicit schema. Adding a field is
//! backward-compatible (older readers see the old shape via
//! `#[serde(default)]` if they choose to parse). Removing a field
//! or changing a field's semantic shape requires a *new* `EventKind`
//! variant (per `crates/audit-ledger/src/entry/event_kind.rs`
//! header: "bumping a payload schema renames the kind, and the old
//! kind remains valid for historical entries").

use std::path::PathBuf;

use aberp_billing::{Currency, IdempotencyKey, RateMetadata, ReadyInvoice, SequenceReservation};
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// InvoiceSequenceReserved
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceSequenceReserved`].
///
/// Written by the binary's `run_single_tx` on the `Fresh` branch of
/// the allocator outcome — i.e. exactly when a sequence number was
/// burned. On replay, this event is **not** re-written; the prior
/// issuance's entry remains the canonical record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceSequenceReservedPayload {
    pub invoice_id: String,
    pub seq: u64,
    pub reservation_id: String,
    pub idempotency_key: String,
}

impl InvoiceSequenceReservedPayload {
    pub fn from_outcome(
        invoice: &ReadyInvoice,
        reservation: &SequenceReservation,
        idempotency_key: IdempotencyKey,
    ) -> Self {
        Self {
            invoice_id: invoice.id.to_prefixed_string(),
            seq: invoice.sequence_number,
            reservation_id: reservation.id.to_prefixed_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
        }
    }

    /// Serialize to bytes for the audit-ledger `payload` column.
    /// `serde_json::to_vec` on a typed struct cannot produce malformed
    /// JSON — quotes, backslashes, control chars, and non-ASCII in any
    /// `String` field are escaped per the spec.
    ///
    /// Borrows `&self` and returns a fresh `Vec<u8>`, hence the `to_*`
    /// name (Rust convention: `as_*` is cheap-reference, `to_*` is
    /// owned-by-clone-or-allocate, `into_*` consumes `self`).
    pub fn to_bytes(&self) -> Vec<u8> {
        // unwrap: serializing fixed-shape value-only structs to JSON
        // bytes cannot fail. The only error path serde_json::to_vec
        // surfaces for these types is OOM, which we treat as a
        // process-level fatal — matching anyhow `?` behaviour upstack.
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceDraftCreated
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceDraftCreated`].
///
/// Written on the same `Fresh` branch as
/// [`InvoiceSequenceReservedPayload`], in the same DuckDB transaction
/// (PR-6 close-out). The fields are intentionally narrow today —
/// just the invoice id and line count — because the full draft
/// content is reconstructible from the `invoice` + `invoice_line`
/// tables. The payload is a pointer, not a duplicate.
///
/// # `nav_xml_path` (PR-18, ADR-0031 §2)
///
/// The on-disk path the binary wrote the NAV InvoiceData XML to
/// (`issue-invoice --out`, `issue-storno --out`,
/// `issue-modification --out`). Consumed by the
/// `drain-submission-queue` worker so it can submit the verbatim
/// bytes without an operator-provided per-invoice path argument.
///
/// `#[serde(default)]` keeps pre-PR-18 entries readable — they
/// deserialise with `nav_xml_path: None`. The drain worker loud-
/// fails on `None` entries unless the operator passes a per-
/// invocation `--xml-path-override` flag (CLAUDE.md rule 12: the
/// missing-path case is operator-visible, never silent).
///
/// Adding the field this way is the additive path the audit-
/// payloads header explicitly names: "Adding a field is backward-
/// compatible (older readers see the old shape via
/// `#[serde(default)]` if they choose to parse)." Removing or
/// renaming would change the payload's semantic shape and require
/// a new `EventKind` variant; the additive surface here keeps the
/// existing `InvoiceDraftCreated` kind unchanged. F12 four-edit
/// ritual does NOT fire for PR-18.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceDraftCreatedPayload {
    pub invoice_id: String,
    pub line_count: usize,
    pub idempotency_key: String,
    /// PR-18 / ADR-0031 §2. NAV InvoiceData XML path the issuing
    /// binary wrote at `--out` time. `None` for pre-PR-18 entries.
    #[serde(default)]
    pub nav_xml_path: Option<String>,
    /// PR-44γ / ADR-0037 §3. Invoice currency in ISO 4217 form (`"HUF"`
    /// or `"EUR"` per the closed vocab at PR-44γ time). Same wire form
    /// as `Currency::iso_code()`; pinned by
    /// `currency_wire_shape_pins_iso_4217_strings` in
    /// `modules/billing/src/domain/money.rs`. Pre-PR-44γ entries
    /// deserialise with `currency: None`; the binary's read path treats
    /// `None` AND `Some("HUF")` identically (both are HUF invoices).
    #[serde(default)]
    pub currency: Option<String>,
    /// PR-44γ / ADR-0037 §1.a + §1.b. The applied MNB rate value as a
    /// canonical-decimal string (matches `rust_decimal::Decimal`'s
    /// `to_string` output; round-trips back via
    /// `Decimal::from_str`). `Some(_)` iff `currency` is a non-HUF
    /// variant; `None` for HUF (the C10 byte-identical invariant
    /// prerequisite — HUF rows carry no rate metadata).
    #[serde(default)]
    pub exchange_rate: Option<String>,
    /// PR-44γ / ADR-0037 §1.a + §2.a. Source identifier — the literal
    /// `"MNB"` constant from `aberp_mnb_rates::SOURCE`. `Some("MNB")`
    /// for EUR rows; `None` for HUF. Read by the future printed-
    /// invoice render (PR-44ε / C9).
    #[serde(default)]
    pub exchange_rate_source: Option<String>,
    /// PR-44γ / ADR-0037 §1.a + §2.b. Publication date of the rate
    /// that was applied, in canonical `YYYY-MM-DD` form. May differ
    /// from the supply-fulfillment date if MNB walked back to the
    /// most-recent prior publication date (weekend, holiday). Read by
    /// the future printed-invoice render's `Exchange-rate date` field.
    /// `Some(_)` for non-HUF rows; `None` for HUF.
    #[serde(default)]
    pub exchange_rate_date: Option<String>,
    /// PR-44γ / ADR-0037 §1.c + §4 invariant C11 / A137. The round-
    /// half-even HUF-equivalent of the invoice's gross total, in whole
    /// forints, expressed as a canonical-decimal string (DECIMAL(18,0)
    /// at the storage layer). `Some(_)` for non-HUF rows; `None` for
    /// HUF (HUF rows carry the regulatory HUF amount in the line
    /// items directly).
    #[serde(default)]
    pub huf_equivalent_total: Option<String>,
}

impl InvoiceDraftCreatedPayload {
    /// Pre-PR-18 constructor — keeps the round-trip test
    /// (`draft_created_round_trip`) and any future call site that
    /// has no XML path to record. Sets `nav_xml_path: None` AND the
    /// five PR-44γ rate-metadata fields to `None`. Default HUF
    /// posture (the only currency available before PR-44γ).
    pub fn from_invoice(invoice: &ReadyInvoice, idempotency_key: IdempotencyKey) -> Self {
        Self {
            invoice_id: invoice.id.to_prefixed_string(),
            line_count: invoice.lines.len(),
            idempotency_key: idempotency_key.to_canonical_string(),
            nav_xml_path: None,
            currency: None,
            exchange_rate: None,
            exchange_rate_source: None,
            exchange_rate_date: None,
            huf_equivalent_total: None,
        }
    }

    /// PR-18 constructor — populates `nav_xml_path` from the
    /// `--out` argument the issuing binary received. The three
    /// issue-* binary call sites (`issue_invoice`, `issue_storno`,
    /// `issue_modification`) switch to this constructor; the
    /// path is converted via `Path::to_string_lossy().to_string()`
    /// at the call site, matching the operator-chosen path on
    /// disk byte-for-byte except where the OS reports a non-UTF-8
    /// path (rare; the operator-visible failure surfaces at file-
    /// read time in the drain worker per CLAUDE.md rule 12).
    ///
    /// PR-44γ — HUF path. Currency is HUF (so the five rate-metadata
    /// fields are `None`). Stamps `currency: Some("HUF")` so a future
    /// reader can distinguish "pre-PR-44γ entry (currency = None)"
    /// from "explicit HUF entry post-PR-44γ" without ambiguity.
    pub fn from_invoice_with_xml_path(
        invoice: &ReadyInvoice,
        idempotency_key: IdempotencyKey,
        nav_xml_path: PathBuf,
    ) -> Self {
        Self {
            invoice_id: invoice.id.to_prefixed_string(),
            line_count: invoice.lines.len(),
            idempotency_key: idempotency_key.to_canonical_string(),
            nav_xml_path: Some(nav_xml_path.to_string_lossy().to_string()),
            currency: Some(Currency::Huf.iso_code().to_string()),
            exchange_rate: None,
            exchange_rate_source: None,
            exchange_rate_date: None,
            huf_equivalent_total: None,
        }
    }

    /// PR-44γ / ADR-0037 — non-HUF constructor. Stamps the currency +
    /// rate-metadata quintet onto the audit payload alongside the
    /// existing PR-18 `nav_xml_path` field. Called by the binary's
    /// `issue_invoice::run()` when `--currency EUR` is in effect.
    ///
    /// The exchange-rate date is rendered in canonical `YYYY-MM-DD`
    /// form (the same form ADR-0037 §1.a names for the
    /// `Exchange-rate date` printed-invoice field); the rate value is
    /// the canonical `rust_decimal::Decimal::to_string` form
    /// (DECIMAL(18,6) at the DuckDB storage layer); the HUF-equivalent
    /// total is the round-half-even integer per ADR-0037 §1.c + C11.
    pub fn from_invoice_with_rate(
        invoice: &ReadyInvoice,
        idempotency_key: IdempotencyKey,
        nav_xml_path: Option<PathBuf>,
        currency: Currency,
        rate: &RateMetadata,
    ) -> Self {
        let date_str = rate
            .date
            .format(&time::macros::format_description!(
                "[year]-[month]-[day]"
            ))
            .unwrap_or_else(|_| "INVALID-DATE".to_string());
        Self {
            invoice_id: invoice.id.to_prefixed_string(),
            line_count: invoice.lines.len(),
            idempotency_key: idempotency_key.to_canonical_string(),
            nav_xml_path: nav_xml_path.map(|p| p.to_string_lossy().to_string()),
            currency: Some(currency.iso_code().to_string()),
            exchange_rate: Some(rate.rate.to_string()),
            exchange_rate_source: Some(rate.source.clone()),
            exchange_rate_date: Some(date_str),
            huf_equivalent_total: Some(rate.huf_equivalent_total.to_string()),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceSubmissionAttempt  (PR-7-B-3 — ADR-0009 §8 invoice.submission_attempt)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceSubmissionAttempt`].
///
/// Written by the binary's `submit_invoice` flow just BEFORE the
/// `manageInvoice` POST returns — capturing the request before the
/// response means a crash mid-flight still leaves the audit trail
/// pointing at "we tried to submit X with body Y", which is the
/// evidence ADR-0009 §8 names.
///
/// `request_xml` is the verbatim bytes of the `<ManageInvoiceRequest>`
/// envelope POSTed to NAV (NOT the inner `<InvoiceData>`; that is
/// reconstructable from the local `invoice` table + the per-index
/// position recorded here). The typed-struct path through
/// `serde_json::to_vec` handles all JSON escaping — closes F9 for the
/// NAV submission path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceSubmissionAttemptPayload {
    pub invoice_id: String,
    pub idempotency_key: String,
    /// `"test"` or `"production"` — which NAV environment we POSTed to.
    /// Recorded so the audit-evidence bundle (ADR-0009 §8) makes the
    /// environment explicit; a production invoice attempted against
    /// `api-test` is a class of operator-error that should be visible
    /// in the ledger without consulting the URL.
    pub endpoint: String,
    /// Verbatim `<ManageInvoiceRequest>` bytes (UTF-8 — the envelope is
    /// always XML; serde_json::to_vec base64-encodes Vec<u8> by
    /// default, so this round-trips cleanly even with embedded quotes,
    /// backslashes, or non-ASCII bytes in the invoice descriptions).
    pub request_xml: Vec<u8>,
}

impl InvoiceSubmissionAttemptPayload {
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        endpoint: &'static str,
        request_xml: Vec<u8>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            endpoint: endpoint.to_string(),
            request_xml,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceSubmissionResponse  (PR-7-B-3 — ADR-0009 §8 invoice.submission_response)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceSubmissionResponse`].
///
/// Written immediately after a successful `manageInvoice` response is
/// received. Carries the verbatim `<ManageInvoiceResponse>` bytes per
/// ADR-0009 §8 plus the parsed `transaction_id` (NAV's opaque tracking
/// token used by `queryTransactionStatus` polls — PR-7-C).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceSubmissionResponsePayload {
    pub invoice_id: String,
    pub idempotency_key: String,
    /// NAV-assigned transaction id. Opaque to ABERP; passed verbatim
    /// to `queryTransactionStatus` in PR-7-C.
    pub transaction_id: String,
    pub response_xml: Vec<u8>,
}

impl InvoiceSubmissionResponsePayload {
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        transaction_id: &str,
        response_xml: Vec<u8>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            transaction_id: transaction_id.to_string(),
            response_xml,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceAckStatus  (PR-7-C — variant declared in PR-7-B-3 to close
// the three-coordinated-edit trap; payload type lives here for the
// same reason — typed at first use, not at first emission.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceAckStatus`].
///
/// Written by the PR-7-C poll loop after each `queryTransactionStatus`
/// call. Carries the parsed ack status and the verbatim response body.
/// One entry per poll — `RECEIVED → PROCESSING → SAVED|ABORTED` is the
/// expected sequence, but ABERP records every poll's result so the
/// audit-evidence bundle (ADR-0009 §8) shows the full latency curve.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceAckStatusPayload {
    pub invoice_id: String,
    pub transaction_id: String,
    /// `"RECEIVED"` | `"PROCESSING"` | `"SAVED"` | `"ABORTED"` per NAV
    /// v3.0. Recorded verbatim; the typed Rust state-machine transition
    /// (PR-7-C scope) is downstream of this.
    pub ack_status: String,
    pub response_xml: Vec<u8>,
}

impl InvoiceAckStatusPayload {
    pub fn new(
        invoice_id: &str,
        transaction_id: &str,
        ack_status: &str,
        response_xml: Vec<u8>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            transaction_id: transaction_id.to_string(),
            ack_status: ack_status.to_string(),
            response_xml,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceRetryRequested  (PR-8 — operator-initiated retry of a stuck
// invoice per ADR-0009 §5. Distinct from `InvoiceSubmissionAttempt`:
// the operator's *decision* to retry is the audit-bearing event, and
// the retry itself fires the normal Attempt/Response pair after.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceRetryRequested`].
///
/// Captures the **precondition justification** for the retry (the
/// prior NAV `transaction_id` and the last observed ack status, if
/// any) alongside the operator's reason text. Reading just this entry
/// from the audit-evidence bundle (ADR-0009 §8) lets a NAV inspector
/// reconstruct "the operator chose to retry because X was the prior
/// ack and the prior submission did not finalize" without walking
/// the full chain.
///
/// `prior_last_ack_status` is `None` iff no `InvoiceAckStatus` entry
/// exists for this invoice yet (operator retried before running
/// `poll-ack` — legitimate but unusual; surfaced so a future audit
/// can see it happened).
///
/// # PR-19 / ADR-0032 §4 — state-2 Pending support
///
/// `prior_transaction_id` becomes `Option<String>` to carry the
/// `StuckPrecondition.prior_transaction_id` verbatim from the
/// precondition walker. State-3 `AwaitingAck` (the existing
/// PR-8 surface) writes `Some(transaction_id)` from the prior
/// `InvoiceSubmissionResponse`; state-2 `Pending` (the new
/// ADR-0032 §4 surface) writes `None` because no
/// `InvoiceSubmissionResponse` exists yet.
///
/// Pre-PR-19 entries deserialise transparently — JSON strings
/// map to `Some(String)` via serde_json's default `Option<T>`
/// deserialisation; pre-PR-19 entries always wrote a non-null
/// string in this field so the round-trip is `String → Some(String)`.
/// The `#[serde(default)]` attribute is NOT strictly required for
/// the round-trip path, but is added defensively against any future
/// entry shape that elides the field entirely.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceRetryRequestedPayload {
    pub invoice_id: String,
    pub idempotency_key: String,
    /// The NAV `transaction_id` recorded by the most-recent prior
    /// `InvoiceSubmissionResponse` for this invoice (`Some` for
    /// state-3 `AwaitingAck` retries) OR `None` for state-2
    /// `Pending` retries (no prior `InvoiceSubmissionResponse`
    /// exists — the prior Attempt's wire either broke or the
    /// process crashed before TX2 commit per ADR-0032 §1). The
    /// retry's own `InvoiceSubmissionResponse` (on success) or
    /// `InvoiceSubmissionAttemptFailed` (on failure) will record
    /// a fresh outcome regardless of stage; keeping the prior
    /// id here makes the state-3 unblock decision traceable
    /// without walking the chain.
    #[serde(default)]
    pub prior_transaction_id: Option<String>,
    /// The string form of the most-recent `InvoiceAckStatus` payload's
    /// `ack_status` field for this invoice. `None` if no ack entry
    /// exists (the operator retried before polling — legitimate;
    /// or state-2 `Pending` retries — by construction no
    /// `InvoiceSubmissionResponse` exists so no ack poll could
    /// have run). Captured here rather than silently elided.
    pub prior_last_ack_status: Option<String>,
    /// Free-form operator-supplied reason for the retry. Required at
    /// the CLI surface so the audit-evidence bundle (ADR-0009 §8)
    /// always carries a human-readable justification.
    pub reason: String,
}

impl InvoiceRetryRequestedPayload {
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        prior_transaction_id: Option<String>,
        prior_last_ack_status: Option<String>,
        reason: &str,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            prior_transaction_id,
            prior_last_ack_status,
            reason: reason.to_string(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceMarkedAbandoned  (PR-8 — operator chose to stop retrying a
// stuck invoice per ADR-0009 §5. Terminal in the audit ledger.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceMarkedAbandoned`].
///
/// Same precondition-justification shape as
/// [`InvoiceRetryRequestedPayload`]. The two payloads share their
/// fields by design: an audit-evidence bundle reader treats
/// "retry-requested" and "marked-abandoned" as paired operator
/// decisions on the same `SubmissionStuck` precondition surface.
///
/// # PR-19 / ADR-0032 §4 — state-2 Pending support
///
/// `prior_transaction_id` becomes `Option<String>` matching
/// [`InvoiceRetryRequestedPayload`]'s shape. State-3 `AwaitingAck`
/// retains the existing PR-8 `Some(transaction_id)` shape; state-2
/// `Pending` writes `None` (no prior `InvoiceSubmissionResponse`
/// exists when an operator marks an Attempt-only invoice abandoned).
/// Pre-PR-19 entries round-trip as `Some` per serde_json's default
/// `Option<T>` deserialisation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceMarkedAbandonedPayload {
    pub invoice_id: String,
    pub idempotency_key: String,
    /// The NAV `transaction_id` recorded by the most-recent prior
    /// `InvoiceSubmissionResponse` for this invoice (`Some` for
    /// state-3 `AwaitingAck` abandonments — the existing PR-8 shape)
    /// OR `None` for state-2 `Pending` abandonments (no
    /// `InvoiceSubmissionResponse` exists; the prior Attempt's wire
    /// either broke or the process crashed before TX2 commit per
    /// ADR-0032 §1). There is no further `InvoiceSubmissionResponse`
    /// after this entry — the invoice's audit chain is
    /// terminal-by-operator-decision regardless of stage.
    #[serde(default)]
    pub prior_transaction_id: Option<String>,
    pub prior_last_ack_status: Option<String>,
    pub reason: String,
}

impl InvoiceMarkedAbandonedPayload {
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        prior_transaction_id: Option<String>,
        prior_last_ack_status: Option<String>,
        reason: &str,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            prior_transaction_id,
            prior_last_ack_status,
            reason: reason.to_string(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceSubmissionAttemptFailed  (PR-19 / ADR-0032 §2 — failure half
// of the Attempt/Response audit pair under the two-tx posture per
// ADR-0032 §1. Written in TX2 of `submit-invoice` / `retry-submission`
// / `drain-submission-queue` when the NAV call returns an error
// instead of `InvoiceSubmissionResponse`. Closes F40 at the
// issuing-path level.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceSubmissionAttemptFailed`].
///
/// Carries the failure-class discriminator + the operator-visible
/// error message + the verbatim response body (when one was received
/// before the error fired). The bundle reader (ADR-0009 §8) sees the
/// preceding `InvoiceSubmissionAttempt` (with the request bytes) and
/// this entry (with the failure-class + the response bytes if any)
/// as the paired evidence record for one failed submission attempt.
///
/// # Error class enumeration (ADR-0032 §2)
///
/// The `error_class` field is one of:
///
///   - `"transport"` — TLS / DNS / socket failure (the wire broke;
///     NAV may or may not have processed the submission). The
///     residual that motivates the deferred Layer-2 `queryInvoiceCheck`
///     surface per ADR-0009 §5 + ADR-0032 §"Open questions".
///   - `"http_status"` — non-2xx HTTP response from NAV. `error_code`
///     carries the status as decimal string; `response_xml` carries
///     the body NAV returned (if any).
///   - `"application"` — NAV-side non-retryable application error
///     (`INVALID_SECURITY_USER`, `SCHEMA_VIOLATION`, etc. per
///     ADR-0009 §5). `error_code` carries the NAV `funcCode` /
///     `errorCode` string.
///   - `"retryable_application"` — NAV-side retryable application
///     error (`OPERATION_FAILED`, HTTP 504 per ADR-0009 §5).
///     `error_code` carries the NAV code.
///   - `"envelope"` — envelope construction failure (rare; indicates
///     a programmer error or upstream quick-xml change). No
///     `error_code`; no `response_xml`.
///   - `"credential"` — keychain access failure
///     (`KeychainItemMissing` / `KeychainBackend`). No `error_code`;
///     no `response_xml`.
///   - `"client_build"` — reqwest::Client construction failure. No
///     `error_code`; no `response_xml`.
///
/// The classification is deterministic (CLAUDE.md rule 5) and lives
/// next to the existing `submission_queue::is_transport_error` helper
/// in `submission_queue::classify_attempt_failure`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceSubmissionAttemptFailedPayload {
    /// Prefixed `inv_<ULID>` form — same shape as every other
    /// invoice-bearing payload.
    pub invoice_id: String,
    /// F8 idempotency key carry-forward — same canonical form as
    /// every other NAV-related entry for this invoice.
    pub idempotency_key: String,
    /// `"test"` or `"production"` — same shape as
    /// [`InvoiceSubmissionAttemptPayload`]'s `endpoint` field. The
    /// audit-evidence bundle (ADR-0009 §8) needs the environment
    /// explicit for inspector triage.
    pub endpoint: String,
    /// Failure-class discriminator per the enumeration above. Read
    /// by the bundle reader for diagnosis; not used for routing
    /// (the bundle filter is by EventKind alone per ADR-0009 §8).
    pub error_class: String,
    /// NAV error code (for `application` / `retryable_application`)
    /// or HTTP status as decimal string (for `http_status`) or
    /// `None` (for `transport` / `envelope` / `credential` /
    /// `client_build` — no NAV-side code exists at those layers).
    pub error_code: Option<String>,
    /// Operator-visible error message — the
    /// `NavTransportError::Display` rendering of the failure.
    /// Never includes secret material per the
    /// `NavTransportError::Display` implementation discipline
    /// (ADR-0020 §3).
    pub error_message: String,
    /// Verbatim response bytes IF a response body was received
    /// before the error fired (typical for `http_status` /
    /// `application` / `retryable_application` classes — NAV's
    /// error response body carries the `<funcCode>` + `<errorCode>`
    /// + `<message>` triple the bundle reader uses for diagnosis).
    /// `None` for `transport` / `envelope` / `credential` /
    /// `client_build` classes where no response body exists.
    pub response_xml: Option<Vec<u8>>,
}

impl InvoiceSubmissionAttemptFailedPayload {
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        endpoint: &'static str,
        error_class: &'static str,
        error_code: Option<String>,
        error_message: String,
        response_xml: Option<Vec<u8>>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            endpoint: endpoint.to_string(),
            error_class: error_class.to_string(),
            error_code,
            error_message,
            response_xml,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceCheckPerformed  (PR-20 / ADR-0033 §2 — Layer-2
// `queryInvoiceCheck` evidence per ADR-0009 §5's named-deferred
// disambiguation surface. Written by `retry-submission`'s state-2
// Pending branch BEFORE the manageInvoice re-POST so the retry can
// skip the re-POST when NAV already has the invoice. Closes F44 at
// the state-2 disambiguation level.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceCheckPerformed`].
///
/// Carries the verbatim `<QueryInvoiceCheckRequest>` bytes (always),
/// the verbatim NAV response bytes (Option — `Some` whenever NAV
/// returned a body, including error bodies), the typed outcome
/// discriminator, and three optional failure-class fields populated
/// iff `outcome == "failure"`.
///
/// # Outcome enumeration (ADR-0033 §2)
///
/// The `outcome` field is one of:
///
///   - `"exists"`  — NAV returned `<invoiceCheckResult>true</>`.
///                   The retry SKIPPED the manageInvoice re-POST
///                   per ADR-0033 §1. No duplicate-submission risk.
///                   The post-positive-check NAV-side state
///                   recovery (fetching the chain via
///                   `queryInvoiceData` per ADR-0009 §5) is named-
///                   deferred as F48; the operator-visible summary
///                   names the gap loud per CLAUDE.md rule 12.
///   - `"absent"`  — NAV returned `<invoiceCheckResult>false</>`.
///                   The retry PROCEEDED to the manageInvoice
///                   re-POST per ADR-0033 §1. The subsequent
///                   `InvoiceSubmissionAttempt` +
///                   `InvoiceSubmissionResponse` (or
///                   `InvoiceSubmissionAttemptFailed`) entries
///                   record the re-POST's outcome.
///   - `"failure"` — `queryInvoiceCheck` failed at any layer
///                   (transport / http_status / response_parse /
///                   application). The retry ABORTED per ADR-0033 §
///                   "Surfaced conflict 1 Reading A"; the operator
///                   re-runs `retry-submission` later. The
///                   `failure_class` / `failure_code` /
///                   `failure_message` fields are populated; the
///                   `response_xml` field is `Some` if a NAV body
///                   was received before the failure fired, `None`
///                   otherwise.
///
/// # Failure-class enumeration (ADR-0033 §2 + §5)
///
/// When `outcome == "failure"`, the `failure_class` field is one of
/// the same seven classes
/// [`InvoiceSubmissionAttemptFailedPayload::error_class`] enumerates
/// (`"transport"` / `"http_status"` / `"application"` /
/// `"retryable_application"` / `"envelope"` / `"credential"` /
/// `"client_build"`). The deterministic classifier lives in
/// `submission_queue::classify_attempt_failure` (extended in PR-20
/// to cover the five new `NavTransportError::QueryInvoiceCheck*`
/// variants).
///
/// # Audit-query / classifier UNCHANGED
///
/// Per ADR-0033 §6, the precondition walker
/// `audit_query::stuck_precondition` does NOT consult this entry.
/// An invoice with `Attempt` + `InvoiceCheckPerformed(outcome=exists)`
/// + no `Response` is still classified as state-2 Pending. The
/// state-2 → not-stuck transition is the F48-deferred recover-from-
/// nav surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceCheckPerformedPayload {
    /// Prefixed `inv_<ULID>` form — same shape as every other
    /// invoice-bearing payload.
    pub invoice_id: String,
    /// F8 idempotency key carry-forward — same canonical form as
    /// every other NAV-related entry for this invoice.
    pub idempotency_key: String,
    /// `"test"` or `"production"` — same shape as
    /// [`InvoiceSubmissionAttemptPayload`]'s `endpoint` field.
    /// The audit-evidence bundle (ADR-0009 §8) needs the
    /// environment explicit for inspector triage.
    pub endpoint: String,
    /// The NAV-facing invoice number string that was queried
    /// (e.g., `"INV-default/00042"`). The bundle reader sees the
    /// exact identifier that hit NAV's queryInvoiceCheck endpoint
    /// without re-deriving from series.code + seq.
    pub nav_invoice_number: String,
    /// Outcome discriminator per the §2 enumeration above. Read by
    /// the bundle reader for diagnosis and by the orchestration
    /// for the operator-visible summary; not used by the
    /// precondition walker (which is informational-only per
    /// ADR-0033 §6).
    pub outcome: String,
    /// Verbatim `<QueryInvoiceCheckRequest>` envelope bytes.
    /// Persisted for every outcome — even on `"failure"` the
    /// request bytes show what ABERP attempted.
    pub request_xml: Vec<u8>,
    /// Verbatim NAV response bytes IF a response body was received.
    /// `Some(...)` for `"exists"` and `"absent"` outcomes (NAV
    /// returned an OK body); `Some(...)` for `"failure"` outcomes
    /// where a body was received before the failure fired (e.g.,
    /// `http_status` / `application` / `retryable_application`
    /// classes — NAV's body carries the `<funcCode>` /
    /// `<errorCode>` / `<message>` triple). `None` for `"failure"`
    /// outcomes where no body was received (transport / envelope
    /// / credential / client_build classes).
    pub response_xml: Option<Vec<u8>>,
    /// Failure-class discriminator per the §2 enumeration above.
    /// `Some(...)` iff `outcome == "failure"`; `None` otherwise.
    /// Same seven-class enumeration as
    /// [`InvoiceSubmissionAttemptFailedPayload::error_class`].
    pub failure_class: Option<String>,
    /// `Some(...)` for `failure_class == "application"` (NAV code)
    /// or `"retryable_application"` (NAV code) or `"http_status"`
    /// (HTTP status as decimal string); `None` otherwise.
    pub failure_code: Option<String>,
    /// Operator-visible error message — the
    /// `NavTransportError::Display` rendering of the failure.
    /// `Some(...)` iff `outcome == "failure"`. Never includes
    /// secret material per ADR-0020 §3.
    pub failure_message: Option<String>,
}

impl InvoiceCheckPerformedPayload {
    /// Construct a payload for an `Exists` or `Absent` outcome.
    /// The orchestration's OK happy path (NAV answered cleanly)
    /// lands here. `response_xml` is always `Some` because NAV
    /// returned an OK body.
    pub fn new_for_outcome(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        endpoint: &'static str,
        nav_invoice_number: &str,
        outcome: &'static str,
        request_xml: Vec<u8>,
        response_xml: Vec<u8>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            endpoint: endpoint.to_string(),
            nav_invoice_number: nav_invoice_number.to_string(),
            outcome: outcome.to_string(),
            request_xml,
            response_xml: Some(response_xml),
            failure_class: None,
            failure_code: None,
            failure_message: None,
        }
    }

    /// Construct a payload for a `"failure"` outcome.
    /// `response_xml` is `Option` because some failure classes
    /// (transport / envelope / credential / client_build) have no
    /// NAV body. The `failure_class` / `failure_code` /
    /// `failure_message` fields are populated per the
    /// `submission_queue::classify_attempt_failure` classifier
    /// output.
    #[allow(clippy::too_many_arguments)]
    pub fn new_for_failure(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        endpoint: &'static str,
        nav_invoice_number: &str,
        request_xml: Vec<u8>,
        response_xml: Option<Vec<u8>>,
        failure_class: &'static str,
        failure_code: Option<String>,
        failure_message: String,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            endpoint: endpoint.to_string(),
            nav_invoice_number: nav_invoice_number.to_string(),
            outcome: "failure".to_string(),
            request_xml,
            response_xml,
            failure_class: Some(failure_class.to_string()),
            failure_code,
            failure_message: Some(failure_message),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceStornoIssued  (PR-10 / ADR-0023 — storno chain-link entry.
// A storno is itself an invoice and burns its own sequence number via
// the standard allocator path (which writes its own
// `InvoiceSequenceReservedPayload` + `InvoiceDraftCreatedPayload`
// pair). THIS payload is the chain-link — it carries both the storno's
// identity (so an audit reader can pivot from the chain entry to the
// storno's own ledger entries via `idempotency_key`) and the base
// invoice's identity (so the per-invoice export bundle can walk the
// chain by following `base_invoice_id`). ADR-0023 §3.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceStornoIssued`].
///
/// Pinned by ADR-0023 §3. Written by `aberp issue-storno` in the same
/// DuckDB transaction as the storno's own allocator + audit-ledger
/// entries.
///
/// `base_sequence_number` is denormalized from the base invoice's row
/// by design (ADR-0023 §3 + Adversarial review #2). Drift is guarded
/// by the integrity-scan extension named in ADR-0023 §4: the base
/// row's `sequence_number` is immutable after issuance, so a mismatch
/// against this payload's copy indicates direct DB tampering — exactly
/// what the audit ledger's hash chain (ADR-0008) is designed to make
/// visible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceStornoIssuedPayload {
    /// The storno's own invoice id — prefixed `inv_<ULID>` form.
    pub storno_invoice_id: String,
    /// The storno's own sequence number (allocated in the same
    /// DuckDB transaction per ADR-0009 §3).
    pub storno_seq: u64,
    /// The storno's own sequence-reservation id (ULID-keyed,
    /// matches `InvoiceSequenceReservedPayload::reservation_id`).
    pub storno_reservation_id: String,
    /// Idempotency key of the `IssueStornoCommand`. Same shape + role
    /// as on `InvoiceSequenceReservedPayload`; threads through F8.
    pub idempotency_key: String,
    /// The **base invoice's** id — prefixed `inv_<ULID>` form. This is
    /// the chain link: ULID-keyed per ADR-0019 (no cross-table FK),
    /// explicit per ADR-0009 §6.
    pub base_invoice_id: String,
    /// The **base invoice's** NAV-facing sequence number, captured
    /// verbatim so the per-invoice export bundle (ADR-0009 §8) can
    /// reconstruct the `<invoiceReference>` value without re-querying
    /// the base row. Denormalized by design — see the type-level doc
    /// comment above.
    pub base_sequence_number: u64,
    /// The `<modificationIndex>` this storno asserts against the base
    /// invoice's chain. Starts at 1 for the first chain entry against
    /// the base, increments for each subsequent storno or future
    /// modification. Allocator rules per ADR-0023 §4.
    pub modification_index: u32,
}

impl InvoiceStornoIssuedPayload {
    /// Build a payload from the parts the allocator just produced.
    /// `new()` rather than `from_outcome(...)` because the chain-link
    /// fields cross multiple domain types (base + storno + chain
    /// index) — no single domain struct carries them all today, and a
    /// speculative `StornoIssuanceOutcome` type would be a CLAUDE.md
    /// rule-2 violation.
    pub fn new(
        storno_invoice_id: &str,
        storno_seq: u64,
        storno_reservation_id: &str,
        idempotency_key: IdempotencyKey,
        base_invoice_id: &str,
        base_sequence_number: u64,
        modification_index: u32,
    ) -> Self {
        Self {
            storno_invoice_id: storno_invoice_id.to_string(),
            storno_seq,
            storno_reservation_id: storno_reservation_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            base_invoice_id: base_invoice_id.to_string(),
            base_sequence_number,
            modification_index,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceModificationIssued  (PR-11 / ADR-0024 — MODIFY chain-link
// entry, the structural parallel to `InvoiceStornoIssuedPayload`. A
// modification is itself an invoice and burns its own sequence number
// via the standard allocator path (which writes its own
// `InvoiceSequenceReservedPayload` + `InvoiceDraftCreatedPayload`
// pair). THIS payload is the chain-link — same fields as the storno
// chain-link plus `modification_issue_date` which NAV requires for
// MODIFY but not for STORNO (ADR-0024 §3, §5).)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceModificationIssued`].
///
/// Pinned by ADR-0024 §5. Written by `aberp issue-modification` in the
/// same DuckDB transaction as the modification's own allocator +
/// audit-ledger entries.
///
/// `base_sequence_number` is denormalized from the base invoice's row
/// by the same posture as
/// [`InvoiceStornoIssuedPayload::base_sequence_number`] — drift
/// guarded by ADR-0023 §4's integrity-scan extension (which carries
/// forward unchanged to MODIFY).
///
/// `modification_issue_date` is the operator-supplied date the
/// modification was issued, stored as `String` in canonical
/// `YYYY-MM-DD` form (rationale per ADR-0024 §5 + "Alternatives
/// considered" — typed-time wrapper would force serde-with adapters
/// for a value the operator already supplies in canonical form).
/// Validation that the string is well-formed happens at the CLI
/// boundary (`apps/aberp/src/issue_modification.rs` step 2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceModificationIssuedPayload {
    /// The modification's own invoice id — prefixed `inv_<ULID>` form.
    pub modification_invoice_id: String,
    /// The modification's own sequence number (allocated in the same
    /// DuckDB transaction per ADR-0009 §3).
    pub modification_seq: u64,
    /// The modification's own sequence-reservation id (ULID-keyed,
    /// matches `InvoiceSequenceReservedPayload::reservation_id`).
    pub modification_reservation_id: String,
    /// Idempotency key of the `IssueModificationCommand`. Same shape +
    /// role as on `InvoiceStornoIssuedPayload`.
    pub idempotency_key: String,
    /// The **base invoice's** id — prefixed `inv_<ULID>` form. Chain
    /// link (ULID-keyed per ADR-0019, explicit per ADR-0009 §6).
    pub base_invoice_id: String,
    /// The **base invoice's** NAV-facing sequence number. Denormalized
    /// by design (see type-level doc comment above).
    pub base_sequence_number: u64,
    /// The `<modificationIndex>` this modification asserts against the
    /// base invoice's chain. Allocator rules per ADR-0024 §7 — walks
    /// both `InvoiceStornoIssued` AND `InvoiceModificationIssued`
    /// entries against the same base.
    pub modification_index: u32,
    /// The operator-supplied `<modificationIssueDate>` in `YYYY-MM-DD`
    /// form. NAV-required for MODIFY (distinguishes the wire
    /// operation from STORNO per ADR-0024 §3); absent on STORNO so
    /// the structural parallel breaks here intentionally.
    pub modification_issue_date: String,
}

impl InvoiceModificationIssuedPayload {
    /// Build a payload from the parts the allocator just produced.
    /// `new()` rather than `from_outcome(...)` for the same reason
    /// [`InvoiceStornoIssuedPayload::new`] uses it: the chain-link
    /// fields cross multiple domain types (base + modification +
    /// chain index + operator date) — no single domain struct carries
    /// them all today, and a speculative `ModificationIssuanceOutcome`
    /// type would be a CLAUDE.md rule-2 violation.
    pub fn new(
        modification_invoice_id: &str,
        modification_seq: u64,
        modification_reservation_id: &str,
        idempotency_key: IdempotencyKey,
        base_invoice_id: &str,
        base_sequence_number: u64,
        modification_index: u32,
        modification_issue_date: &str,
    ) -> Self {
        Self {
            modification_invoice_id: modification_invoice_id.to_string(),
            modification_seq,
            modification_reservation_id: modification_reservation_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            base_invoice_id: base_invoice_id.to_string(),
            base_sequence_number,
            modification_index,
            modification_issue_date: modification_issue_date.to_string(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceTechnicalAnnulmentRequested  (PR-12 / ADR-0025 — operator-
// decision audit entry for a NAV-side technical annulment of a prior
// data submission. Structurally distinct from STORNO + MODIFY: NOT a
// chain entry, NO sequence-slot burn, NO derived typestate transition
// on the base. The annulment's audit footprint is THIS payload alone;
// the future submit-annulment PR will write `InvoiceSubmissionAttempt`
// + `InvoiceSubmissionResponse` against the manageAnnulment wire call.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceTechnicalAnnulmentRequested`].
///
/// Pinned by ADR-0025 §3. Written by `aberp request-technical-annulment`
/// in a single DuckDB transaction. No companion entries
/// (`InvoiceSequenceReserved` / `InvoiceDraftCreated` are NOT written —
/// the annulment is not an invoice and does not burn a sequence slot,
/// see ADR-0025 §1).
///
/// `prior_transaction_id` is captured from the most-recent prior
/// `InvoiceSubmissionResponse` against the base — denormalized so the
/// audit-evidence bundle (ADR-0009 §8) makes the annulment-target
/// submission unambiguously identifiable without a second walk. Same
/// posture as `InvoiceRetryRequestedPayload::prior_transaction_id` /
/// `InvoiceMarkedAbandonedPayload::prior_transaction_id`.
///
/// `annulment_code` carries the canonical NAV wire-form string
/// (`ERRATIC_DATA` / `ERRATIC_INVOICE_NUMBER` /
/// `ERRATIC_INVOICE_ISSUE_DATE` / `ERRATIC_ELECTRONIC_HASH_VALUE`).
/// The CLI's clap `ValueEnum` lowercased-hyphen form is converted to
/// the canonical wire form before this payload is built (ADR-0025 §3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceTechnicalAnnulmentRequestedPayload {
    /// The **base invoice's** id — prefixed `inv_<ULID>` form. The
    /// annulment is FOR this invoice (not a new invoice produced by
    /// the annulment), so the payload's `invoice_id` field IS the
    /// base id directly. Key contrast with the storno/modify chain-
    /// link payloads, which carry both `*_invoice_id` and
    /// `base_invoice_id`.
    pub invoice_id: String,
    /// Idempotency key of the `RequestTechnicalAnnulmentCommand`.
    /// Operator-decision idempotency, distinct from the base
    /// invoice's issuance idempotency key. Same shape + role as
    /// `InvoiceMarkedAbandonedPayload::idempotency_key`.
    pub idempotency_key: String,
    /// The base invoice's NAV `transactionId` (from the most-recent
    /// prior `InvoiceSubmissionResponse` entry against the base).
    /// Captured here so the audit-evidence bundle (ADR-0009 §8)
    /// makes the annulment-target submission unambiguously
    /// identifiable without a second walk back to the response entry.
    pub prior_transaction_id: String,
    /// One of the four NAV annulment codes in **canonical wire form**:
    /// `ERRATIC_DATA`, `ERRATIC_INVOICE_NUMBER`,
    /// `ERRATIC_INVOICE_ISSUE_DATE`, `ERRATIC_ELECTRONIC_HASH_VALUE`.
    /// Stored as `String` (not a typed enum) per ADR-0025 §
    /// "Alternatives considered" — the audit payload's serialization
    /// shape is the canonical record; a typed-enum wrapper would
    /// force serde-with adapters for a value that is canonical on
    /// the wire. The CLI's clap-ValueEnum is the loud-fail boundary
    /// (rejects unknown codes at parse time).
    pub annulment_code: String,
    /// Free-form operator-supplied reason text. Same posture as
    /// `InvoiceRetryRequestedPayload::reason` /
    /// `InvoiceMarkedAbandonedPayload::reason` — required at the CLI
    /// boundary so the audit-evidence bundle (ADR-0009 §8) always
    /// carries a human-readable justification for the annulment
    /// decision.
    pub reason: String,
}

impl InvoiceTechnicalAnnulmentRequestedPayload {
    /// Build a payload from the parts the
    /// `request-technical-annulment` orchestrator just resolved.
    /// `new()` (not `from_outcome(...)`) because the payload's
    /// fields cross the operator decision (code + reason) AND the
    /// audit chain (invoice id + prior transaction id + idempotency
    /// key); no single domain struct carries them all, and a
    /// speculative `AnnulmentRequestOutcome` type would be a
    /// CLAUDE.md rule-2 violation.
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        prior_transaction_id: &str,
        annulment_code: &str,
        reason: &str,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            prior_transaction_id: prior_transaction_id.to_string(),
            annulment_code: annulment_code.to_string(),
            reason: reason.to_string(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceAnnulmentSubmissionAttempt  (PR-13 / ADR-0026 §2 — wire half
// of the technical-annulment surface. Structural parallel to
// `InvoiceSubmissionAttemptPayload` with the same field shape, but
// deliberately forked as a distinct type so the type system enforces
// the kind ⇄ payload binding even when the EventKind discriminator
// is correct. Same posture as `InvoiceStornoIssuedPayload` vs
// `InvoiceModificationIssuedPayload` — structurally similar, forked
// deliberately so a future audit-evidence-bundle reader cannot
// silently deserialize one as the other.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceAnnulmentSubmissionAttempt`].
///
/// Written by the binary's `submit_annulment` flow just BEFORE the
/// `manageAnnulment` POST returns — same capture-before-response
/// posture as `InvoiceSubmissionAttemptPayload` per ADR-0009 §8.
///
/// `request_xml` is the verbatim bytes of the
/// `<ManageAnnulmentRequest>` envelope (NOT the inner
/// `<InvoiceAnnulment>` body; that lives on disk at the path the
/// operator passed to `--annulment-xml`). The typed-struct path
/// through `serde_json::to_vec` handles all JSON escaping per F9.
///
/// `idempotency_key` is the **annulment-request's** key (looked up
/// from the prior `InvoiceTechnicalAnnulmentRequested` audit entry
/// per ADR-0026 §6 + §7), NOT the base invoice's issuance key.
/// Rationale: the annulment is a distinct operator decision per
/// ADR-0025 §3 — the audit-evidence bundle reader walks back from
/// this wire entry to the request entry via shared idempotency key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceAnnulmentSubmissionAttemptPayload {
    /// The **base invoice's** id — prefixed `inv_<ULID>` form. The
    /// annulment is FOR this invoice; same field semantics as
    /// `InvoiceTechnicalAnnulmentRequestedPayload::invoice_id`.
    pub invoice_id: String,
    /// Idempotency key of the prior
    /// `InvoiceTechnicalAnnulmentRequested` (the operator-decision
    /// key minted by `request-technical-annulment`). Flows through
    /// per ADR-0026 §"F8 contract" so a re-submission against the
    /// same on-disk annulment XML carries the same key.
    pub idempotency_key: String,
    /// `"test"` or `"production"` — which NAV environment the
    /// annulment was POSTed against. Same loud-fail surface as
    /// `InvoiceSubmissionAttemptPayload::endpoint` (a production
    /// annulment attempted against `api-test` is an operator-error
    /// class that should be visible in the ledger without consulting
    /// the URL).
    pub endpoint: String,
    /// Verbatim `<ManageAnnulmentRequest>` bytes (UTF-8). Same
    /// serde_json base64-encoding behaviour for `Vec<u8>` as
    /// `InvoiceSubmissionAttemptPayload::request_xml`, so the
    /// round-trip preserves embedded quotes / backslashes /
    /// non-ASCII bytes inside the operator's reason text.
    pub request_xml: Vec<u8>,
}

impl InvoiceAnnulmentSubmissionAttemptPayload {
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        endpoint: &'static str,
        request_xml: Vec<u8>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            endpoint: endpoint.to_string(),
            request_xml,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceAnnulmentSubmissionResponse  (PR-13 / ADR-0026 §2 — wire-
// response half. Same fork-from-`InvoiceSubmissionResponsePayload`
// rationale as the attempt above.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceAnnulmentSubmissionResponse`].
///
/// Written immediately after a successful `manageAnnulment`
/// response is received. Carries the verbatim
/// `<ManageAnnulmentResponse>` bytes per ADR-0009 §8 plus the
/// parsed `transaction_id` (NAV's annulment-side tracking token —
/// the future `query-annulment-status` poll will key on this id).
///
/// `transaction_id` is NAV-assigned. ABERP treats it as opaque; no
/// shape parsing. Same posture as
/// `InvoiceSubmissionResponsePayload::transaction_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceAnnulmentSubmissionResponsePayload {
    /// The **base invoice's** id — prefixed `inv_<ULID>` form. Same
    /// field semantics as the attempt payload's `invoice_id`.
    pub invoice_id: String,
    /// Annulment-request's idempotency key. Same per ADR-0026 §6 +
    /// §7 + §"F8 contract".
    pub idempotency_key: String,
    /// NAV-assigned transaction id from the `manageAnnulment`
    /// response. Opaque to ABERP; passed verbatim to a future
    /// `query-annulment-status` call.
    pub transaction_id: String,
    /// Verbatim `<ManageAnnulmentResponse>` bytes.
    pub response_xml: Vec<u8>,
}

impl InvoiceAnnulmentSubmissionResponsePayload {
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        transaction_id: &str,
        response_xml: Vec<u8>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            transaction_id: transaction_id.to_string(),
            response_xml,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceAnnulmentAckStatus  (PR-14 / ADR-0027 §2 — wire-poll half
// of the technical-annulment surface. Structural parallel to
// `InvoiceAckStatusPayload` with the same field shape, but
// deliberately forked as a distinct type so the type system
// enforces the kind ⇄ payload binding even when the EventKind
// discriminator is correct. Same posture as
// `InvoiceAnnulmentSubmissionAttemptPayload` vs
// `InvoiceSubmissionAttemptPayload` per ADR-0026 §2.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceAnnulmentAckStatus`].
///
/// Written by the binary's `poll_annulment_ack` flow after each
/// `queryTransactionStatus` call against the annulment-side
/// `transactionId` (looked up from the prior
/// `InvoiceAnnulmentSubmissionResponse` per ADR-0027 §4). One entry
/// per poll attempt — same per-poll-commit posture as
/// `InvoiceAckStatusPayload` per ADR-0009 §8 ("every response across
/// the chain" intent).
///
/// `transaction_id` is NAV's **annulment-side** tracking id — the
/// one returned by the prior `manageAnnulment` response, NOT the
/// base invoice's submission `transactionId`. Stored verbatim;
/// opaque to ABERP.
///
/// `ack_status` is the parsed NAV `<invoiceStatus>` enumeration
/// (`"RECEIVED"` | `"PROCESSING"` | `"SAVED"` | `"ABORTED"`) per
/// NAV v3.0. Reused unchanged from
/// `InvoiceAckStatusPayload::ack_status` per ADR-0027 §3 (the wire
/// endpoint is shared; the audit-ledger discriminator forks at the
/// kind level, not at the enumeration). On terminal `SAVED` the
/// operator-visible message names the receiver-confirmation gap
/// loud per ADR-0027 §5 — NAV's SAVED for an annulment submission
/// means "NAV accepted the annulment for processing," NOT "the
/// receiver has confirmed."
///
/// `response_xml` is the verbatim
/// `<QueryTransactionStatusResponse>` bytes per ADR-0009 §8 (the
/// audit evidence cannot be lost to a parser bug).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceAnnulmentAckStatusPayload {
    /// The **base invoice's** id — prefixed `inv_<ULID>` form. Same
    /// field semantics as
    /// `InvoiceAnnulmentSubmissionResponsePayload::invoice_id`.
    pub invoice_id: String,
    /// NAV-assigned annulment-side transaction id (from the prior
    /// `InvoiceAnnulmentSubmissionResponse`). Opaque to ABERP;
    /// passed verbatim to `queryTransactionStatus`.
    pub transaction_id: String,
    /// `"RECEIVED"` | `"PROCESSING"` | `"SAVED"` | `"ABORTED"` per
    /// NAV v3.0. Recorded verbatim. Same enumeration as
    /// `InvoiceAckStatusPayload::ack_status` per ADR-0027 §3.
    pub ack_status: String,
    /// Verbatim `<QueryTransactionStatusResponse>` bytes.
    pub response_xml: Vec<u8>,
}

impl InvoiceAnnulmentAckStatusPayload {
    pub fn new(
        invoice_id: &str,
        transaction_id: &str,
        ack_status: &str,
        response_xml: Vec<u8>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            transaction_id: transaction_id.to_string(),
            ack_status: ack_status.to_string(),
            response_xml,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// InvoiceAnnulmentReceiverConfirmation  (PR-15 / ADR-0028 §2 —
// receiver-confirmation observation half of the technical-annulment
// surface. Pairs with the PR-14 wire-poll entries; closes the final
// ADR-0009 §6 observation gap at the audit-evidence level.
//
// Structurally distinct from `InvoiceAnnulmentAckStatusPayload`:
// extends the audit-evidence shape with two additional fields
// (`nav_invoice_number` because `queryInvoiceData` keys on the
// invoice number not the transaction id; `annulment_transaction_id`
// to anchor the back-walk to the annulment lineage), and OMITS the
// `ack_status` field (no parsed enumeration per ADR-0028 §"Surfaced
// conflict 3" — verbatim-bytes-only posture until NAV-testbed
// verification surfaces the actual response shape).
//
// Forked as a distinct type per the same kind ⇄ payload binding
// posture every prior annulment-side payload uses (ADR-0026 §2 /
// ADR-0027 §2): the type system enforces the discriminator even
// when JSON shape happens to be compatible.)
// ──────────────────────────────────────────────────────────────────────

/// Payload for [`aberp_audit_ledger::EventKind::InvoiceAnnulmentReceiverConfirmation`].
///
/// Written by the binary's `observe_receiver_confirmation` flow
/// after each `queryInvoiceData` call against the BASE invoice's
/// NAV-facing invoice number (ADR-0028 §1 + §3). One entry per
/// call — same per-call-commit posture as
/// `InvoiceAnnulmentAckStatusPayload` per ADR-0009 §8 ("every
/// response across the chain" intent), BUT the call shape itself
/// is one-shot (not a bounded poll loop) per ADR-0028 §4 + §
/// "Surfaced conflict 2": the receiver-confirmation is human-paced
/// so a fixed-cadence loop is structurally wrong; the operator
/// re-runs the command at their cadence.
///
/// `nav_invoice_number` is the string ABERP asked NAV about
/// (e.g., `"INV-default/00042"`). Stored verbatim so the audit-
/// evidence bundle reader can see what was queried without
/// re-deriving from `series.code + sequence_number`.
///
/// `annulment_transaction_id` is the NAV-assigned **annulment-side**
/// tracking id from the prior `InvoiceAnnulmentSubmissionResponse`
/// (NOT the base invoice's submission transactionId). Pinned here
/// so the bundle reader anchors the receiver-confirmation entry to
/// the annulment lineage by ID without re-walking the ledger; also
/// surfaces the "this observation is about THIS annulment, not the
/// original CREATE submission" intent at field-level granularity.
///
/// `idempotency_key` is the **annulment-request's** key (F8 carry-
/// forward per ADR-0028 §7 — same posture as the PR-14 ack-status
/// entries per ADR-0027 §6; closes the per-annulment audit lineage
/// end-to-end: every entry from request through receiver-
/// confirmation shares one key).
///
/// `response_xml` is the verbatim `<QueryInvoiceDataResponse>`
/// bytes per ADR-0009 §8 (the audit evidence cannot be lost to a
/// parser bug). Per ADR-0028 §"Surfaced conflict 3" the verbatim-
/// bytes-only posture applies until NAV-testbed verification
/// surfaces the actual receiver-confirmation response field; a
/// future amendment ADR adds a parsed `receiver_state` enum field
/// additively (the existing `response_xml` field carries the
/// historic evidence regardless).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceAnnulmentReceiverConfirmationPayload {
    /// The **base invoice's** id — prefixed `inv_<ULID>` form. The
    /// observation is about the receiver-side state of the
    /// annulment of THIS invoice; same field semantics as
    /// `InvoiceAnnulmentAckStatusPayload::invoice_id`.
    pub invoice_id: String,
    /// The base invoice's NAV-facing invoice number string (e.g.,
    /// `"INV-default/00042"`). This is what was passed to NAV's
    /// `queryInvoiceData` operation per ADR-0028 §3. Stored
    /// verbatim so the audit-evidence bundle reader sees the
    /// query input without re-deriving from billing-store state
    /// (which may have evolved between the call and the read).
    pub nav_invoice_number: String,
    /// NAV-assigned annulment-side transaction id (from the prior
    /// `InvoiceAnnulmentSubmissionResponse` entry). Pinned here
    /// at field-level so the bundle reader anchors to the
    /// annulment lineage by ID; also distinguishes "this
    /// observation is about the annulment submission" from "this
    /// observation is about the original CREATE submission" at
    /// the payload level without inspecting the kind discriminator
    /// alone.
    pub annulment_transaction_id: String,
    /// Annulment-request's idempotency key (the operator-decision
    /// key minted by `request-technical-annulment` and carried
    /// forward through every annulment-lineage entry per the F8
    /// contract — ADR-0026 §F8 + ADR-0027 §6 + ADR-0028 §7).
    /// Same shape + role as on every other annulment-lineage
    /// payload.
    pub idempotency_key: String,
    /// Verbatim `<QueryInvoiceDataResponse>` bytes. No parsed
    /// receiver-confirmation state field per ADR-0028 §"Surfaced
    /// conflict 3" — the verbatim bytes are the audit-evidence
    /// today; the parsed-field extension lands additively in a
    /// future amendment ADR after NAV-testbed verification.
    pub response_xml: Vec<u8>,
}

impl InvoiceAnnulmentReceiverConfirmationPayload {
    /// Build a payload from the parts the
    /// `observe-receiver-confirmation` orchestrator just resolved.
    /// `new()` (not `from_outcome(...)`) for the same reason every
    /// prior annulment-side payload uses it: the fields cross the
    /// audit chain (invoice id + annulment-side transaction id +
    /// idempotency key) AND the query input (nav_invoice_number),
    /// AND the NAV response (response_xml); no single domain
    /// struct carries them all, and a speculative
    /// `ReceiverConfirmationOutcome` type would be a CLAUDE.md
    /// rule-2 violation.
    pub fn new(
        invoice_id: &str,
        nav_invoice_number: &str,
        annulment_transaction_id: &str,
        idempotency_key: IdempotencyKey,
        response_xml: Vec<u8>,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            nav_invoice_number: nav_invoice_number.to_string(),
            annulment_transaction_id: annulment_transaction_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            response_xml,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of audit payload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests — round-trip every payload through serde_json
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_billing::{
        CustomerId, Huf, InvoiceId, LineItem, ReservationId, ReservationStatus, SeriesId,
    };
    use time::OffsetDateTime;

    /// Build a ReadyInvoice fixture whose description contains the
    /// exact JSON-hostile characters that PR-5's `format!` approach
    /// could not safely interpolate. If this test round-trips
    /// cleanly, the typed-struct path is doing the escaping the old
    /// path did not.
    fn fixture_invoice() -> ReadyInvoice {
        ReadyInvoice {
            id: InvoiceId::new(),
            series_id: SeriesId::new(),
            customer_id: CustomerId::new(),
            lines: vec![
                LineItem {
                    description: "line with \"quotes\" and \\ backslashes \n\t newlines"
                        .to_string(),
                    quantity: 2,
                    unit_price: Huf(1_500),
                    vat_rate_basis_points: 2700,
                },
                LineItem {
                    description: "ünïcödé and other non-ASCII: 日本語".to_string(),
                    quantity: 1,
                    unit_price: Huf(500),
                    vat_rate_basis_points: 2700,
                },
            ],
            issue_date: OffsetDateTime::now_utc(),
            sequence_number: 7,
            fiscal_year: 0,
        }
    }

    fn fixture_reservation(invoice_id: InvoiceId, series_id: SeriesId) -> SequenceReservation {
        SequenceReservation {
            id: ReservationId::new(),
            series_id,
            fiscal_year: 0,
            number: 7,
            invoice_id,
            status: ReservationStatus::Reserved,
            void_reason: None,
            reserved_at: OffsetDateTime::now_utc(),
            used_at: None,
            voided_at: None,
        }
    }

    #[test]
    fn sequence_reserved_round_trip() {
        let invoice = fixture_invoice();
        let reservation = fixture_reservation(invoice.id, invoice.series_id);
        let idem = IdempotencyKey::new();
        let original = InvoiceSequenceReservedPayload::from_outcome(&invoice, &reservation, idem);
        let bytes = original.to_bytes();

        // Bytes must parse back to an identical struct. If serde drops
        // a field on encode or decode, this fails loudly.
        let decoded: InvoiceSequenceReservedPayload =
            serde_json::from_slice(&bytes).expect("decode must succeed");
        assert_eq!(decoded, original);

        // The idempotency_key field must carry the ADR-0005 prefix —
        // the F8 contract is reinforced from the audit-payload side.
        assert!(decoded.idempotency_key.starts_with("idem_"));
    }

    #[test]
    fn draft_created_round_trip() {
        let invoice = fixture_invoice();
        let idem = IdempotencyKey::new();
        let original = InvoiceDraftCreatedPayload::from_invoice(&invoice, idem);
        let bytes = original.to_bytes();

        let decoded: InvoiceDraftCreatedPayload =
            serde_json::from_slice(&bytes).expect("decode must succeed");
        assert_eq!(decoded, original);

        // The line_count must match the fixture's line count exactly.
        assert_eq!(decoded.line_count, 2);
        // PR-18 / ADR-0031 §2: the pre-PR-18 constructor leaves
        // nav_xml_path: None. The drain worker treats None as the
        // operator-must-supply-override case.
        assert_eq!(decoded.nav_xml_path, None);
    }

    /// PR-18 / ADR-0031 §2 — the with-xml-path constructor populates
    /// `nav_xml_path: Some(...)` and round-trips cleanly. CLAUDE.md
    /// rule 9: this test pins the new constructor's intent (the
    /// drain worker keys on the value of this field), not just its
    /// shape — without it a future regression flattening
    /// `from_invoice_with_xml_path` back into `from_invoice` would
    /// pass the existing `draft_created_round_trip` but break drain.
    #[test]
    fn draft_created_with_xml_path_round_trip() {
        use std::path::PathBuf;
        let invoice = fixture_invoice();
        let idem = IdempotencyKey::new();
        let path = PathBuf::from("/tmp/out/inv_01J0.xml");
        let original = InvoiceDraftCreatedPayload::from_invoice_with_xml_path(
            &invoice,
            idem,
            path.clone(),
        );
        let bytes = original.to_bytes();

        let decoded: InvoiceDraftCreatedPayload =
            serde_json::from_slice(&bytes).expect("decode must succeed");
        assert_eq!(decoded, original);
        assert_eq!(
            decoded.nav_xml_path.as_deref(),
            Some("/tmp/out/inv_01J0.xml")
        );
    }

    /// PR-18 / ADR-0031 §2 — pre-PR-18 serialized form (no
    /// `nav_xml_path` field at all) MUST deserialise cleanly with
    /// `nav_xml_path: None`. The `#[serde(default)]` attribute is
    /// the load-bearing posture; this test pins it. A future
    /// regression removing the attribute would surface here, not
    /// at runtime on a real pre-PR-18 ledger.
    #[test]
    fn draft_created_deserialises_pre_pr_18_bytes_without_xml_path_field() {
        let pre_pr_18 = br#"{
            "invoice_id": "inv_01J0",
            "line_count": 2,
            "idempotency_key": "idem_01J0"
        }"#;
        let decoded: InvoiceDraftCreatedPayload =
            serde_json::from_slice(pre_pr_18).expect("pre-PR-18 form must deserialise");
        assert_eq!(decoded.invoice_id, "inv_01J0");
        assert_eq!(decoded.line_count, 2);
        assert_eq!(decoded.idempotency_key, "idem_01J0");
        assert_eq!(decoded.nav_xml_path, None);
        // PR-44γ — pre-PR-44γ entries deserialise with the five
        // rate-metadata fields all `None`; the `#[serde(default)]`
        // attribute on each field is the load-bearing posture.
        assert_eq!(decoded.currency, None);
        assert_eq!(decoded.exchange_rate, None);
        assert_eq!(decoded.exchange_rate_source, None);
        assert_eq!(decoded.exchange_rate_date, None);
        assert_eq!(decoded.huf_equivalent_total, None);
    }

    /// PR-44γ / ADR-0037 — the EUR issuance audit payload carries the
    /// five rate-metadata fields and round-trips through serde without
    /// drift. Per CLAUDE.md rule 9 (tests verify intent, not just
    /// behaviour) — each field is asserted by exact value, so a
    /// regression that drops or renames any field surfaces loud.
    ///
    /// The fixture rate is `405.230000` (a realistic MNB EUR/HUF
    /// publication shape — 6 decimal precision per C11). The
    /// rate-publication date is `2026-05-22` (Friday — a normal
    /// publication day in the 2026-05-23 issuance window the brief
    /// names). The HUF-equivalent total is `5_065` (the
    /// `12.50 EUR × 405.230000` worked example from the
    /// `huf_equivalent_uses_banker_rounding_on_ties` differential).
    #[test]
    fn draft_created_round_trip_with_rate_metadata() {
        use rust_decimal::Decimal;
        use std::path::PathBuf;
        use std::str::FromStr;

        let invoice = fixture_invoice();
        let idem = IdempotencyKey::new();
        let rate = aberp_billing::RateMetadata {
            rate: Decimal::from_str("405.230000").expect("rate parses"),
            source: "MNB".to_string(),
            date: time::macros::date!(2026 - 05 - 22),
            huf_equivalent_total: 5_065,
        };
        let path = PathBuf::from("/tmp/out/inv_01J0.xml");
        let original = InvoiceDraftCreatedPayload::from_invoice_with_rate(
            &invoice,
            idem,
            Some(path.clone()),
            aberp_billing::Currency::Eur,
            &rate,
        );
        let bytes = original.to_bytes();

        let decoded: InvoiceDraftCreatedPayload =
            serde_json::from_slice(&bytes).expect("EUR audit payload must round-trip");
        assert_eq!(decoded, original);
        // Field-by-field pin per CLAUDE.md rule 9. A future
        // PartialEq-dropping refactor still surfaces because each field
        // is asserted.
        assert_eq!(decoded.currency.as_deref(), Some("EUR"));
        assert_eq!(decoded.exchange_rate.as_deref(), Some("405.230000"));
        assert_eq!(decoded.exchange_rate_source.as_deref(), Some("MNB"));
        assert_eq!(decoded.exchange_rate_date.as_deref(), Some("2026-05-22"));
        assert_eq!(decoded.huf_equivalent_total.as_deref(), Some("5065"));
        assert_eq!(
            decoded.nav_xml_path.as_deref(),
            Some("/tmp/out/inv_01J0.xml")
        );
    }

    /// PR-44γ / ADR-0037 §3 — the HUF path through
    /// `from_invoice_with_xml_path` stamps `currency: Some("HUF")`
    /// explicitly (NOT `None`, which would alias with pre-PR-44γ
    /// entries). The five rate-metadata fields are `None` for HUF
    /// per the C10 byte-identical invariant prerequisite — HUF rows
    /// carry no rate metadata.
    #[test]
    fn draft_created_huf_explicit_currency_no_rate_metadata() {
        use std::path::PathBuf;

        let invoice = fixture_invoice();
        let idem = IdempotencyKey::new();
        let path = PathBuf::from("/tmp/out/inv_01J0.xml");
        let payload =
            InvoiceDraftCreatedPayload::from_invoice_with_xml_path(&invoice, idem, path);
        let decoded: InvoiceDraftCreatedPayload =
            serde_json::from_slice(&payload.to_bytes()).expect("HUF payload must round-trip");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.currency.as_deref(), Some("HUF"));
        assert_eq!(decoded.exchange_rate, None);
        assert_eq!(decoded.exchange_rate_source, None);
        assert_eq!(decoded.exchange_rate_date, None);
        assert_eq!(decoded.huf_equivalent_total, None);
    }

    // ── PR-7-B-3 NAV-submission payload round-trips ─────────────────

    /// Fixture XML carrying the same JSON-hostile bytes as
    /// `fixture_invoice()` carries in line descriptions — quotes,
    /// backslashes, control chars, non-ASCII. The typed-struct path
    /// MUST escape every one of these when wrapping the verbatim NAV
    /// body into the audit-payload `Vec<u8>` field. Closes F9 for the
    /// PR-7-B-3 NAV submission path.
    fn fixture_hostile_xml() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"<ManageInvoiceRequest>");
        out.extend_from_slice(b"<note>\"quotes\" \\ backslashes \n\t control</note>");
        out.extend_from_slice("ünïcödé and other non-ASCII: 日本語".as_bytes());
        out.extend_from_slice(b"</ManageInvoiceRequest>");
        out
    }

    #[test]
    fn submission_attempt_round_trips_hostile_xml() {
        let payload = InvoiceSubmissionAttemptPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            IdempotencyKey::new(),
            "test",
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();

        // First: the bytes must be valid JSON. PR-5's `format!`-built
        // JSON failed exactly this assertion when interpolating a
        // string with `"`.
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");

        // Second: the typed round-trip must reproduce the struct
        // byte-for-byte — including the hostile XML bytes inside
        // `request_xml`. If serde drops or re-escapes a byte, this
        // fails for that variant.
        let decoded: InvoiceSubmissionAttemptPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.request_xml, fixture_hostile_xml());
        assert_eq!(decoded.endpoint, "test");
        assert!(decoded.idempotency_key.starts_with("idem_"));
    }

    #[test]
    fn submission_response_round_trips_hostile_xml() {
        let payload = InvoiceSubmissionResponsePayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            IdempotencyKey::new(),
            "txid-with-\"-quote-and-\\-backslash",
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceSubmissionResponsePayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        // Even the transaction_id round-trips with hostile chars —
        // NAV's tracking ids are opaque, so ABERP defends downstream
        // tooling against unusual but legal characters.
        assert_eq!(
            decoded.transaction_id,
            "txid-with-\"-quote-and-\\-backslash"
        );
    }

    #[test]
    fn ack_status_round_trips_hostile_xml() {
        let payload = InvoiceAckStatusPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "txid-1",
            "SAVED",
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceAckStatusPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
    }

    // ── PR-8 operator-unblock payload round-trips ───────────────────

    /// `InvoiceRetryRequestedPayload` round-trips clean even when the
    /// operator's reason text carries JSON-hostile characters — the
    /// typed-struct path is the only `format!`-free surface, so an
    /// operator who quotes a stuck-invoice number inside their reason
    /// cannot break the audit chain.
    #[test]
    fn retry_requested_round_trips_with_hostile_reason() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceRetryRequestedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            Some("txid-with-\"-quote-and-\\-backslash".to_string()),
            Some("PROCESSING".to_string()),
            "operator note: \"customer X\" insists on resubmit \\ urgent",
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceRetryRequestedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert!(decoded.idempotency_key.starts_with("idem_"));
    }

    /// `InvoiceRetryRequestedPayload` accepts `prior_last_ack_status =
    /// None` — captures the legitimate-but-unusual case of an operator
    /// retrying before any poll ran (e.g. the submit-invoice flow saw a
    /// non-retryable error from NAV's per-attempt error path and the
    /// operator decided to retry without first running poll-ack).
    #[test]
    fn retry_requested_accepts_none_prior_last_ack_status() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceRetryRequestedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            Some("prior-txid".to_string()),
            None,
            "no prior poll — operator retried directly",
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceRetryRequestedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert!(decoded.prior_last_ack_status.is_none());
    }

    /// PR-19 / ADR-0032 §4: `InvoiceRetryRequestedPayload` accepts
    /// `prior_transaction_id = None` — the state-2 Pending retry
    /// shape. Captures the case where the prior Attempt's wire broke
    /// (or the process crashed before TX2 commit per ADR-0032 §1) so
    /// no `InvoiceSubmissionResponse` exists yet. CLAUDE.md rule 9:
    /// the round-trip pins the wire shape so a future serde refactor
    /// that drops `Option`-ness from the field surfaces here, not
    /// silently in production audit bytes.
    #[test]
    fn retry_requested_accepts_none_prior_transaction_id_for_state_2() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceRetryRequestedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            None,
            None,
            "state-2 Pending retry — prior Attempt's wire broke before NAV responded",
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceRetryRequestedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert!(decoded.prior_transaction_id.is_none());
        assert!(decoded.prior_last_ack_status.is_none());
    }

    /// PR-19 / ADR-0032 §4: pre-PR-19 `InvoiceRetryRequestedPayload`
    /// bytes (where `prior_transaction_id` was a bare `String`,
    /// not `Option<String>`) deserialise transparently into the new
    /// `Option<String>` shape as `Some(...)`. Pins the serde
    /// backward-compat contract so a future refactor that breaks
    /// the round-trip surface here, not silently against historical
    /// ledger entries.
    #[test]
    fn retry_requested_deserialises_pre_pr_19_string_bytes_as_some() {
        // Build the pre-PR-19 wire shape by hand — JSON string for
        // `prior_transaction_id`, no `Option` discriminator. The
        // serde_json default for `Option<String>` parses a string
        // as `Some(String)`.
        let pre_pr_19_bytes = br#"{
            "invoice_id":"inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "idempotency_key":"idem_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "prior_transaction_id":"TXID-PR-7-B-3",
            "prior_last_ack_status":"PROCESSING",
            "reason":"pre-PR-19 retry"
        }"#;
        let decoded: InvoiceRetryRequestedPayload =
            serde_json::from_slice(pre_pr_19_bytes).expect("typed decode");
        assert_eq!(
            decoded.prior_transaction_id.as_deref(),
            Some("TXID-PR-7-B-3")
        );
    }

    /// `InvoiceMarkedAbandonedPayload` round-trips clean with hostile
    /// reason text. Same F9 trap-closing posture as
    /// `retry_requested_round_trips_with_hostile_reason`.
    #[test]
    fn marked_abandoned_round_trips_with_hostile_reason() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceMarkedAbandonedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            Some("prior-txid".to_string()),
            Some("RECEIVED".to_string()),
            "abandoned: NAV inspector said issue corrective \"new\" invoice instead",
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceMarkedAbandonedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert!(decoded.idempotency_key.starts_with("idem_"));
    }

    /// PR-19 / ADR-0032 §4: `InvoiceMarkedAbandonedPayload` accepts
    /// `prior_transaction_id = None` — the state-2 Pending
    /// abandonment shape. The operator marks an Attempt-only invoice
    /// abandoned; no `InvoiceSubmissionResponse` exists yet so the
    /// prior transaction id is `None`.
    #[test]
    fn marked_abandoned_accepts_none_prior_transaction_id_for_state_2() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceMarkedAbandonedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            None,
            None,
            "state-2 Pending abandonment — operator gives up after multiple AttemptFailed",
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceMarkedAbandonedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert!(decoded.prior_transaction_id.is_none());
    }

    /// PR-19 / ADR-0032 §4: pre-PR-19 `InvoiceMarkedAbandonedPayload`
    /// bytes deserialise transparently into the new `Option<String>`
    /// shape as `Some(...)`. Mirror of
    /// `retry_requested_deserialises_pre_pr_19_string_bytes_as_some`.
    #[test]
    fn marked_abandoned_deserialises_pre_pr_19_string_bytes_as_some() {
        let pre_pr_19_bytes = br#"{
            "invoice_id":"inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "idempotency_key":"idem_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "prior_transaction_id":"TXID-PR-8",
            "prior_last_ack_status":"ABORTED",
            "reason":"pre-PR-19 abandonment"
        }"#;
        let decoded: InvoiceMarkedAbandonedPayload =
            serde_json::from_slice(pre_pr_19_bytes).expect("typed decode");
        assert_eq!(decoded.prior_transaction_id.as_deref(), Some("TXID-PR-8"));
    }

    /// PR-19 / ADR-0032 §2: `InvoiceSubmissionAttemptFailedPayload`
    /// round-trips clean for every documented `error_class`. CLAUDE.md
    /// rule 9: the round-trip pins both the wire shape AND the
    /// classifier's enumeration vocabulary; a future refactor that
    /// renames `"transport"` to `"net"` (or similar) surfaces here.
    #[test]
    fn attempt_failed_round_trips_for_transport_class() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceSubmissionAttemptFailedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            "transport",
            None,
            "manageInvoice HTTP call failed: connection reset".to_string(),
            None,
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceSubmissionAttemptFailedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.error_class, "transport");
        assert!(decoded.error_code.is_none());
        assert!(decoded.response_xml.is_none());
    }

    /// PR-19 / ADR-0032 §2: an `application`-class failure carries
    /// the NAV error code + the verbatim response body. The bundle
    /// reader pulls `funcCode` / `errorCode` / `message` from the
    /// response body for inspector triage.
    #[test]
    fn attempt_failed_round_trips_for_application_class() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceSubmissionAttemptFailedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "production",
            "application",
            Some("INVALID_SECURITY_USER".to_string()),
            "manageInvoice non-retryable error: INVALID_SECURITY_USER — bad credentials"
                .to_string(),
            Some(b"<ManageInvoiceResponse>...</ManageInvoiceResponse>".to_vec()),
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceSubmissionAttemptFailedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.error_class, "application");
        assert_eq!(
            decoded.error_code.as_deref(),
            Some("INVALID_SECURITY_USER")
        );
        assert!(decoded.response_xml.is_some());
    }

    /// PR-19 / ADR-0032 §2: an `http_status`-class failure carries
    /// the HTTP status as decimal string in `error_code` + the
    /// verbatim response body. Pinned distinctly from `application`
    /// because NAV returns a body for 5xx replies too.
    #[test]
    fn attempt_failed_round_trips_for_http_status_class() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceSubmissionAttemptFailedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            "http_status",
            Some("503".to_string()),
            "manageInvoice returned non-success HTTP status: 503".to_string(),
            Some(b"<html>Service Unavailable</html>".to_vec()),
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceSubmissionAttemptFailedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.error_class, "http_status");
        assert_eq!(decoded.error_code.as_deref(), Some("503"));
    }

    /// PR-19 / ADR-0032 §2: hostile bytes in `error_message`
    /// (operator-facing NAV diagnostic with quote / backslash /
    /// non-ASCII) round-trip clean through the typed-struct path.
    /// Same F9 trap-closing posture as the hostile-reason tests on
    /// the operator-decision payloads.
    #[test]
    fn attempt_failed_round_trips_with_hostile_error_message() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceSubmissionAttemptFailedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            "application",
            Some("SCHEMA_VIOLATION".to_string()),
            "NAV said: \"<invoiceMain>\" has \\bad shape; \u{00e1}rv\u{00ed}zt\u{0151}r\u{0151} test"
                .to_string(),
            Some(b"<response>...</response>".to_vec()),
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceSubmissionAttemptFailedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
    }

    // ── PR-20 / ADR-0033 §2 — InvoiceCheckPerformed round-trips ────

    /// PR-20 / ADR-0033 §2: an `Exists` outcome round-trips with
    /// `failure_class` / `failure_code` / `failure_message` all
    /// `None`. The `response_xml` field is `Some` because NAV
    /// returned an OK body. Pins the constructor's invariant:
    /// `new_for_outcome` cannot populate failure fields.
    #[test]
    fn check_performed_round_trips_for_exists_outcome() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceCheckPerformedPayload::new_for_outcome(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            "INV-default/00042",
            "exists",
            b"<QueryInvoiceCheckRequest>...</QueryInvoiceCheckRequest>".to_vec(),
            b"<QueryInvoiceCheckResponse><invoiceCheckResult>true</invoiceCheckResult></QueryInvoiceCheckResponse>".to_vec(),
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceCheckPerformedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.outcome, "exists");
        assert!(decoded.response_xml.is_some());
        assert!(decoded.failure_class.is_none());
        assert!(decoded.failure_code.is_none());
        assert!(decoded.failure_message.is_none());
    }

    /// PR-20 / ADR-0033 §2: an `Absent` outcome round-trips
    /// with the same shape as `Exists` (failure fields None;
    /// response_xml Some). The discriminator is the `outcome`
    /// field; the bundle reader filters on it.
    #[test]
    fn check_performed_round_trips_for_absent_outcome() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceCheckPerformedPayload::new_for_outcome(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "production",
            "INV-default/00099",
            "absent",
            b"<QueryInvoiceCheckRequest>...</QueryInvoiceCheckRequest>".to_vec(),
            b"<QueryInvoiceCheckResponse><invoiceCheckResult>false</invoiceCheckResult></QueryInvoiceCheckResponse>".to_vec(),
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceCheckPerformedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.outcome, "absent");
        assert!(decoded.failure_class.is_none());
    }

    /// PR-20 / ADR-0033 §2: a `"failure"` outcome with a
    /// transport-class failure (no NAV body) round-trips with
    /// `response_xml` = None and the three failure fields populated.
    /// Mirrors the transport-class shape of
    /// `attempt_failed_round_trips_for_transport_class`.
    #[test]
    fn check_performed_round_trips_for_transport_failure() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceCheckPerformedPayload::new_for_failure(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            "INV-default/00042",
            b"<QueryInvoiceCheckRequest>...</QueryInvoiceCheckRequest>".to_vec(),
            None,
            "transport",
            None,
            "queryInvoiceCheck HTTP call failed: connection reset".to_string(),
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceCheckPerformedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.outcome, "failure");
        assert_eq!(decoded.failure_class.as_deref(), Some("transport"));
        assert!(decoded.failure_code.is_none());
        assert!(decoded.response_xml.is_none());
        assert!(decoded.failure_message.is_some());
    }

    /// PR-20 / ADR-0033 §2: a `"failure"` outcome with an
    /// application-class failure (NAV returned a body) round-trips
    /// with `response_xml` = Some + `failure_code` carrying the
    /// NAV error code. Mirrors the application-class shape of
    /// `attempt_failed_round_trips_for_application_class`.
    #[test]
    fn check_performed_round_trips_for_application_failure() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceCheckPerformedPayload::new_for_failure(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "production",
            "INV-default/00042",
            b"<QueryInvoiceCheckRequest>...</QueryInvoiceCheckRequest>".to_vec(),
            Some(b"<QueryInvoiceCheckResponse><common:funcCode>ERROR</common:funcCode></QueryInvoiceCheckResponse>".to_vec()),
            "application",
            Some("INVALID_SECURITY_USER".to_string()),
            "queryInvoiceCheck non-retryable error: INVALID_SECURITY_USER — bad creds"
                .to_string(),
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceCheckPerformedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.failure_class.as_deref(), Some("application"));
        assert_eq!(
            decoded.failure_code.as_deref(),
            Some("INVALID_SECURITY_USER")
        );
        assert!(decoded.response_xml.is_some());
    }

    /// PR-20 / ADR-0033 §2: hostile bytes in `failure_message`
    /// round-trip clean through the typed-struct path. Same F9
    /// trap-closing posture as
    /// `attempt_failed_round_trips_with_hostile_error_message`.
    #[test]
    fn check_performed_round_trips_with_hostile_failure_message() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceCheckPerformedPayload::new_for_failure(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            "INV-default/00042",
            b"<x/>".to_vec(),
            Some(b"<response>...</response>".to_vec()),
            "application",
            Some("SCHEMA_VIOLATION".to_string()),
            "NAV said: \"<invoiceMain>\" has \\bad shape; \u{00e1}rv\u{00ed}zt\u{0151}r\u{0151} test"
                .to_string(),
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceCheckPerformedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
    }

    // ── PR-10 storno-chain payload round-trips (ADR-0023 §3) ────────

    /// Round-trip the storno chain-link payload through the typed
    /// serde path. Same F9 trap-closing posture: even though the
    /// invoice id and reservation id are constrained by their
    /// ULID-prefixed shape (no quote/backslash chars by construction),
    /// the round-trip is the canonical proof that ADR-0023 §3's
    /// payload contract holds in code.
    #[test]
    fn storno_issued_round_trip() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceStornoIssuedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAW",
            42,
            "rsv_01ARZ3NDEKTSV4RRFFQ69G5FAX",
            idem,
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            7,
            1,
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceStornoIssuedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        // Pin every chain-link field byte-for-byte. CLAUDE.md rule 9:
        // a test that only checked `decoded == payload` would still
        // pass if a future refactor dropped fields and PartialEq
        // happened to drop with them. Field-by-field walk catches it.
        assert_eq!(decoded.storno_invoice_id, "inv_01ARZ3NDEKTSV4RRFFQ69G5FAW");
        assert_eq!(decoded.storno_seq, 42);
        assert_eq!(
            decoded.storno_reservation_id,
            "rsv_01ARZ3NDEKTSV4RRFFQ69G5FAX"
        );
        assert!(decoded.idempotency_key.starts_with("idem_"));
        assert_eq!(decoded.base_invoice_id, "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(decoded.base_sequence_number, 7);
        assert_eq!(decoded.modification_index, 1);
    }

    /// `modification_index` must round-trip cleanly across the full
    /// `u32` range. Higher chain indices on a long-running base
    /// invoice are legitimate; storage as `u32` should not truncate.
    #[test]
    fn storno_issued_round_trip_preserves_high_modification_index() {
        let payload = InvoiceStornoIssuedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAW",
            42,
            "rsv_01ARZ3NDEKTSV4RRFFQ69G5FAX",
            IdempotencyKey::new(),
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            7,
            u32::MAX,
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceStornoIssuedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded.modification_index, u32::MAX);
    }

    // ── PR-11 modification-chain payload round-trips (ADR-0024 §5) ──

    /// Round-trip the MODIFY chain-link payload through serde. Same
    /// posture as `storno_issued_round_trip`: the round-trip is the
    /// canonical proof that ADR-0024 §5's payload contract holds in
    /// code. The `modification_issue_date` field carries the
    /// MODIFY-only delta over the storno shape; this test pins that
    /// it round-trips byte-for-byte.
    #[test]
    fn modification_issued_round_trip() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceModificationIssuedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAW",
            42,
            "rsv_01ARZ3NDEKTSV4RRFFQ69G5FAX",
            idem,
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            7,
            1,
            "2026-05-21",
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceModificationIssuedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        // CLAUDE.md rule 9: field-by-field pin so a future PartialEq-
        // dropping refactor still surfaces. The MODIFY-only field
        // `modification_issue_date` is the most likely silent-drop
        // target since it has no STORNO analogue.
        assert_eq!(
            decoded.modification_invoice_id,
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAW"
        );
        assert_eq!(decoded.modification_seq, 42);
        assert_eq!(
            decoded.modification_reservation_id,
            "rsv_01ARZ3NDEKTSV4RRFFQ69G5FAX"
        );
        assert!(decoded.idempotency_key.starts_with("idem_"));
        assert_eq!(decoded.base_invoice_id, "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(decoded.base_sequence_number, 7);
        assert_eq!(decoded.modification_index, 1);
        assert_eq!(decoded.modification_issue_date, "2026-05-21");
    }

    /// `modification_index` must round-trip cleanly across the full
    /// `u32` range — same rationale as
    /// `storno_issued_round_trip_preserves_high_modification_index`.
    /// The MODIFY chain index is allocated from the union walk over
    /// both `InvoiceStornoIssued` and `InvoiceModificationIssued`
    /// entries (ADR-0024 §7), so a long-running base with many
    /// corrections plus a storno can plausibly reach higher indices
    /// than the storno-only walk would.
    #[test]
    fn modification_issued_round_trip_preserves_high_modification_index() {
        let payload = InvoiceModificationIssuedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAW",
            42,
            "rsv_01ARZ3NDEKTSV4RRFFQ69G5FAX",
            IdempotencyKey::new(),
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            7,
            u32::MAX,
            "2026-05-21",
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceModificationIssuedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded.modification_index, u32::MAX);
        assert_eq!(decoded.modification_issue_date, "2026-05-21");
    }

    /// The trap PR-6.1 closed: PR-5's `format!`-built JSON could not
    /// safely interpolate strings with embedded quotes / backslashes.
    /// The typed-struct path *must* escape them and produce valid
    /// JSON that round-trips. If this fixture ever stops carrying
    /// hostile characters, the trap can regress silently.
    #[test]
    fn round_trip_preserves_json_hostile_characters() {
        let invoice = fixture_invoice();
        let reservation = fixture_reservation(invoice.id, invoice.series_id);
        let idem = IdempotencyKey::new();
        let payload = InvoiceSequenceReservedPayload::from_outcome(&invoice, &reservation, idem);
        let bytes = payload.to_bytes();

        // Sanity: the bytes are valid JSON. (If `to_vec` produced
        // malformed JSON, `from_slice` to a `serde_json::Value` would
        // fail before we even compared structs.)
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        assert!(v.is_object());

        // The struct itself must round-trip.
        let decoded: InvoiceSequenceReservedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
    }

    // ── PR-12 technical-annulment payload round-trips (ADR-0025 §3) ─

    /// Round-trip the technical-annulment payload through serde.
    /// Same field-by-field pin posture as
    /// `modification_issued_round_trip`: CLAUDE.md rule 9 — assert
    /// the intent, not just the round-trip equality. The four
    /// chain-link-absent fields (`invoice_id` is the BASE, not a new
    /// invoice; no chain index; no modification_issue_date) make the
    /// shape contrast with STORNO/MODIFY load-bearing.
    #[test]
    fn technical_annulment_requested_round_trip() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceTechnicalAnnulmentRequestedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "TXID-42",
            "ERRATIC_DATA",
            "test invoice accidentally sent to production",
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceTechnicalAnnulmentRequestedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        // Field-by-field pin per CLAUDE.md rule 9.
        assert_eq!(decoded.invoice_id, "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(decoded.idempotency_key.starts_with("idem_"));
        assert_eq!(decoded.prior_transaction_id, "TXID-42");
        assert_eq!(decoded.annulment_code, "ERRATIC_DATA");
        assert_eq!(
            decoded.reason,
            "test invoice accidentally sent to production"
        );
    }

    // ── PR-13 annulment wire-evidence payload round-trips (ADR-0026 §2) ─

    /// Round-trip the annulment-wire-attempt payload. Field-by-field
    /// pin per CLAUDE.md rule 9 — a future PartialEq-dropping
    /// refactor still surfaces because each field is asserted. The
    /// `endpoint` field is the load-bearing test/production
    /// distinction; pin it explicitly so a future contributor cannot
    /// silently drop the audit-bearing environment label.
    #[test]
    fn annulment_submission_attempt_round_trip() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceAnnulmentSubmissionAttemptPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceAnnulmentSubmissionAttemptPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.invoice_id, "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(decoded.idempotency_key.starts_with("idem_"));
        assert_eq!(decoded.endpoint, "test");
        assert_eq!(decoded.request_xml, fixture_hostile_xml());
    }

    /// Round-trip the annulment-wire-response payload. Same posture
    /// as the attempt above; additionally pins `transaction_id`
    /// round-trip cleanliness across JSON-hostile bytes (NAV's
    /// annulment-side transaction ids are opaque, and defending
    /// downstream tooling against unusual but legal characters is
    /// the same posture
    /// `submission_response_round_trips_hostile_xml` takes).
    #[test]
    fn annulment_submission_response_round_trip_with_hostile_txid() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceAnnulmentSubmissionResponsePayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "txid-with-\"-quote-and-\\-backslash",
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceAnnulmentSubmissionResponsePayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(
            decoded.transaction_id,
            "txid-with-\"-quote-and-\\-backslash"
        );
        assert!(decoded.idempotency_key.starts_with("idem_"));
    }

    /// ADR-0026 §2 explicitly forks the annulment wire-evidence
    /// payloads from the invoice ones for type-safe distinction.
    /// Pin that the two attempt struct types do NOT round-trip
    /// through each other's deserializer — even when the JSON shape
    /// happens to be compatible (it is, because the field names are
    /// the same). The type-system distinction is what stops a
    /// future audit-evidence-bundle reader from silently
    /// deserializing one as the other; this test pins the *intent*
    /// (CLAUDE.md rule 9) — if a refactor merges the two struct
    /// types into one, the type-equality assert at compile time
    /// would catch the merge, but THIS test catches the case where
    /// someone keeps two struct types but copy-pastes one's tests
    /// against the other's bytes.
    ///
    /// Note: the JSON IS structurally identical (same field names);
    /// the test verifies the typed Rust round-trip is field-for-field
    /// equivalent across the disjoint discriminators, NOT that
    /// serde refuses the cross-type decode. The discriminator lives
    /// at the EventKind level, not in the JSON.
    #[test]
    fn annulment_attempt_payload_is_structurally_parallel_to_invoice_attempt() {
        let idem = IdempotencyKey::new();
        let annulment = InvoiceAnnulmentSubmissionAttemptPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            b"<ManageAnnulmentRequest/>".to_vec(),
        );
        let invoice = InvoiceSubmissionAttemptPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "test",
            b"<ManageInvoiceRequest/>".to_vec(),
        );
        // The two structs are deliberately distinct types per
        // ADR-0026 §2; this is the load-bearing compile-time
        // distinction. At runtime, both `to_bytes` outputs are
        // valid JSON of the same shape, BUT a JSON value decoded
        // from one cannot be assigned to the other without going
        // through serde_json::from_slice — which is the call site
        // where the type system enforces the distinction. The
        // audit-evidence bundle reader keys on EventKind, not on
        // payload JSON shape, so the discriminator is the
        // load-bearing distinction.
        let annulment_bytes = annulment.to_bytes();
        let invoice_bytes = invoice.to_bytes();
        // Different request_xml bytes -> different serialized JSON.
        assert_ne!(annulment_bytes, invoice_bytes);
    }

    // ── PR-14 annulment ack-status payload round-trips (ADR-0027 §2) ─

    /// Round-trip the annulment-side ack-status payload. Field-by-
    /// field pin per CLAUDE.md rule 9 — a future PartialEq-dropping
    /// refactor still surfaces because each field is asserted. The
    /// `ack_status` field is the load-bearing terminal-vs-
    /// intermediate distinction; pin it explicitly so a future
    /// contributor cannot silently drop the NAV-enumeration string.
    #[test]
    fn annulment_ack_status_round_trip() {
        let payload = InvoiceAnnulmentAckStatusPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "NAV-ANNUL-TXID-42",
            "SAVED",
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceAnnulmentAckStatusPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        // Field-by-field pin per CLAUDE.md rule 9.
        assert_eq!(decoded.invoice_id, "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(decoded.transaction_id, "NAV-ANNUL-TXID-42");
        assert_eq!(decoded.ack_status, "SAVED");
        assert_eq!(decoded.response_xml, fixture_hostile_xml());
    }

    /// PR-14 / ADR-0027 §2: the annulment ack-status payload is
    /// structurally parallel to `InvoiceAckStatusPayload`, forked
    /// deliberately so the type system enforces the kind ⇄ payload
    /// binding at the audit-evidence-bundle reader's call sites.
    /// Same posture as the PR-13 attempt-payload parallel test.
    /// The JSON IS structurally identical (same field names); the
    /// type-system distinction is what stops a future bundle
    /// reader from silently deserializing one as the other.
    #[test]
    fn annulment_ack_status_payload_is_structurally_parallel_to_invoice_ack_status() {
        let annulment = InvoiceAnnulmentAckStatusPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "NAV-ANNUL-TXID-42",
            "PROCESSING",
            b"<QueryTransactionStatusResponse/>".to_vec(),
        );
        let invoice = InvoiceAckStatusPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "NAV-INV-TXID-42",
            "PROCESSING",
            b"<QueryTransactionStatusResponse/>".to_vec(),
        );
        let a = annulment.to_bytes();
        let i = invoice.to_bytes();
        // Different transaction_id bytes -> different serialized
        // JSON. The discriminator at the EventKind level is the
        // load-bearing distinction; this pin documents that the
        // payload-byte distinction by transaction id holds at the
        // JSON level too.
        assert_ne!(a, i);
    }

    /// F9 trap-closing posture: NAV's annulment-side transaction
    /// ids are opaque and could carry JSON-hostile characters. The
    /// typed-struct path MUST escape them and produce valid JSON
    /// that round-trips. Mirror of
    /// `submission_response_round_trips_hostile_xml` /
    /// `annulment_submission_response_round_trip_with_hostile_txid`.
    #[test]
    fn annulment_ack_status_round_trips_with_hostile_txid() {
        let payload = InvoiceAnnulmentAckStatusPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "txid-with-\"-quote-and-\\-backslash",
            "ABORTED",
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceAnnulmentAckStatusPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(
            decoded.transaction_id,
            "txid-with-\"-quote-and-\\-backslash"
        );
    }

    // ── PR-15 annulment-receiver-confirmation payload round-trips (ADR-0028 §2) ─

    /// Round-trip the receiver-confirmation observation payload.
    /// Field-by-field pin per CLAUDE.md rule 9 — a future
    /// PartialEq-dropping refactor still surfaces because each
    /// field is asserted. The `nav_invoice_number` field is the
    /// load-bearing query-input record; pin it explicitly so a
    /// future contributor cannot silently drop the recorded
    /// query-key. Similarly `annulment_transaction_id` anchors
    /// the back-walk to the annulment lineage; pin it too.
    #[test]
    fn annulment_receiver_confirmation_round_trip() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceAnnulmentReceiverConfirmationPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "INV-default/00042",
            "NAV-ANNUL-TXID-42",
            idem,
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceAnnulmentReceiverConfirmationPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        // Field-by-field pin per CLAUDE.md rule 9.
        assert_eq!(decoded.invoice_id, "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(decoded.nav_invoice_number, "INV-default/00042");
        assert_eq!(decoded.annulment_transaction_id, "NAV-ANNUL-TXID-42");
        assert!(decoded.idempotency_key.starts_with("idem_"));
        assert_eq!(decoded.response_xml, fixture_hostile_xml());
    }

    /// PR-15 / ADR-0028 §2: the receiver-confirmation payload is
    /// structurally distinct from `InvoiceAnnulmentAckStatusPayload`
    /// — it carries TWO additional fields (`nav_invoice_number` +
    /// `annulment_transaction_id`) and OMITS `ack_status`. The
    /// type-system distinction is what stops a future audit-evidence
    /// bundle reader from silently deserializing one as the other.
    /// Same posture as the PR-13 / PR-14 parallel-payload tests.
    /// The JSON shapes are DIFFERENT here (vs PR-13's
    /// `annulment_attempt_payload_is_structurally_parallel_to_invoice_attempt`
    /// where the JSON shapes happen to match); pinning that
    /// difference is the load-bearing check.
    #[test]
    fn annulment_receiver_confirmation_payload_has_distinct_json_shape_from_ack_status() {
        let idem = IdempotencyKey::new();
        let receiver = InvoiceAnnulmentReceiverConfirmationPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "INV-default/00042",
            "NAV-ANNUL-TXID-42",
            idem,
            b"<QueryInvoiceDataResponse/>".to_vec(),
        );
        let ack = InvoiceAnnulmentAckStatusPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "NAV-ANNUL-TXID-42",
            "SAVED",
            b"<QueryTransactionStatusResponse/>".to_vec(),
        );
        let r = receiver.to_bytes();
        let a = ack.to_bytes();
        // Different field sets -> different serialized JSON.
        assert_ne!(r, a);
        // The receiver-confirmation JSON must contain
        // `nav_invoice_number` (the field that distinguishes it
        // from ack-status). Pinning the field name catches a
        // future rename that would silently break the audit-
        // evidence bundle reader's discrimination by JSON shape
        // (even though kind-alone classification at the audit-
        // ledger level is the load-bearing distinction).
        let r_str = std::str::from_utf8(&r).expect("utf-8");
        assert!(
            r_str.contains("nav_invoice_number"),
            "receiver-confirmation payload must carry nav_invoice_number field: {r_str}"
        );
        assert!(
            r_str.contains("annulment_transaction_id"),
            "receiver-confirmation payload must carry annulment_transaction_id field: {r_str}"
        );
        assert!(
            !r_str.contains("\"ack_status\""),
            "receiver-confirmation payload must NOT carry an ack_status field \
             (per ADR-0028 §\"Surfaced conflict 3\" verbatim-bytes-only posture): {r_str}"
        );
    }

    /// F9 trap-closing posture: NAV-side strings (annulment
    /// transaction id, nav_invoice_number) and verbatim response
    /// bytes could in principle carry JSON-hostile characters.
    /// The typed-struct path MUST escape them and produce valid
    /// JSON that round-trips. Mirror of every prior
    /// `*_round_trips_with_hostile_*` test in this module.
    #[test]
    fn annulment_receiver_confirmation_round_trips_with_hostile_inputs() {
        let payload = InvoiceAnnulmentReceiverConfirmationPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "INV-\"weird\"-series/00042",
            "txid-with-\"-quote-and-\\-backslash",
            IdempotencyKey::new(),
            fixture_hostile_xml(),
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceAnnulmentReceiverConfirmationPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert_eq!(decoded.nav_invoice_number, "INV-\"weird\"-series/00042");
        assert_eq!(
            decoded.annulment_transaction_id,
            "txid-with-\"-quote-and-\\-backslash"
        );
    }

    /// F9 trap-closing posture: the operator-supplied reason text may
    /// carry JSON-hostile characters (quotes / backslashes / control
    /// chars / non-ASCII). The typed-struct path MUST escape them and
    /// produce valid JSON that round-trips. Mirror of
    /// `marked_abandoned_round_trips_with_hostile_reason` /
    /// `retry_requested_round_trips_with_hostile_reason`.
    #[test]
    fn technical_annulment_round_trips_with_hostile_reason() {
        let idem = IdempotencyKey::new();
        let payload = InvoiceTechnicalAnnulmentRequestedPayload::new(
            "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            idem,
            "txid-with-\"-quote-and-\\-backslash",
            "ERRATIC_INVOICE_NUMBER",
            "accountant note: \"customer X\" reported wrong number \\ ünïcödé",
        );
        let bytes = payload.to_bytes();
        let _: serde_json::Value =
            serde_json::from_slice(&bytes).expect("bytes must be valid JSON");
        let decoded: InvoiceTechnicalAnnulmentRequestedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        // Even the prior_transaction_id round-trips with hostile chars
        // — NAV's tracking ids are opaque per
        // `submission_response_round_trips_hostile_xml`'s same posture.
        assert_eq!(
            decoded.prior_transaction_id,
            "txid-with-\"-quote-and-\\-backslash"
        );
        assert_eq!(decoded.annulment_code, "ERRATIC_INVOICE_NUMBER");
    }
}
