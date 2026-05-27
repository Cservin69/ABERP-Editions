# ADR-0042 — Invoice Notes ("Megjegyzés") Are Recipient-Facing, Never on the NAV XML

**Status:** Accepted — PR-82 (2026-05-27)
**Author:** Ervin Áben (ABERP), session 102
**Supersedes / amends:** none (additive surface)
**Related:** ADR-0037 (EUR invoicing), ADR-0031 (NAV XML on-disk posture), ADR-0040 (multi-bank schema)

## Context

PR-82 introduces buyer-facing notes ("Megjegyzés" — Hungarian for
"Note / Remark", a standard invoice field) at two levels:

- **Per-invoice global note** — one free-text note on the whole invoice.
- **Per-line note** — one free-text note on each invoice line.

Both are optional and fully independent (an invoice can carry only a
global note, only line notes, both, or neither). The triggering
business need (Ervin, 2026-05-27): *"Invoicing is not for NAV but a
regulatory supplement — we send invoices to buyers."* The buyer-facing
artifacts (printed PDF, future SMTP email body) ARE the product; NAV
is the legal layer underneath. Notes are how the operator communicates
with the buyer on the document.

NAV's Online Számla v3.0 `<InvoiceData>` XSD has **no slot** for these
notes. Any element ABERP emits that isn't in the XSD's `ALLOWED` /
`ORDERED_REQUIRED` sets surfaces as a NAV-side `SCHEMA_VIOLATION` —
the exact class of failure PR-76 / PR-77 spent two sessions clearing.

## Decision

**Notes are recipient-facing only. They live in:**

1. **DuckDB storage** — new nullable columns `invoice.invoice_note`
   (`VARCHAR`) and `invoice_line.note` (`VARCHAR`). Migration via
   `MIGRATE_PR_82_SQL` in `modules/billing/src/adapters/duckdb_store.rs`,
   idempotent (`ADD COLUMN IF NOT EXISTS`).
2. **Audit-ledger payload** — added to `InvoiceDraftCreatedPayload`
   as `invoice_note: Option<String>` and `line_notes: Vec<Option<String>>`
   so the operator-twin record of "what was issued" matches the printed
   PDF byte-for-byte. Pre-PR-82 entries deserialise with empty defaults
   via `#[serde(default)]`.
3. **Printed PDF** — rendered under the "MEGJEGYZÉS" block (invoice
   level) and as an italic sub-line beneath the description column
   (per line). The `aberp_invoice_pdf` crate's `InvoiceModel.note` +
   `LineItem.note` fields surface them.
4. **SPA detail view** — rendered read-only in a "Megjegyzés / Note"
   section between the meta-grid and the chain-children list.
5. **(Future PR-83+)** — SMTP email body carries the global note in
   the message text.

**Notes MUST NEVER be emitted into the NAV `<InvoiceData>` XML.**

The load-bearing pin is `apps/aberp/tests/nav_xml_notes_never_leak.rs`:
two assertions per fixture (byte-equal renders with/without notes, and
literal-substring absence). A future code change that wires
`LineItem.note` or `invoice_note` into the renderer trips the pin
loud.

## Why a single load-bearing rule

The NAV submission path is the regulatory critical surface. PR-76 and
PR-77 each cost a full session to recover from a wire-shape mismatch
NAV rejected; PR-82 introduces a new field family with operator-typed
free text (the worst possible content from a SCHEMA_VIOLATION
perspective — unconstrained string content). A single named invariant
("notes never on the wire") with a single load-bearing test
(`nav_xml_notes_never_leak`) closes the failure mode at its root:
the renderer simply does not consume the field.

The alternative — emit notes onto the wire and rely on operator
discipline to not exceed NAV's content rules — was rejected because
NAV's XSD has no slot at all; there is no "safe" form of emit.

## Storage shape (PR-82)

```sql
-- DuckDB invoice table — new column
ALTER TABLE invoice      ADD COLUMN IF NOT EXISTS invoice_note VARCHAR;
-- DuckDB invoice_line table — new column
ALTER TABLE invoice_line ADD COLUMN IF NOT EXISTS note         VARCHAR;
```

Both columns are nullable; pre-PR-82 rows stay NULL (no backfill — a
fabricated empty string would be wire-confusing). The
`load_invoice_note_in_tx` helper (in `aberp_billing`) returns
`Option<String>`; the per-line note rides on the existing
`LineItem.note: Option<String>` field through the standard
`load_ready_invoice_by_id` read path.

## Audit-payload shape (PR-82)

`InvoiceDraftCreatedPayload` gains two `#[serde(default)]` fields:

- `invoice_note: Option<String>`
- `line_notes: Vec<Option<String>>` (parallel to line ordinals;
  `line_notes.len()` matches `line_count` for post-PR-82 entries)

The `with_notes(&invoice, invoice_note)` builder method reads each
`LineItem.note` off the invoice and stamps the payload.

## What this ADR does NOT cover

- **Storno reason** — PR-83 wires the storno-level note (the buyer-
  visible "WHY this invoice was cancelled") on top of PR-82's
  infrastructure. Same never-on-the-wire posture.
- **SMTP email delivery** — a separate PR with a mandatory adversarial
  security review per Ervin's discipline (credential handling,
  TLS enforcement, header-injection vectors).
- **Note formatting** — plain text only. Rich text / markdown is
  named-deferred; the operator can use newlines and the PDF's
  word-wrap (`wrap_note_text`) handles paragraph-length notes.
- **Length cap** — the SPA's `maxlength` attributes are 2000 chars
  (per line) and 4000 chars (per invoice). The backend does not
  enforce these today; if an operational case surfaces a need, the
  cap moves to the preflight validator.

## Consequences

- A new code path (`load_invoice_notes` in `print_invoice.rs`)
  re-joins the NAV XML (regulatory record on disk) with the DuckDB-
  stored notes at print time. The PDF render is no longer
  byte-deterministic from the NAV XML alone — it also depends on the
  DuckDB row. This is a deliberate trade-off: notes ARE in the
  operator-twin's record (audit-ledger payload), so the regulatory
  reconstruction is still possible from `nav_xml + audit_payload`.

- The `LineItem` domain type gains a `note: Option<String>` field
  that propagates through every typestate transition (Draft → Ready →
  Submitted → Finalized → Rejected → Stuck → Abandoned) verbatim.
  Storno's `negate_line` preserves the note (per-line notes are
  recipient-facing metadata, not amount data).

- The audit-payload schema bump uses `#[serde(default)]` rather than
  a new `EventKind` variant per the audit-ledger header's
  "additive field" rule. Pre-PR-82 entries deserialise with empty
  defaults; the chain hash remains valid.

## Adversarial review (PR-82)

| Risk | Mitigation |
| --- | --- |
| Notes leak into NAV XML, causing SCHEMA_VIOLATION at submission | `nav_xml_notes_never_leak.rs` — byte-equal renders + substring absence, two assertions per fixture, three test cases (full, none, partial line notes) |
| DuckDB migration breaks pre-PR-82 DBs | `notes_migration.rs` — old-schema fixture + `ensure_schema()` + assert columns added + pre-PR-82 row survives intact + idempotent re-run |
| Per-line note ordinal drift between NAV XML and DuckDB | Index-paired read in `print_invoice::render_to_bytes` — NAV line N's amounts pair with `invoice_line.note WHERE ordinal = N`; line write order is the regulatory wire order (ascending ordinal at INSERT time) |
| Audit-payload schema bump invalidates pre-PR-82 entries | `#[serde(default)]` on both new fields; round-trip through `serde_json::to_vec` / `from_slice` preserves the empty shape |
| Operator-typed note containing XML control chars / non-UTF-8 / extreme length | DuckDB `VARCHAR` columns handle arbitrary bytes; the PDF renderer's `wrap_note_text` is naive on whitespace and tolerates anything; the SPA's `maxlength` is the only soft cap (2000 / 4000 chars). Backend length cap is named-deferred per CLAUDE.md rule 2 (no speculative validation) |
| Storno / modification chains carrying forward base's line notes silently | `negate_line` in `nav_xml.rs` preserves `note` verbatim. PR-82 documents this as the intended behaviour (per-line notes are line content, not amount data); a future PR can add chain-level note editing if an operational need surfaces |

## How PR-83 will plug in

PR-83 ("storno reason") reuses PR-82's infrastructure verbatim:

- `IssueStornoRequest` gains an optional `storno_reason: Option<String>`
  on the wire body.
- `issue_storno::run_single_tx` calls `with_notes(&storno_invoice, storno_reason.as_deref())`
  on the audit payload (same builder PR-82 wired for the fresh-issuance path).
- The storno's `invoice.invoice_note` column carries the reason verbatim.
- The printed PDF for the storno renders "MEGJEGYZÉS" with the reason text.
- NAV XML stays untouched (same invariant; the `nav_xml_notes_never_leak`
  pin trips on a regression at either issue or storno path).

No new fields, no new migration, no new audit-payload variant. PR-83
becomes a small surgical change on top of PR-82's foundation.
