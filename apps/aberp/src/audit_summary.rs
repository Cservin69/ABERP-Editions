// S424 / session-424 — kind-aware subject + one-line summary extractors
// for the cross-domain audit-events screen (`GET /api/audit-events`).
//
// The audit ledger stores one JSON payload per entry (NOT cbor — the
// cbor is only the hash input, ADR-0035 §8). The list endpoint must NOT
// ship the full payload in every row (NAV request/response XML blobs are
// base64 strings tens of KB each — a 50-row page of submission events
// would be multiple MB; see the audit-screen design report §3.3). So the
// row carries two cheap projections instead:
//
//   * `subject_of(entry)`  — the single operator-meaningful id the row
//     is "about" (invoice_id / quote_id / …), for the Subject column.
//   * `summary_of(entry)`  — a ONE-LINE human summary built from a small
//     set of VERIFIED payload fields (the ✅ rows in the design report's
//     §1.1 table). For every other kind it returns "" and the SPA falls
//     back to the kind label — fail-soft to the kind name, NEVER mislabel
//     a field (CLAUDE.md rule 12 + design risk #3).
//
// Plus `subject_matches(entry, needle)` — the SERVER-side subject filter.
// It deliberately checks the needle against EVERY subject-ish key (own id
// AND chain-base id) so an operator searching `invoice:INV-X` finds the
// storno/modification entries whose `base_invoice_id` is X, not just the
// entries whose own id is X ([[hulye-biztos]] — "what happened to X" in
// one click). `subject_of` (display) returns the single highest-priority
// id; `subject_matches` (filter) is the union.
//
// Pure over `&Entry`; pinned by the `#[cfg(test)]` block below.

use aberp_audit_ledger::Entry;
use serde_json::Value;

/// Payload keys that name a subject, in DISPLAY priority order. The first
/// present, non-empty string value is the row's Subject column. Ordered
/// so the entry's OWN primary id wins over a chain-base reference (a
/// storno row reads as "about the storno", with the base reachable via
/// `subject_matches` + the chain-link sidebar).
const SUBJECT_KEYS: &[&str] = &[
    "invoice_id",
    "storno_invoice_id",
    "modification_invoice_id",
    "quote_id",
    "base_invoice_id",
    "queue_row_id",
    "work_order_id",
    "sales_order_id",
    "dispatch_id",
    "inspection_id",
    "adapter_id",
    "vendor_id",
    "partner_id",
    // S432 — material traceability events key on the grade as `material_id`.
    "material_id",
    // S438 — part-UID marking + Part UID Lookup events key on the IUID.
    "part_uid",
    // S439 — NCR / CAPA quality events key on the report / action id.
    "ncr_id",
    "capa_id",
    "nav_invoice_number",
];

/// Decode the entry payload to a JSON object map, or `None` if the bytes
/// are not a JSON object (corrupt entry — the caller degrades to "no
/// subject / empty summary" rather than panicking, CLAUDE.md rule 12).
fn payload_obj(entry: &Entry) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_slice::<Value>(&entry.payload) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The single operator-meaningful id the entry is "about", for the
/// Subject column. First present [`SUBJECT_KEYS`] entry wins; `None` for
/// system/heartbeat entries that carry no subject id.
pub fn subject_of(entry: &Entry) -> Option<String> {
    let obj = payload_obj(entry)?;
    for key in SUBJECT_KEYS {
        if let Some(Value::String(s)) = obj.get(*key) {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

/// SERVER-side subject filter: `true` iff `needle_lower` (already
/// lowercased by the caller) is a substring of ANY subject-ish key's
/// value — own id OR chain-base id. The union (not just `subject_of`)
/// is what makes "find every event touching invoice X" land the storno
/// and modification rows whose own id differs from X.
pub fn subject_matches(entry: &Entry, needle_lower: &str) -> bool {
    let obj = match payload_obj(entry) {
        Some(o) => o,
        None => return false,
    };
    for key in SUBJECT_KEYS {
        if let Some(Value::String(s)) = obj.get(*key) {
            if s.to_lowercase().contains(needle_lower) {
                return true;
            }
        }
    }
    false
}

/// Read a string field off a payload map.
fn s(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Read an integer field off a payload map.
fn i(obj: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    obj.get(key).and_then(Value::as_i64)
}

/// Format a minor-unit amount as a 2-decimal major value. Common-case
/// (EUR/USD/2-digit) is exact; the authoritative `amount_minor` stays in
/// the payload for drill-down, so this is a glanceable summary, not an
/// accounting figure.
fn money(minor: i64, currency: &str) -> String {
    let major = minor / 100;
    let frac = (minor % 100).abs();
    format!("{major}.{frac:02} {currency}")
}

/// A ONE-LINE human summary built from VERIFIED payload fields. Covers
/// the high-traffic ✅ kinds from the design report §1.1; every other
/// kind returns "" so the SPA renders the kind label instead (fail-soft,
/// never a fabricated field — CLAUDE.md rule 12 / design risk #3).
pub fn summary_of(entry: &Entry) -> String {
    let obj = match payload_obj(entry) {
        Some(o) => o,
        None => return String::new(),
    };
    match entry.kind.as_str() {
        "invoice.sequence_reserved" => match i(&obj, "seq") {
            Some(seq) => format!("Sorszám / Seq #{seq} reserved"),
            None => String::new(),
        },
        "invoice.draft_created" => match i(&obj, "line_count") {
            Some(n) => format!("Draft created · {n} line(s)"),
            None => "Draft created".to_string(),
        },
        "invoice.submission_attempt" => match s(&obj, "endpoint") {
            Some(ep) => format!("Submitted to NAV ({ep})"),
            None => "Submitted to NAV".to_string(),
        },
        "invoice.submission_response" => match s(&obj, "transaction_id") {
            Some(tx) => format!("NAV response · txn {tx}"),
            None => "NAV response".to_string(),
        },
        "invoice.ack_status" => match s(&obj, "ack_status") {
            Some(ack) => format!("NAV ack: {ack}"),
            None => "NAV ack".to_string(),
        },
        "invoice.retry_requested" => match s(&obj, "reason") {
            Some(r) => format!("Retry requested · {r}"),
            None => "Retry requested".to_string(),
        },
        "invoice.marked_abandoned" => match s(&obj, "reason") {
            Some(r) => format!("Marked abandoned · {r}"),
            None => "Marked abandoned".to_string(),
        },
        "invoice.check_performed" => match s(&obj, "outcome") {
            Some(o) => format!("Existence check: {o}"),
            None => "Existence check".to_string(),
        },
        "invoice.payment_recorded" => {
            let method = s(&obj, "method").unwrap_or_default();
            let reference = s(&obj, "reference");
            let head = match (i(&obj, "amount_minor"), s(&obj, "currency")) {
                (Some(a), Some(c)) => format!("Paid {} {}", money(a, &c), method)
                    .trim()
                    .to_string(),
                _ if !method.is_empty() => format!("Paid · {method}"),
                _ => "Payment recorded".to_string(),
            };
            match reference {
                Some(r) if !r.is_empty() => format!("{head} · {r}"),
                _ => head,
            }
        }
        "invoice.storno_issued" => match s(&obj, "base_invoice_id") {
            Some(base) => format!("Storno of {base}"),
            None => "Storno issued".to_string(),
        },
        "invoice.modification_issued" => match s(&obj, "base_invoice_id") {
            Some(base) => format!("Modification of {base}"),
            None => "Modification issued".to_string(),
        },
        "invoice.emailed_sent" => {
            let outcome = s(&obj, "outcome").unwrap_or_default();
            match s(&obj, "recipient") {
                Some(to) if !outcome.is_empty() => format!("Email {outcome} → {to}"),
                Some(to) => format!("Email → {to}"),
                None if !outcome.is_empty() => format!("Email {outcome}"),
                None => "Email sent".to_string(),
            }
        }
        "quote.pricing_fetched" => match (s(&obj, "material_grade"), i(&obj, "quantity")) {
            (Some(m), Some(q)) => format!("Quote fetched · {m} ×{q}"),
            (Some(m), None) => format!("Quote fetched · {m}"),
            _ => "Quote fetched".to_string(),
        },
        "quote.pricing_posted" => match s(&obj, "valid_until_iso") {
            Some(v) => format!("Quote priced · valid until {v}"),
            None => "Quote priced".to_string(),
        },
        "quote.operator_refused" => match s(&obj, "reason") {
            Some(r) => format!("Operator refused · {r}"),
            None => "Operator refused".to_string(),
        },
        "system.numbering_template_changed" => {
            match (i(&obj, "old_start_value"), i(&obj, "new_start_value")) {
                (Some(o), Some(n)) => format!("Numbering floor {o} → {n}"),
                _ => "Numbering template changed".to_string(),
            }
        }
        // Long-tail (◑) kinds: no verified field set — the SPA renders
        // the kind label. Returning "" is the honest fail-soft.
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{
        Actor, BinaryHash, EntryHash, EntryId, EventKind, Sequence, TenantId,
    };
    use time::OffsetDateTime;

    /// Build a synthetic entry with a given kind + JSON payload. The hash
    /// fields are placeholders — these extractors never read them.
    fn entry(kind: EventKind, payload: Value) -> Entry {
        Entry {
            id: EntryId::new(),
            seq: Sequence::FIRST,
            prev_hash: EntryHash::from_bytes([0u8; 32]),
            time_wall: OffsetDateTime::UNIX_EPOCH,
            time_mono: 0,
            actor: Actor {
                session_id: "sess".to_string(),
                user_id: "ervin".to_string(),
                capabilities: Default::default(),
            },
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            tenant_id: TenantId::new("t").unwrap(),
            kind,
            payload: serde_json::to_vec(&payload).unwrap(),
            idempotency_key: None,
            entry_hash: EntryHash::from_bytes([0u8; 32]),
        }
    }

    #[test]
    fn subject_of_prefers_invoice_id() {
        let e = entry(
            EventKind::InvoicePaymentRecorded,
            serde_json::json!({ "invoice_id": "INV-1", "currency": "EUR" }),
        );
        assert_eq!(subject_of(&e).as_deref(), Some("INV-1"));
    }

    #[test]
    fn subject_of_quote() {
        let e = entry(
            EventKind::QuoteOperatorRefused,
            serde_json::json!({ "quote_id": "qpj_9", "reason": "no stock" }),
        );
        assert_eq!(subject_of(&e).as_deref(), Some("qpj_9"));
    }

    #[test]
    fn subject_of_storno_is_own_id_not_base() {
        // Display priority: the storno's own id wins (invoice_id absent).
        let e = entry(
            EventKind::InvoiceStornoIssued,
            serde_json::json!({
                "storno_invoice_id": "INV-2S",
                "base_invoice_id": "INV-2",
            }),
        );
        assert_eq!(subject_of(&e).as_deref(), Some("INV-2S"));
    }

    #[test]
    fn subject_matches_finds_base_invoice_on_storno() {
        // The filter union: searching "inv-2" lands the storno via its
        // base_invoice_id even though subject_of returns the storno's id.
        let e = entry(
            EventKind::InvoiceStornoIssued,
            serde_json::json!({
                "storno_invoice_id": "INV-2S",
                "base_invoice_id": "INV-2",
            }),
        );
        assert!(subject_matches(&e, "inv-2"));
        assert!(subject_matches(&e, "inv-2s"));
        assert!(!subject_matches(&e, "inv-9"));
    }

    #[test]
    fn subject_of_none_for_subjectless_payload() {
        let e = entry(
            EventKind::QuoteIntakePollAttempted,
            serde_json::json!({ "heartbeat": true }),
        );
        assert_eq!(subject_of(&e), None);
    }

    #[test]
    fn summary_payment_recorded() {
        let e = entry(
            EventKind::InvoicePaymentRecorded,
            serde_json::json!({
                "invoice_id": "INV-1",
                "amount_minor": 123_400,
                "currency": "EUR",
                "method": "BankTransfer",
                "reference": "INV-2026/00042",
            }),
        );
        assert_eq!(
            summary_of(&e),
            "Paid 1234.00 EUR BankTransfer · INV-2026/00042"
        );
    }

    #[test]
    fn summary_operator_refused_carries_reason() {
        let e = entry(
            EventKind::QuoteOperatorRefused,
            serde_json::json!({ "quote_id": "q1", "reason": "out of capacity" }),
        );
        assert_eq!(summary_of(&e), "Operator refused · out of capacity");
    }

    #[test]
    fn summary_storno_names_base() {
        let e = entry(
            EventKind::InvoiceStornoIssued,
            serde_json::json!({ "storno_invoice_id": "INV-2S", "base_invoice_id": "INV-2" }),
        );
        assert_eq!(summary_of(&e), "Storno of INV-2");
    }

    #[test]
    fn summary_falls_soft_to_empty_for_longtail_kind() {
        // A ◑ kind with no verified summariser → "" (SPA renders the
        // kind label). NEVER a fabricated field.
        let e = entry(
            EventKind::MaterialReserved,
            serde_json::json!({ "material": "S235", "qty": 5 }),
        );
        assert_eq!(summary_of(&e), "");
    }

    #[test]
    fn summary_ack_status() {
        let e = entry(
            EventKind::InvoiceAckStatus,
            serde_json::json!({ "invoice_id": "INV-1", "ack_status": "SAVED" }),
        );
        assert_eq!(summary_of(&e), "NAV ack: SAVED");
    }
}
