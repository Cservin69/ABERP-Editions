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

use aberp_billing::{IdempotencyKey, ReadyInvoice, SequenceReservation};
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceDraftCreatedPayload {
    pub invoice_id: String,
    pub line_count: usize,
    pub idempotency_key: String,
}

impl InvoiceDraftCreatedPayload {
    pub fn from_invoice(invoice: &ReadyInvoice, idempotency_key: IdempotencyKey) -> Self {
        Self {
            invoice_id: invoice.id.to_prefixed_string(),
            line_count: invoice.lines.len(),
            idempotency_key: idempotency_key.to_canonical_string(),
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceRetryRequestedPayload {
    pub invoice_id: String,
    pub idempotency_key: String,
    /// The NAV `transaction_id` recorded by the most-recent prior
    /// `InvoiceSubmissionResponse` for this invoice. The retry's own
    /// `InvoiceSubmissionResponse` will record a fresh `transaction_id`;
    /// keeping the prior id here makes the unblock decision traceable.
    pub prior_transaction_id: String,
    /// The string form of the most-recent `InvoiceAckStatus` payload's
    /// `ack_status` field for this invoice. `None` if no ack entry
    /// exists (the operator retried before polling — captured here
    /// rather than silently elided).
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
        prior_transaction_id: &str,
        prior_last_ack_status: Option<String>,
        reason: &str,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            prior_transaction_id: prior_transaction_id.to_string(),
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceMarkedAbandonedPayload {
    pub invoice_id: String,
    pub idempotency_key: String,
    /// The NAV `transaction_id` recorded by the most-recent prior
    /// `InvoiceSubmissionResponse` for this invoice. There is no
    /// further `InvoiceSubmissionResponse` after this entry — the
    /// invoice's audit chain is terminal-by-operator-decision.
    pub prior_transaction_id: String,
    pub prior_last_ack_status: Option<String>,
    pub reason: String,
}

impl InvoiceMarkedAbandonedPayload {
    pub fn new(
        invoice_id: &str,
        idempotency_key: IdempotencyKey,
        prior_transaction_id: &str,
        prior_last_ack_status: Option<String>,
        reason: &str,
    ) -> Self {
        Self {
            invoice_id: invoice_id.to_string(),
            idempotency_key: idempotency_key.to_canonical_string(),
            prior_transaction_id: prior_transaction_id.to_string(),
            prior_last_ack_status,
            reason: reason.to_string(),
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
            "txid-with-\"-quote-and-\\-backslash",
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
            "prior-txid",
            None,
            "no prior poll — operator retried directly",
        );
        let bytes = payload.to_bytes();
        let decoded: InvoiceRetryRequestedPayload =
            serde_json::from_slice(&bytes).expect("typed decode");
        assert_eq!(decoded, payload);
        assert!(decoded.prior_last_ack_status.is_none());
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
            "prior-txid",
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
}
