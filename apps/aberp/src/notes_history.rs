//! PR-172 — notes-history typeahead source.
//!
//! Operators issuing repeat invoices to the same customer often type
//! identical buyer-facing notes. This module is the read-side helper
//! that powers the SPA's three textarea typeaheads (per-line note,
//! per-invoice note, storno reason) by walking the audit ledger and
//! returning the operator's prior note strings most-recent-first.
//!
//! The buyer-facing notes themselves are pinned by
//! `adr/0042-invoice-notes-never-in-nav-xml.md`: the strings live on
//! `InvoiceDraftCreatedPayload::invoice_note` / `line_notes` and never
//! reach NAV. This module reads — never writes — those fields. The
//! NAV firewall stays intact.
//!
//! Per-tenant scoping is implicit: each `Ledger` is bound to a single
//! tenant DB at `Ledger::open` time, so a serve-time call here only
//! sees the calling tenant's notes.
//!
//! Why not a billing-column scan instead? Two reasons. (1) The audit
//! ledger is the regulatory source of truth for issued invoices and
//! is the same source the rest of `serve.rs` already walks for state
//! derivation (`list_invoices`, `get_invoice_detail`). (2) A storno's
//! reason lives on the storno child's own `InvoiceDraftCreated`
//! payload; routing that via SQL would need a join against the chain-
//! link entries, which is exactly the kind of duplication F12 names.
//!
//! See [[aberp-notes-and-email]] memory for the broader-product
//! context this PR fits into.

use aberp_audit_ledger::{Entry, EventKind, Ledger};
use anyhow::{Context, Result};
use serde::Deserialize;

use crate::audit_payloads::InvoiceDraftCreatedPayload;

/// Closed-vocab notes-history scope. Three buckets, one per textarea
/// the SPA exposes. A query against the audit ledger filters to a
/// single scope so the per-line history does not pollute the per-
/// invoice typeahead (and vice versa).
///
/// Wire form is the lowercase kebab string `"line"` / `"invoice"` /
/// `"storno"` — matches what the SPA query-param composer emits.
/// Unrecognised strings fail loud at the route boundary
/// (`from_storage_str` returns `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesHistoryScope {
    /// Per-line buyer note ("Megjegyzés" sub-line on the printed PDF).
    /// Source: `InvoiceDraftCreatedPayload::line_notes[i]` for every
    /// draft entry, regardless of whether the draft is a normal
    /// invoice, a storno, or a modification — line notes are line
    /// notes.
    Line,
    /// Per-invoice buyer note ("MEGJEGYZÉS" block on the printed PDF).
    /// Source: `InvoiceDraftCreatedPayload::invoice_note` for drafts
    /// whose invoice_id is NOT a storno child. Modification children
    /// land here because the operator-typed note IS the document-
    /// level note for that modification (it is not a storno reason).
    Invoice,
    /// Storno reason ("Sztornó indoka" block on the printed PDF).
    /// Source: `InvoiceDraftCreatedPayload::invoice_note` for drafts
    /// whose invoice_id appears as the `storno_invoice_id` field of
    /// some `InvoiceStornoIssued` chain-link entry.
    Storno,
}

impl NotesHistoryScope {
    /// Wire-string for the query parameter. Lowercase kebab — matches
    /// what the SPA composer emits.
    pub fn as_str(&self) -> &'static str {
        match self {
            NotesHistoryScope::Line => "line",
            NotesHistoryScope::Invoice => "invoice",
            NotesHistoryScope::Storno => "storno",
        }
    }

    /// Parse from the query-parameter wire string. Unknown strings →
    /// `None` (the route returns 400 with a typed error rather than
    /// silently coercing to a default scope).
    pub fn from_storage_str(s: &str) -> Option<Self> {
        match s {
            "line" => Some(NotesHistoryScope::Line),
            "invoice" => Some(NotesHistoryScope::Invoice),
            "storno" => Some(NotesHistoryScope::Storno),
            _ => None,
        }
    }
}

/// Default cap on the number of distinct notes returned to the SPA.
/// The brief picks 50 — the operator's typeahead surfaces the top
/// matches client-side; deeper history rarely helps.
pub const DEFAULT_LIMIT: usize = 50;

/// Walk the audit ledger and return the most-recently-used distinct
/// notes for `scope`, up to `limit` entries.
///
/// Ordering: most-recent-first (newest sequence number wins). Within
/// the result, each string appears at most once — duplicates collapse
/// to their most-recent occurrence.
///
/// Dedupe normalisation: leading/trailing whitespace is trimmed before
/// comparison and storage; case is preserved (notes are buyer-facing,
/// and "ÁFA" vs "áfa" are intentionally distinct in Hungarian
/// invoicing).
///
/// Empty / whitespace-only strings are filtered out.
pub fn list_notes_history(
    ledger: &Ledger,
    scope: NotesHistoryScope,
    limit: usize,
) -> Result<Vec<String>> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries for notes-history scan")?;

    // First pass: collect the set of invoice_ids that are storno
    // children. A storno's invoice_note is the storno reason; it MUST
    // route to scope=Storno even though it lives on a draft payload
    // structurally identical to a normal invoice's draft.
    let mut storno_ids: std::collections::HashSet<String> = Default::default();
    for entry in &entries {
        if entry.kind == EventKind::InvoiceStornoIssued {
            if let Some(id) = extract_storno_child_id(entry) {
                storno_ids.insert(id);
            }
        }
    }

    // Second pass: walk drafts in reverse-sequence order (newest first)
    // and collect notes by-scope, deduping on the trimmed string.
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut out: Vec<String> = Vec::new();
    for entry in entries.iter().rev() {
        if out.len() >= limit {
            break;
        }
        if entry.kind != EventKind::InvoiceDraftCreated {
            continue;
        }
        let parsed: InvoiceDraftCreatedPayload = match serde_json::from_slice(&entry.payload) {
            Ok(p) => p,
            // Malformed payload — skip rather than abort the whole
            // typeahead. The audit-evidence chain verifier surfaces
            // tampering separately; here we degrade gracefully so a
            // single corrupt entry never wipes the operator's
            // history dropdown.
            Err(_) => continue,
        };

        match scope {
            NotesHistoryScope::Line => {
                for note in parsed.line_notes.iter().flatten() {
                    if try_collect(note, &mut seen, &mut out, limit) && out.len() >= limit {
                        break;
                    }
                }
            }
            NotesHistoryScope::Invoice => {
                if storno_ids.contains(&parsed.invoice_id) {
                    continue;
                }
                if let Some(note) = parsed.invoice_note.as_deref() {
                    try_collect(note, &mut seen, &mut out, limit);
                }
            }
            NotesHistoryScope::Storno => {
                if !storno_ids.contains(&parsed.invoice_id) {
                    continue;
                }
                if let Some(note) = parsed.invoice_note.as_deref() {
                    try_collect(note, &mut seen, &mut out, limit);
                }
            }
        }
    }

    Ok(out)
}

/// Trim, drop empty, dedup, push. Returns true if the note was
/// admitted (caller may use this to short-circuit a per-line loop
/// once the limit is hit).
fn try_collect(
    note: &str,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<String>,
    limit: usize,
) -> bool {
    if out.len() >= limit {
        return false;
    }
    let trimmed = note.trim();
    if trimmed.is_empty() {
        return false;
    }
    if !seen.insert(trimmed.to_string()) {
        return false;
    }
    out.push(trimmed.to_string());
    true
}

/// Pull the `storno_invoice_id` field out of an `InvoiceStornoIssued`
/// entry's payload. Mirrors the permissive probe pattern in
/// `serve::extract_chain_link` — a payload that fails to decode is
/// skipped rather than blowing up the whole scan.
fn extract_storno_child_id(entry: &Entry) -> Option<String> {
    #[derive(Deserialize)]
    struct Probe {
        storno_invoice_id: String,
    }
    let probe: Probe = serde_json::from_slice(&entry.payload).ok()?;
    Some(probe.storno_invoice_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_payloads::{InvoiceDraftCreatedPayload, InvoiceStornoIssuedPayload};
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};
    use aberp_billing::IdempotencyKey;

    /// Closed-vocab round-trip: every variant's `as_str` parses back
    /// to itself via `from_storage_str`. Pins the wire vocab so a
    /// future variant-rename surfaces here rather than as a silent
    /// SPA / backend wire-shape drift.
    #[test]
    fn notes_history_scope_round_trip_for_every_variant() {
        for variant in [
            NotesHistoryScope::Line,
            NotesHistoryScope::Invoice,
            NotesHistoryScope::Storno,
        ] {
            let wire = variant.as_str();
            let parsed = NotesHistoryScope::from_storage_str(wire)
                .unwrap_or_else(|| panic!("round-trip failed for variant wire={wire}"));
            assert_eq!(parsed, variant, "round-trip mismatch for {wire}");
        }
    }

    /// Unknown wire strings produce `None`. Pins the loud-on-bad-input
    /// posture per CLAUDE.md rule 12.
    #[test]
    fn notes_history_scope_rejects_unknown_wire_strings() {
        assert!(NotesHistoryScope::from_storage_str("").is_none());
        assert!(NotesHistoryScope::from_storage_str("LINE").is_none());
        assert!(NotesHistoryScope::from_storage_str("modification").is_none());
        assert!(NotesHistoryScope::from_storage_str("invoice ").is_none());
    }

    fn fixture_ledger() -> (Ledger, Actor) {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        (ledger, actor)
    }

    /// Append an `InvoiceDraftCreated` entry with operator-supplied
    /// invoice_note / line_notes. The other payload fields default
    /// to the pre-PR-44γ shape — irrelevant to the notes scan.
    fn write_draft_with_notes(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        invoice_note: Option<&str>,
        line_notes: Vec<Option<&str>>,
    ) {
        let idem = IdempotencyKey::new();
        let mut payload = InvoiceDraftCreatedPayload {
            customer_community_vat_number: None,
            invoice_id: invoice_id.to_string(),
            line_count: line_notes.len(),
            idempotency_key: idem.to_canonical_string(),
            nav_xml_path: None,
            currency: None,
            exchange_rate: None,
            exchange_rate_source: None,
            exchange_rate_date: None,
            huf_equivalent_total: None,
            bank_account_id: None,
            bank_account_currency: None,
            bank_account_number: None,
            bank_account_bank_name: None,
            bank_account_swift_bic: None,
            invoice_note: invoice_note.map(|s| s.to_string()),
            line_notes: line_notes
                .into_iter()
                .map(|s| s.map(String::from))
                .collect(),
            payment_deadline: None,
            delivery_date: None,
            delivery_date_override: None,
            customer_vat_status: None,
        };
        // Silence the unused-`mut` warning when the test does not
        // mutate post-construction — the binding stays mut for
        // future tests that want to tweak fields in place.
        let _ = &mut payload;
        let bytes = serde_json::to_vec(&payload).unwrap();
        ledger
            .append(
                EventKind::InvoiceDraftCreated,
                bytes,
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    /// Append an `InvoiceStornoIssued` chain-link entry tagging
    /// `storno_invoice_id` as a storno child of `base_invoice_id`.
    fn write_storno_link(
        ledger: &mut Ledger,
        actor: &Actor,
        storno_invoice_id: &str,
        base_invoice_id: &str,
    ) {
        let idem = IdempotencyKey::new();
        let payload = InvoiceStornoIssuedPayload::new(
            storno_invoice_id,
            42,
            "rsv_test",
            idem,
            base_invoice_id,
            7,
            1,
        );
        ledger
            .append(
                EventKind::InvoiceStornoIssued,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    /// Per-invoice scope returns only invoice_note from drafts that
    /// are NOT storno children. Most-recent-first. Empty / blank
    /// notes are filtered. Duplicate strings collapse.
    #[test]
    fn invoice_scope_orders_most_recent_first_and_dedupes() {
        let (mut ledger, actor) = fixture_ledger();

        write_draft_with_notes(&mut ledger, &actor, "inv_001", Some("Köszönjük"), vec![]);
        write_draft_with_notes(&mut ledger, &actor, "inv_002", Some("Áthozat"), vec![]);
        write_draft_with_notes(&mut ledger, &actor, "inv_003", Some(" Köszönjük "), vec![]);
        write_draft_with_notes(&mut ledger, &actor, "inv_004", Some(""), vec![]);
        write_draft_with_notes(&mut ledger, &actor, "inv_005", None, vec![]);

        let out = list_notes_history(&ledger, NotesHistoryScope::Invoice, DEFAULT_LIMIT).unwrap();
        // inv_003 is most recent and trims to the same string as
        // inv_001; the most-recent occurrence wins, the older copy
        // is dedup-elided.
        assert_eq!(out, vec!["Köszönjük".to_string(), "Áthozat".to_string()]);
    }

    /// Line scope walks every draft's line_notes and returns each
    /// distinct trimmed string. Storno children are not excluded —
    /// line notes are line notes regardless of document kind.
    #[test]
    fn line_scope_returns_distinct_line_notes_across_drafts() {
        let (mut ledger, actor) = fixture_ledger();

        write_draft_with_notes(
            &mut ledger,
            &actor,
            "inv_001",
            None,
            vec![Some("Garancia: 1 év"), None, Some("Helyszíni átadás")],
        );
        write_draft_with_notes(
            &mut ledger,
            &actor,
            "inv_002",
            None,
            vec![Some("Garancia: 1 év"), Some("Új tétel")],
        );

        let out = list_notes_history(&ledger, NotesHistoryScope::Line, DEFAULT_LIMIT).unwrap();
        // Most-recent draft first; inv_002's "Garancia: 1 év" wins
        // dedup against inv_001's older occurrence. inv_002's line
        // order is preserved within the draft.
        assert_eq!(
            out,
            vec![
                "Garancia: 1 év".to_string(),
                "Új tétel".to_string(),
                "Helyszíni átadás".to_string(),
            ]
        );
    }

    /// Storno scope returns invoice_note only for drafts whose
    /// invoice_id appears as a `storno_invoice_id` chain-link.
    /// Non-storno drafts' invoice_note never leaks to the storno
    /// typeahead (and vice versa).
    #[test]
    fn storno_scope_isolates_from_invoice_scope() {
        let (mut ledger, actor) = fixture_ledger();

        write_draft_with_notes(
            &mut ledger,
            &actor,
            "inv_base_001",
            Some("Eredeti számla megjegyzés"),
            vec![],
        );
        write_draft_with_notes(
            &mut ledger,
            &actor,
            "inv_storno_001",
            Some("Téves vevő adatok"),
            vec![],
        );
        write_storno_link(&mut ledger, &actor, "inv_storno_001", "inv_base_001");

        let storno = list_notes_history(&ledger, NotesHistoryScope::Storno, DEFAULT_LIMIT).unwrap();
        assert_eq!(storno, vec!["Téves vevő adatok".to_string()]);

        let invoice =
            list_notes_history(&ledger, NotesHistoryScope::Invoice, DEFAULT_LIMIT).unwrap();
        assert_eq!(invoice, vec!["Eredeti számla megjegyzés".to_string()]);
    }

    /// Empty ledger → empty Vec, no error. First-ever invoice on a
    /// fresh tenant must not loud-fail the typeahead.
    #[test]
    fn empty_ledger_returns_empty_vec() {
        let (ledger, _actor) = fixture_ledger();
        for scope in [
            NotesHistoryScope::Line,
            NotesHistoryScope::Invoice,
            NotesHistoryScope::Storno,
        ] {
            let out = list_notes_history(&ledger, scope, DEFAULT_LIMIT).unwrap();
            assert!(
                out.is_empty(),
                "empty ledger must yield empty vec for scope={:?}",
                scope
            );
        }
    }

    /// `limit` caps the output. A limit of 0 returns no entries.
    #[test]
    fn limit_caps_the_output_length() {
        let (mut ledger, actor) = fixture_ledger();
        for i in 0..10 {
            let note = format!("note-{i}");
            write_draft_with_notes(
                &mut ledger,
                &actor,
                &format!("inv_{i:03}"),
                Some(&note),
                vec![],
            );
        }

        let out = list_notes_history(&ledger, NotesHistoryScope::Invoice, 3).unwrap();
        assert_eq!(out.len(), 3);
        // Most-recent first: note-9, note-8, note-7.
        assert_eq!(
            out,
            vec![
                "note-9".to_string(),
                "note-8".to_string(),
                "note-7".to_string()
            ]
        );

        let zero = list_notes_history(&ledger, NotesHistoryScope::Invoice, 0).unwrap();
        assert!(zero.is_empty());
    }

    /// Case is preserved on dedupe — "ÁFA" and "áfa" stay distinct.
    /// Hungarian invoicing uses both forms with different intent
    /// (heading vs body text); a lowercase-fold would collapse them.
    #[test]
    fn dedupe_preserves_case() {
        let (mut ledger, actor) = fixture_ledger();
        write_draft_with_notes(&mut ledger, &actor, "inv_001", Some("ÁFA-mentes"), vec![]);
        write_draft_with_notes(&mut ledger, &actor, "inv_002", Some("áfa-mentes"), vec![]);

        let out = list_notes_history(&ledger, NotesHistoryScope::Invoice, DEFAULT_LIMIT).unwrap();
        // Most-recent first; both retained.
        assert_eq!(
            out,
            vec!["áfa-mentes".to_string(), "ÁFA-mentes".to_string()]
        );
    }

    /// S192 — multi-line notes (textarea content with embedded `\n`)
    /// MUST round-trip through serde + the dedupe trim path verbatim.
    /// The HU bookkeeping workflow uses two-line `"Számla\nMegjegyzés"`
    /// blocks; an over-eager `trim()` that stripped internal newlines
    /// would silently flatten the typeahead's output to a single line
    /// and the operator would lose visibility into pre-typed multi-
    /// line notes. The PR-172 trim path only touches leading/trailing
    /// whitespace via `str::trim`, but a future "normalise whitespace"
    /// refactor could regress this — this pin makes the embedded-`\n`
    /// preservation contract load-bearing.
    #[test]
    fn multiline_notes_preserve_internal_newlines_through_serde_round_trip() {
        let (mut ledger, actor) = fixture_ledger();
        // Three notes that share leading/trailing whitespace (the trim
        // path strips this) but carry internal `\n` (the trim path
        // MUST leave it alone).
        let note_a = "  Köszönjük\na megrendelést  ";
        let note_b = "Garancia: 1 év\nHelyszíni átadás";
        let note_c = "Áthozat\n2. oldal\n3. oldal";
        write_draft_with_notes(&mut ledger, &actor, "inv_001", Some(note_a), vec![]);
        write_draft_with_notes(&mut ledger, &actor, "inv_002", Some(note_b), vec![]);
        write_draft_with_notes(&mut ledger, &actor, "inv_003", Some(note_c), vec![]);

        let out = list_notes_history(&ledger, NotesHistoryScope::Invoice, DEFAULT_LIMIT).unwrap();
        // Most-recent first; internal newlines preserved; only the
        // outer whitespace on note_a is trimmed.
        assert_eq!(
            out,
            vec![
                "Áthozat\n2. oldal\n3. oldal".to_string(),
                "Garancia: 1 év\nHelyszíni átadás".to_string(),
                "Köszönjük\na megrendelést".to_string(),
            ]
        );
        // Defence pin: every returned note contains at least one `\n`
        // (the multi-line shape survived end-to-end).
        for s in &out {
            assert!(
                s.contains('\n'),
                "embedded newline lost from notes-history scan: {s:?}"
            );
        }
    }

    /// S192 — exact boundary pin on `DEFAULT_LIMIT = 50`. With 51
    /// unique notes in the ledger, a `limit=50` call MUST return
    /// exactly 50 entries — the 50 newest in reverse-sequence order
    /// (`note-50` → `note-01`), with the oldest (`note-00`) elided.
    /// Pre-PR-172 reading suggested this was correct; pinning closes
    /// a future off-by-one regression (e.g., a `<` vs `<=` flip in the
    /// `if out.len() >= limit` guards in `list_notes_history` /
    /// `try_collect`).
    #[test]
    fn limit_boundary_returns_fifty_newest_from_fifty_one_unique_notes() {
        let (mut ledger, actor) = fixture_ledger();
        // 51 distinct invoices, each with a unique invoice_note.
        for i in 0..51 {
            let note = format!("note-{i:02}");
            write_draft_with_notes(
                &mut ledger,
                &actor,
                &format!("inv_{i:03}"),
                Some(&note),
                vec![],
            );
        }

        let out = list_notes_history(&ledger, NotesHistoryScope::Invoice, DEFAULT_LIMIT).unwrap();
        assert_eq!(
            out.len(),
            DEFAULT_LIMIT,
            "fifty entries (the closed boundary at limit=50)"
        );
        // Most-recent first → first element is note-50, last is note-01.
        assert_eq!(out.first().map(String::as_str), Some("note-50"));
        assert_eq!(out.last().map(String::as_str), Some("note-01"));
        // The oldest entry MUST be elided.
        assert!(
            !out.iter().any(|s| s == "note-00"),
            "the 51st-oldest note must be dropped at the limit boundary"
        );
    }
}
