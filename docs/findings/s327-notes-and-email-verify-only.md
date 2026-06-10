# S327 / PR-27 — invoice notes + SMTP email: VERIFY-ONLY

**Verdict: SHIPPED.** All five MUST-priority components of
`project_aberp_notes_and_email` are already live on `main` (`e88a7f0`)
and in production at `PROD_v2.27.13`. The re-cut is **REFUSED** per
HARD RULE #2 (verify before implementing) and the verify-only
precedent set by S320/S321/S322/S326. No new code, no `PROD_v2.27.14`.

The shipped implementation is *more* complete than the brief sketch:
the brief explicitly defers notes-history typeahead to backlog, yet
that too is already shipped (PR-172, `apps/aberp/src/notes_history.rs`).

## Component-by-component verdict

### 1. Per-line-item notes (Megjegyzés) — SHIPPED (PR-82)
- Domain field `LineItem.note: Option<String>`, propagated verbatim
  through every typestate transition (ADR-0042).
- Audit payload `InvoiceDraftCreatedPayload.line_notes: Vec<Option<String>>`
  ([audit_payloads.rs:205](apps/aberp/src/audit_payloads.rs:205)),
  stamped by `with_notes` ([audit_payloads.rs:446](apps/aberp/src/audit_payloads.rs:446)).
- PDF: per-line italic "Megjegyzés:" sub-line
  ([invoice-pdf/src/lib.rs:694](crates/invoice-pdf/src/lib.rs:694),
  field `crates/invoice-pdf/src/model.rs:132`).
- Additive `#[serde(default)]` schema bump — no new EventKind, chain
  hash preserved (ADR-0042 "additive field" rule).

### 2. Per-invoice global note — SHIPPED (PR-82)
- `InvoiceDraftCreatedPayload.invoice_note: Option<String>`
  ([audit_payloads.rs:198](apps/aberp/src/audit_payloads.rs:198)).
- PDF: MEGJEGYZÉS block after line-items, before totals
  ([invoice-pdf/src/lib.rs:427](crates/invoice-pdf/src/lib.rs:427)).

### 3. Storno reason as buyer-facing note — SHIPPED (PR-83)
- `IssueStornoRequest.storno_reason: Option<String>`
  ([issue_storno.rs:239](apps/aberp/src/issue_storno.rs:239)),
  persisted on the storno child's own `invoice.invoice_note`
  ([issue_storno.rs:367](apps/aberp/src/issue_storno.rs:367)) — matches
  the storno-chain modeling the brief asked for.
- Rendered on the storno PDF via the same note path; **never** in NAV XML.

### 4. SMTP email delivery, default-ON + per-invoice opt-out — SHIPPED (PR-92 / PR-99 / S184 / PR-203)
- Module `apps/aberp/src/email_invoice.rs` (1130 lines), `lettre`
  async transport, `aberp.smtp.prod` SPOC via
  `secrets_cache` + `smtp_config` (no second keychain).
- `SendTrigger::{AutoOnIssue, Manual}`
  ([email_invoice.rs:219](apps/aberp/src/email_invoice.rs:219)).
- Default-ON, opt-out on all three issuance paths via
  `.unwrap_or(true)`:
  - issue: `email_buyer_on_issue` ([serve.rs:5635](apps/aberp/src/serve.rs:5635))
  - storno: `email_buyer_on_storno` ([serve.rs:6767](apps/aberp/src/serve.rs:6767))
  - modification: `email_buyer_on_modification` ([serve.rs:7162](apps/aberp/src/serve.rs:7162))
- SPA: default-`true` opt-out checkbox ([api.ts:640](apps/aberp-ui/ui/src/lib/api.ts:640)).
- EventKind `InvoiceEmailedSent` (success AND failure, `auto: bool`
  discriminator) — `crates/audit-ledger/src/entry/event_kind.rs:561`.
- Manual route `POST` send + `SendTrigger::Manual`
  ([serve.rs:15388](apps/aberp/src/serve.rs:15388)).

### 5. Mandatory adversarial security review — SHIPPED (PR-93 / ADR-0047)
The brief's demanded review was already performed as **PR-93** and
codified in [adr/0047-smtp-email-delivery-security.md](adr/0047-smtp-email-delivery-security.md).
Every concern the brief enumerates is closed in source + pinned by test:

| Brief concern | Shipped mitigation | Pin |
| --- | --- | --- |
| Credential leakage in logs | password `Zeroizing<String>` keychain→lettre, no Debug/Display | `pr_93_email_send_error_display_carries_no_credentials` |
| TLS enforcement, no plaintext fallback | `SmtpSecurity::{StartTls,Tls}` only; no cleartext path | `build_transport_source_has_no_plaintext_fallback`, `pr_93_no_tls_validation_bypass_tokens_in_source`, `pr_93_only_one_transport_constructor_call_site` |
| Email header injection | `validate_no_crlf` on recipient/display/subject before lettre | `pr_93_validate_no_crlf_rejects_unicode_line_separators`, `pr_93_is_forbidden_header_byte_pins_exact_set` |
| Attachment path traversal | `sanitize_invoice_number_for_filename` ASCII-allowlist | `sanitize_invoice_number_rejects_path_traversal`, `pr_93_sanitize_invoice_number_fuzz_corpus` |
| Send-to-wrong-recipient | partner email is ONLY To source; `MissingRecipient` refusal, no fallback | wrong-recipient guard, `email_recipient_override.rs` |
| Idempotency / replay | `IdempotencyKey` on send | `fresh_send_idempotency_key` |

23 dedicated security tests in `email_invoice.rs`.

## NAV firewall — confirmed (the brief's #1 "Don't")
Notes and storno reason are **never** emitted into NAV XML. Dedicated
regression suite [nav_xml_notes_never_leak.rs](apps/aberp/tests/nav_xml_notes_never_leak.rs)
(380 lines) pins this for BOTH issue and storno paths:
- `nav_xml_invoice_data_byte_identical_with_or_without_notes`
- `nav_xml_invoice_data_never_contains_note_sentinel_text`
- `nav_xml_invoice_data_byte_identical_with_partial_line_notes`
- storno-emit sentinel cases (PR-83-sentinel-storno-INDOKA)

This is exactly the coverage the brief's requested `s327_notes_never_in_nav_xml`
/ `s327_storno_reason_never_in_nav_xml` tests would provide. Adding
`s327_`-named duplicates over comprehensive existing coverage would
violate CLAUDE.md #3 (surgical) and #13 (don't duplicate) — not done.

## Test mapping (brief s327_* → shipped equivalent)
| Brief test | Shipped equivalent |
| --- | --- |
| `s327_invoice_notes_round_trip_in_db` | `draft_created_with_notes_round_trips_storno_reason_and_line_notes` |
| `s327_notes_render_in_pdf` | invoice-pdf render tests (note blocks) |
| `s327_notes_never_in_nav_xml` | `nav_xml_invoice_data_never_contains_note_sentinel_text` |
| `s327_storno_reason_round_trip_and_pdf` | issue_storno round-trip + storno PDF |
| `s327_storno_reason_never_in_nav_xml` | storno-emit sentinel cases (nav_xml_notes_never_leak) |
| `s327_email_*` (12) | 23 `email_invoice.rs` security/behaviour tests + `email_recipient_override.rs` |
| `s327_email_header_injection_safe_against_crlf` | `pr_93_validate_no_crlf_rejects_unicode_line_separators` |
| `s327_email_tls_enforced_no_plaintext_fallback` | `build_transport_source_has_no_plaintext_fallback` |

## Out of scope (confirmed deferred, NOT implemented)
- Notes-history autocomplete — brief says backlog; ALSO already shipped
  (PR-172, `notes_history.rs`), so no action either way.
- Backend note length cap — named-deferred in ADR-0042 (SPA `maxlength`
  is the soft cap); not added (CLAUDE.md #2, no speculative validation).
- CLI `--reason` flag for storno — named-deferred in PR-83 (SPA-only
  affordance today).

## Conservative calls
- **REFUSED the re-cut.** Per `[[feedback-no-ask-user-question]]` I did
  not ask; the most-reversible action when everything is already shipped
  is to ship nothing and document. No `PROD_v2.27.14`.
- Did not add redundant `s327_`-named tests (CLAUDE.md #3/#13).
- Branch `session-327/pr-27-notes-and-email` carries this doc only.
