//! [`EventKind`] — typed event kinds per ADR-0008 §"Entry shape".
//!
//! `kind` is the type discriminant for `payload`'s schema. Schema versioning
//! is implicit in the kind name: bumping a payload schema renames the kind,
//! and the old kind remains valid for historical entries.
//!
//! No serde derive: PR-3 stores the kind as a plain text column in DuckDB
//! via [`EventKind::as_str`]. Serde will join when a serialization path
//! (export bundle, wire protocol) actually needs it.

/// PR-3 shipped only `Test`. PR-5 added the first two invoice-lifecycle
/// kinds from ADR-0009 §2 (`InvoiceSequenceReserved`, `InvoiceDraftCreated`).
/// PR-7-B-3 adds the three NAV-submission evidence kinds from ADR-0009 §8
/// (`InvoiceSubmissionAttempt`, `InvoiceSubmissionResponse`,
/// `InvoiceAckStatus`). The first two of those three fire in PR-7-B-3's
/// `submit-invoice` flow; `InvoiceAckStatus` is added now (rather than
/// in PR-7-C) so the three-coordinated-edit trap (PR-6.1 F12 — variant +
/// `as_str` + `from_storage_str` + the test-list array) is closed for the
/// whole NAV submission path in one PR.
///
/// PR-8 adds two operator-unblock kinds from ADR-0009 §5
/// (`InvoiceRetryRequested`, `InvoiceMarkedAbandoned`). Each marks an
/// **operator-initiated** event distinct from the per-attempt NAV
/// evidence kinds: `InvoiceRetryRequested` records the operator's
/// decision to re-submit a stuck invoice (the retry itself then
/// produces normal `InvoiceSubmissionAttempt` / `InvoiceSubmissionResponse`
/// entries via the existing submit pipeline); `InvoiceMarkedAbandoned`
/// records the operator's decision to stop retrying. Both adds
/// re-exercise the F12 four-coordinated-edit trap — variant +
/// `as_str` + `from_storage_str` + the `round_trip_for_every_variant`
/// hand-listed array. This is the first PR since PR-6.1 to add a new
/// variant; the trap is performing its job by definition only if all
/// four edits land in the same commit.
///
/// The remaining invoice-lifecycle kinds (`Finalized`, `Rejected`,
/// `SubmissionStuck`, `Voided`, `Amended`, `StornoIssued`,
/// `TechnicalAnnulmentRequested`) land when their state transition
/// first fires in the codebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// Test-only kind used by `tests/chain_conformance.rs`. Not allowed in
    /// production code; a future conformance check should gate this.
    Test,

    /// A sequence number was reserved in `invoice_sequence_reservation`
    /// as part of the atomic allocator (ADR-0009 §3).
    InvoiceSequenceReserved,

    /// An invoice row was inserted with state `Draft` (ADR-0009 §2).
    /// In PR-5 this fires together with `InvoiceSequenceReserved`
    /// because the binary's command path goes Draft -> Ready in one
    /// allocator call. A future PR may split them.
    InvoiceDraftCreated,

    /// A `manageInvoice` request was POSTed to NAV. Payload carries the
    /// verbatim request XML (ADR-0009 §8). Fires before the response is
    /// received so a crash between POST and response still leaves the
    /// audit trail intact. PR-7-B-3.
    InvoiceSubmissionAttempt,

    /// A `manageInvoice` response was received from NAV with the
    /// `transactionId`. Payload carries the verbatim response XML and
    /// the parsed `transaction_id`. Fires AFTER `InvoiceSubmissionAttempt`
    /// in the same `submit-invoice` flow. PR-7-B-3.
    InvoiceSubmissionResponse,

    /// A `queryTransactionStatus` poll completed. Payload carries the
    /// verbatim response XML and the parsed ack status
    /// (`RECEIVED` / `PROCESSING` / `SAVED` / `ABORTED`). PR-7-C will
    /// emit this; the variant is declared in PR-7-B-3 to close the
    /// three-coordinated-edit trap in one go.
    InvoiceAckStatus,

    /// The operator initiated a re-submission of an invoice that is in
    /// the `SubmissionStuck` precondition per ADR-0009 §5. Payload
    /// carries the prior `transaction_id`, the prior last ack status
    /// (the audit precondition justification), and the operator's
    /// reason text. The retry itself then fires the normal
    /// `InvoiceSubmissionAttempt` + `InvoiceSubmissionResponse` pair
    /// via the existing submit pipeline; this kind records the
    /// **operator's decision** distinctly so the audit-evidence
    /// bundle (ADR-0009 §8) makes the unblock explicit. PR-8.
    InvoiceRetryRequested,

    /// The operator marked a stuck invoice abandoned per ADR-0009 §5.
    /// Terminal in the audit ledger — no further automatic state
    /// advance is permitted for this invoice. Payload carries the
    /// prior `transaction_id`, the prior last ack status, and the
    /// operator's reason text. PR-8.
    InvoiceMarkedAbandoned,
}

impl EventKind {
    /// Render in the on-disk form. Paired with [`EventKind::from_storage_str`]
    /// as a round-trip-proven pair (unit tests in this module check that
    /// for every variant `V`, `from_storage_str(V.as_str()) == Ok(V)`).
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Test => "test",
            EventKind::InvoiceSequenceReserved => "invoice.sequence_reserved",
            EventKind::InvoiceDraftCreated => "invoice.draft_created",
            EventKind::InvoiceSubmissionAttempt => "invoice.submission_attempt",
            EventKind::InvoiceSubmissionResponse => "invoice.submission_response",
            EventKind::InvoiceAckStatus => "invoice.ack_status",
            EventKind::InvoiceRetryRequested => "invoice.retry_requested",
            EventKind::InvoiceMarkedAbandoned => "invoice.marked_abandoned",
        }
    }

    /// Parse the on-disk form back into an `EventKind`. Errors on
    /// unknown strings — silent fallback would mask schema drift per
    /// CLAUDE.md rule 12 ("fail loud").
    ///
    /// Adding a new `EventKind` variant requires three coordinated
    /// edits: the variant itself, an arm in [`EventKind::as_str`],
    /// and an arm here. The round-trip unit test below will fail
    /// loudly if `as_str` and `from_storage_str` ever drift apart
    /// for an existing variant. Adding a variant without updating
    /// this function is a compile error only if the new variant's
    /// `as_str` arm is also added — caller is on the hook for both;
    /// PR-6.1 surfaced this trap (Fortnightly review F12).
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "test" => Ok(EventKind::Test),
            "invoice.sequence_reserved" => Ok(EventKind::InvoiceSequenceReserved),
            "invoice.draft_created" => Ok(EventKind::InvoiceDraftCreated),
            "invoice.submission_attempt" => Ok(EventKind::InvoiceSubmissionAttempt),
            "invoice.submission_response" => Ok(EventKind::InvoiceSubmissionResponse),
            "invoice.ack_status" => Ok(EventKind::InvoiceAckStatus),
            "invoice.retry_requested" => Ok(EventKind::InvoiceRetryRequested),
            "invoice.marked_abandoned" => Ok(EventKind::InvoiceMarkedAbandoned),
            _ => Err("unknown EventKind storage string"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip every known variant. If a future contributor adds a
    /// variant + `as_str` arm but forgets the `from_storage_str` arm,
    /// this test fails for that variant — the maintenance trap F12
    /// named is now caught at test time, not at runtime against a
    /// production row.
    #[test]
    fn round_trip_for_every_variant() {
        // Hand-listed so a future variant addition makes the maintainer
        // *think* about whether they updated this list. `strum`-style
        // auto-iteration would silently exclude a new variant if the
        // contributor forgot to add a derive — exactly the trap.
        let variants = [
            EventKind::Test,
            EventKind::InvoiceSequenceReserved,
            EventKind::InvoiceDraftCreated,
            EventKind::InvoiceSubmissionAttempt,
            EventKind::InvoiceSubmissionResponse,
            EventKind::InvoiceAckStatus,
            EventKind::InvoiceRetryRequested,
            EventKind::InvoiceMarkedAbandoned,
        ];
        for v in variants {
            let s = v.as_str();
            let parsed = EventKind::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?} -> {e}"));
            assert_eq!(parsed, v, "round-trip mismatch for {s:?}");
        }
    }

    #[test]
    fn from_storage_str_rejects_unknown() {
        assert!(EventKind::from_storage_str("invoice.future_kind").is_err());
        assert!(EventKind::from_storage_str("").is_err());
    }

    /// PR-7-B-3 specifically: the three new on-disk strings must
    /// match the dot-separated convention so existing tooling that
    /// filters by prefix (`invoice.*`) catches them. If a future
    /// contributor renames one without the `invoice.` prefix, this
    /// assertion fires.
    #[test]
    fn pr_7_b_3_kinds_use_invoice_prefix() {
        assert!(EventKind::InvoiceSubmissionAttempt
            .as_str()
            .starts_with("invoice."));
        assert!(EventKind::InvoiceSubmissionResponse
            .as_str()
            .starts_with("invoice."));
        assert!(EventKind::InvoiceAckStatus.as_str().starts_with("invoice."));
    }

    /// PR-8 specifically: the two operator-unblock kinds must also use
    /// the `invoice.` prefix so the audit-evidence bundle (ADR-0009 §8)
    /// can be filtered with the same prefix glob as the NAV-evidence
    /// kinds. Same loud-fail rationale as `pr_7_b_3_kinds_use_invoice_prefix`.
    #[test]
    fn pr_8_operator_unblock_kinds_use_invoice_prefix() {
        assert!(EventKind::InvoiceRetryRequested
            .as_str()
            .starts_with("invoice."));
        assert!(EventKind::InvoiceMarkedAbandoned
            .as_str()
            .starts_with("invoice."));
    }
}
