# ADR-0024 — Modification (MODIFY) chain — operator surface, chain-link allocator, audit-payload pin, three-way operation detector (ADR-0009 §6 amendment)

- **Status:** Accepted
- **Date:** 2026-05-21
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — extends ADR-0009 §6 with
  the concrete pins PR-11 needs in order to land MODIFY code without
  re-litigating naming, allocator semantics, or audit-payload shape.
  Same shape as ADR-0023 vs STORNO. Does **not** supersede ADR-0009;
  the §6 decisions there (a modification is itself an invoice; STORNO
  and MODIFY share `manageInvoice` shape; sequence numbers never reused;
  chain link is ULID-keyed without cross-table FK) remain in force.
- **Related:**
  - **ADR-0009 §6** (storno + modification chain — the surface this
    ADR pins for build, MODIFY half).
  - **ADR-0023** (storno chain amendment — the structural template
    this ADR mirrors and the source of the `modification_index`
    allocator semantics MODIFY now extends).
  - **ADR-0009 §2** (invoice state machine — `Amended` side path off
    `Finalized`; symmetric to `Storno`).
  - **ADR-0009 §3** (sequence allocator — MODIFY invoices consume
    sequence slots same as STORNO).
  - **ADR-0009 §5** (idempotency Layer 1 + Layer 2 — both apply to the
    modify command unchanged).
  - **ADR-0009 §8** (audit-evidence retention — per-invoice export
    bundle traverses MODIFY-chain entries by the same
    `base_invoice_id` ULID traversal as STORNO).
  - **ADR-0008** (audit ledger — typed `EventKind`, the F12 closed-
    set decoder, the per-payload typed struct discipline).
  - **ADR-0019** (no foreign keys — chain link is ULID-by-payload).
  - **ADR-0020 §1, §2** (NAV environment explicit on the CLI).
  - **ADR-0022** (NAV runtime XSD validator — the modify's
    `<InvoiceData>` walks the same on-disk validation gate; the
    `walk_invoice_reference` allowlist is extended in PR-11 to allow
    the optional `<modificationIssueDate>` MODIFY adds — see §2).
  - Session 14 handoff finding **F22** (`detect_operation_from_xml`
    two-way classifier needs a third arm when MODIFY lands — closed
    by this ADR's §3).
- **Source material:** `docs/research/nav-and-billingo.md` §Storno
  and modification.

## Context

ADR-0023 pinned the STORNO half of ADR-0009 §6 build-ready. PR-10
landed the storno code surface (variant + payload + emitter +
detector + orchestration). Session 14's handoff names PR-11's most
natural next surface as the **MODIFY** half: a real-world invoice
correction (price changed, line removed, customer detail wrong) is
more likely to need MODIFY than a technical annulment (which is for
NAV-side data errors, rare in practice). Session 14's handoff also
records finding **F22** — the `submit-invoice` body classifier
implemented in PR-10 is a two-way (Create vs Storno) classifier that
keys off `<invoiceReference>` presence; MODIFY also carries
`<invoiceReference>` and needs a third disambiguator.

This ADR closes the MODIFY-side pins so PR-11 can land code without
re-litigation. The ADR-0023 §1–§5 structure transfers directly; the
MODIFY-specific deltas are surfaced in §1, §2, §3 below.

### Surfaced conflicts (CLAUDE.md rule 7)

Two ambiguities the build phase will otherwise paper over:

1. **Whether `<modificationIssueDate>` is positioned inside
   `<invoiceReference>` or as a separate sibling at a higher level.**
   The research file (`docs/research/nav-and-billingo.md` §"Storno
   and modification") groups it with `<modifyWithoutMaster>` and
   `<modificationIndex>` in one phrase about `<invoiceReference>`'s
   contents, which reads as "inside the block". The NAV v3.0 XSD
   itself is not vendored in this repo; verifying the exact position
   requires a NAV-testbed POST. PR-11 commits to **inside
   `<invoiceReference>` as an optional child, positioned between
   `<originalInvoiceNumber>` and `<modifyWithoutMaster>`** — the
   research-doc grammatical reading. If NAV's testbed rejects, the
   amendment is mechanical (move the element + update the validator's
   allowed-children list); the chain semantics this ADR pins do not
   change. **Named trigger for verification:** the first PR exercising
   `aberp submit-invoice` against NAV's `api-test` endpoint with a
   MODIFY body (the existing live-test scaffolding in
   `apps/aberp/tests/submit_invoice_live.rs`'s shape).

2. **Whether MODIFY's base-state precondition is `Finalized` only or
   also accepts a prior `Amended` base** (i.e. is MODIFY-after-MODIFY
   permitted by default?). NAV's API permits a chain of
   modifications; the accountant convention question is open in the
   same shape as ADR-0023 §7's storno-of-a-storno question. **PR-11
   default: permit** (matches NAV API permissiveness, matches
   ADR-0023's posture for the analogous storno-of-a-storno case).
   See §6.

## Decision

### 1. Operator CLI surface for MODIFY

**Subcommand name:** `aberp issue-modification`.

**Rationale for the verb.** Parallels `aberp issue-storno` from
ADR-0023 §1 — the same verb family for the same shape of operation.
`issue-modification` (not `modify`, not `amend-invoice`, not
`correct-invoice`) because:

- A modification is itself an invoice (ADR-0009 §6), parallel to a
  storno; the verb `issue-*` is the consistent CLI family signal that
  the command burns a sequence slot and produces a fresh invoice.
- `modify` as a bare verb collides with the operator-facing notion of
  editing a stored row; the audit-bearing operation is "issue a new
  modification invoice", not "edit the base".
- `amend-invoice` reads as a sentence rather than a CLI verb +
  object; `issue-modification` matches `issue-invoice` /
  `issue-storno` parallelism that operators reading `aberp --help`
  will pattern-match on.

**Argument shape** (clap-flavoured, mirrors `issue-storno`):

| Flag | Type | Default | Purpose |
|---|---|---|---|
| `--references` | `String` (prefixed `inv_<ULID>`) | none (required) | The base invoice this modification corrects. Same shape + role as `issue-storno --references`. Must be in a chain-positive state per §6 below (Finalized or already-Amended by default; not Storno, not Aborted, not Abandoned). |
| `--in` | `PathBuf` (JSON spec) | none (required) | The modification's own line content. Same JSON shape as `issue-invoice --in` / `issue-storno --in`; PR-11's MODIFY semantics are **full-replace**: the modification carries the complete corrected invoice body, NOT a delta against the base. NAV accepts both delta and full-replace shapes; full-replace is simpler for the operator and matches ABERP's audit posture (the modification is auditable on its own bytes without needing the base's bytes to interpret). See §4. |
| `--out` | `PathBuf` | none (required) | Path to write the modification's `<InvoiceData>` XML. Same on-disk validator gate as `issue-invoice`/`issue-storno`. |
| `--db` | `PathBuf` | `./aberp.duckdb` | Tenant DuckDB. |
| `--tenant` | `String` | `"default"` | Tenant identifier. |
| `--series` | `String` | `"INV-default"` | Series for the modification's own sequence number. Default = same series as the base (ADR-0023 §1 carries forward; the override-path note in `apps/aberp/src/issue_storno.rs` applies verbatim to MODIFY). |
| `--modification-date` | `String` (`YYYY-MM-DD`) | none (required) | The date the modification was issued. NAV's `<modificationIssueDate>` field — distinct from the base invoice's `<invoiceIssueDate>` (which travels through unchanged from the operator's `--in` JSON). Operator-required: silently defaulting to "today" hides the case where an accountant is filing a backdated correction with explicit dates. CLAUDE.md rule 12 (fail loud) + rule 4 (no hidden defaults on audit-bearing fields). The value is parsed via `time::Date::parse` and rejected if not a valid `YYYY-MM-DD`. |

**What `issue-modification` does NOT do.** Same posture as
`issue-storno` per ADR-0023 §1. It does not call NAV. It walks the
same allocator path under one DuckDB transaction (ADR-0009 §3),
writes the modification's own `<InvoiceData>` XML on disk via
ADR-0022's runtime validator, and writes the chain-link audit entry
(§2). The operator's next step is `aberp submit-invoice
--invoice-xml <modification.xml> --invoice-id <modification-id>
--endpoint {test|production}` — the existing wire path, with
`detect_operation_from_xml` extended to recognize MODIFY (§3).

**Technical annulment remains distinct** — re-asserted from ADR-0023
§6. PR-11 does NOT add `request-technical-annulment`. That subcommand
calls a different NAV endpoint (`manageAnnulment` vs `manageInvoice`)
and consumes the `invoice.technical_annulment_requested` audit kind
already named in ADR-0009 §2. Its trigger is a future PR (PR-12+),
not PR-11.

### 2. EventKind variant + on-disk storage form + XSD validator extension

**New EventKind variant:** `EventKind::InvoiceModificationIssued`.

**Storage form:** `"invoice.modification_issued"`. Same dot-separated
`invoice.` prefix convention as every other lifecycle kind; the
existing `invoice.*` glob the per-invoice export bundle (ADR-0009 §8)
will use picks this up alongside `invoice.storno_issued`.

**No second variant added** — same posture as ADR-0023 §2 for STORNO.
The modification's submit/poll-ack/retry events reuse the existing
variants. The base invoice does NOT get a new ledger entry from the
modification issuance; its derived typestate (`Finalized → Amended`
per ADR-0009 §2) is observed by reading the existence of a successful
`InvoiceModificationIssued` entry whose `base_invoice_id` points at
it. ADR-0023 §2's "no second source of truth for the same fact"
posture transfers.

**XSD validator extension.** The
`crates/nav-xsd-validator/src/validate.rs::walk_invoice_reference`
allowlist (added in PR-10 for STORNO) is extended to allow the
optional `<modificationIssueDate>` child of `<invoiceReference>`.

- ALLOWED set adds `"modificationIssueDate"`.
- ORDERED_REQUIRED set is UNCHANGED — `<modificationIssueDate>` is
  **optional** (STORNO bodies do not carry it; MODIFY bodies do).
  Same posture as ADR-0023 §4-named A40 (the validator's
  `check_ordered_required` only projects onto the required set, so
  optional children may appear at any position relative to the
  required ones).
- The validator's position-tolerance is the inspector-bar-relevant
  trade-off: a NAV inspector reading a hand-rolled XML might expect
  `<modificationIssueDate>` in a specific position. ABERP's emitter
  writes it in the correct position (between `<originalInvoiceNumber>`
  and `<modifyWithoutMaster>` per §1 conflict 1); a future
  tightening (require `<modificationIssueDate>` before
  `<modifyWithoutMaster>` when present) would be an explicit
  decision, not a silent regression. Loud comment in
  `walk_invoice_reference` names this — same comment shape ADR-0023
  §4-A40 uses for `<invoiceReference>` position-tolerance.

### 3. Three-way operation detector (closes F22)

`apps/aberp/src/submit_invoice.rs::detect_operation_from_xml` is
extended from a two-way classifier (Create | Storno) to a three-way
classifier (Create | Modify | Storno) per F22.

**Disambiguator:** the presence of `<modificationIssueDate>` in the
body.

| Body shape | `InvoiceOperation` |
|---|---|
| No `<invoiceReference>` | `Create` |
| Contains `<invoiceReference>` AND contains `<modificationIssueDate>` | `Modify` |
| Contains `<invoiceReference>` AND does NOT contain `<modificationIssueDate>` | `Storno` |

The detector is a contains-check on string substrings — deterministic
code, no LLM (CLAUDE.md rule 5). The opening tag is matched bare
(`<modificationIssueDate>`) per the emitter's no-attribute convention,
same posture as ADR-0023 §4-A39 for `<invoiceReference>`.

**Why presence-of-date as the discriminator (rather than, e.g., a
SOAP-envelope hint).** The `<InvoiceData>` body is the only artifact
that `submit-invoice` reads from disk; the SOAP envelope is built at
submit time inside `nav-transport` and would couple the detector to
upstream call-site state. Keeping the discriminator in-body matches
the PR-10 STORNO detector's posture: the body shape on disk is
self-describing, and the wire `operation` field is derived from that
shape rather than carried as a separate flag. The single-source-of-
truth property is what closes the failure mode where the operator's
intent and the body's shape disagree — there is no "intent" field
separate from the body bytes.

**F22 is closed by this ADR's enactment in PR-11.** The detector's
three-arm extension lands together with the three new detector unit
tests (Create, Storno, Modify) per the F12 four-edit-ritual
discipline (test list extension is one of the four sub-edits in the
ritual, see ADR-0023 §3).

### 4. Modification body — full-replace, not delta

**Decision:** the modification's `<InvoiceData>` carries the **full
corrected invoice body**, NOT a delta against the base.

**Rationale.**

- **Auditable on its own bytes.** ADR-0009 §8's per-invoice export
  bundle reconstructs the chain by walking `InvoiceModificationIssued`
  payloads' `base_invoice_id` links; each modification's XML is
  self-contained. A delta-shape would require the bundle reader to
  also load the base to render the modification's effective body, a
  second-source-of-truth pattern that ADR-0023 §2 + ADR-0008
  consistently rule out.
- **NAV accepts both shapes.** NAV's `manageInvoice` with
  `operation=MODIFY` accepts a full-replace body (the modification is
  treated as the new effective invoice; `<invoiceReference>` plus
  `<modificationIndex>` link it to the chain). Delta-shapes are
  permitted at the line level via `<lineModificationReference>` but
  ABERP does not consume them in PR-11.
- **Operator surface clarity.** `aberp issue-modification --in
  <full_corrected.json>` is parallel to `aberp issue-invoice --in
  <fresh.json>` — the JSON spec is the complete invoice body either
  way. A delta-mode would require a second JSON shape, which is the
  CLAUDE.md rule 2 speculative-abstraction trap.

**What this locks ABERP into.** The first PR that needs line-level
delta semantics (a customer who returns 2 of 5 widgets while keeping
the remaining 3 unchanged) is when `<lineModificationReference>`
lands. The trigger is named: first PR that adds an operator-facing
"partial return" or "partial credit" surface. Until then, the
operator who wants to amend three lines submits a modification
carrying all eight lines of the corrected invoice. The audit ledger
makes the chain visible; the inspector reading the chain sees the
correction without ambiguity.

**Line amounts are NOT negated** — this is a key contrast with
STORNO. The modification's line amounts are the **new effective
values**; the modification carries the corrected invoice as it
should be, not the delta. (A storno carries `-1 × original` per
ADR-0023 §4-A38; a modification carries the new value.) The emitter
`render_modification_data` therefore reuses `write_lines` /
`write_summary` against the input invoice's lines directly — no
parallel `negate_line` call.

### 5. Typed payload struct + the F12 four-edit ritual

**Payload type name:** `InvoiceModificationIssuedPayload` in
`apps/aberp/src/audit_payloads.rs`. Trailing `Payload` per the
convention every other typed payload follows; same constructor
discipline (`new(...)` rather than `from_outcome(...)` per ADR-0023
§3 — fields cross multiple domain types).

**Field shape:**

```rust
pub struct InvoiceModificationIssuedPayload {
    /// The modification's own invoice id — prefixed `inv_<ULID>`.
    pub modification_invoice_id: String,
    /// The modification's own sequence number.
    pub modification_seq: u64,
    /// The modification's own sequence-reservation id.
    pub modification_reservation_id: String,
    /// The idempotency key of the `IssueModificationCommand`.
    pub idempotency_key: String,
    /// The **base invoice's** id — prefixed `inv_<ULID>`. Chain link
    /// (ULID-keyed, ADR-0019).
    pub base_invoice_id: String,
    /// The **base invoice's** NAV-facing sequence number. Denormalized
    /// by the same posture as `InvoiceStornoIssuedPayload::base_sequence_number`
    /// — drift guarded by ADR-0023 §4's integrity-scan extension.
    pub base_sequence_number: u64,
    /// `<modificationIndex>` for this modification's chain position.
    /// Allocator: same `max(chain index) + 1` rule as ADR-0023 §4 —
    /// extended to walk BOTH `InvoiceStornoIssued` AND
    /// `InvoiceModificationIssued` payloads against the same
    /// `base_invoice_id`. See §7.
    pub modification_index: u32,
    /// The operator-supplied `<modificationIssueDate>` in `YYYY-MM-DD`
    /// form. NAV-required for MODIFY; absent on STORNO. Captured as
    /// `String` (not `time::Date`) because the audit payload's
    /// serialization shape is the canonical record and a typed-time
    /// wrapper would force serde-with adapters — CLAUDE.md rule 2
    /// (no speculative abstractions). Validation that the string is
    /// `YYYY-MM-DD`-shaped happens at the CLI boundary (§1).
    pub modification_issue_date: String,
}
```

The `to_bytes(&self) -> Vec<u8>` shape matches every other payload.

**The F12 four-edit ritual** (carries forward from ADR-0023 §3 with
one additional sub-edit on the validator):

| # | File | Edit |
|---|---|---|
| 1 | `crates/audit-ledger/src/entry/event_kind.rs` | Add `InvoiceModificationIssued` variant + `as_str` arm (storage form `"invoice.modification_issued"`) + `from_storage_str` arm + extend the `round_trip_for_every_variant` test's variant list. Same four sub-edits as ADR-0023 §3's row 1; F12 closed-set discipline. |
| 2 | `apps/aberp/src/audit_payloads.rs` | New `InvoiceModificationIssuedPayload` struct + `new(...)` + `to_bytes(&self)` + two round-trip unit tests (one happy-path; one with `modification_issue_date` carrying boundary-shape input). |
| 3 | `apps/aberp/src/cli.rs` | New `Command::IssueModification(IssueModificationArgs)` variant + the `IssueModificationArgs` struct per §1 above. |
| 4 | `apps/aberp/src/issue_modification.rs` | New file — `run` + `run_single_tx` mirroring `issue_storno.rs`'s shape, with the MODIFY-specific delta (no negation; chain-walk widened to include both `InvoiceStornoIssued` and `InvoiceModificationIssued` entries; payload uses the new typed shape). |

**Plus three derivative edits in PR-11 that consequence-of-the-above
covers (NOT a five-edit ritual extension — these are mechanical):**

- `apps/aberp/src/lib.rs`: `pub mod issue_modification;`.
- `apps/aberp/src/main.rs`: dispatch arm
  `cli::Command::IssueModification(a) => issue_modification::run(&a),`.
- `apps/aberp/src/submit_invoice.rs`: extend `detect_operation_from_xml`
  to three-way per §3 + extend the existing two detector unit tests
  to a three-arm test list with one new MODIFY arm.

And one XSD validator edit per §2:

- `crates/nav-xsd-validator/src/validate.rs::walk_invoice_reference`:
  add `"modificationIssueDate"` to the ALLOWED list (NOT to
  ORDERED_REQUIRED — it is MODIFY-only and optional from the
  validator's perspective).

### 6. Idempotency and base-state precondition for `issue-modification`

**Idempotency.** Both layers of ADR-0009 §5 apply unchanged from
ADR-0023 §5's storno posture. Layer 1 (client-side ULID) on the
`IssueModificationCommand`; Layer 2 (NAV-side) does not fire for
`issue-modification` directly (the modification's own submission goes
through `submit-invoice` and inherits §5 Layer 2).

**Cross-modify behaviour.** If the operator runs
`issue-modification --references inv_A` twice without retrying the
same command, the second invocation produces a **second**
modification against `inv_A` with `modification_index = max + 1`.
NAV accepts this. The same accountant-policy open question that
ADR-0023 §5 + §7 names for storno-of-a-storno applies symmetrically
to modify-after-modify; default until accountant resolves: permit.

**Base-state precondition.** PR-11's
`issue_modification::run` precondition walker accepts:

- Base in `Finalized` (most-recent `InvoiceAckStatus` for the base is
  `"SAVED"`).
- Base already in `Amended` (a prior `InvoiceModificationIssued`
  payload pointing at the same `base_invoice_id` exists; chain is
  positive). This is the modify-after-modify case.

And **loud-rejects**:

- Base never submitted (no `InvoiceSubmissionResponse`).
- Base in `Stuck` (most-recent ack is `RECEIVED` or `PROCESSING`).
- Base in `Rejected` (most-recent ack is `ABORTED`).
- Base in `Abandoned` (an `InvoiceMarkedAbandoned` exists).
- Base in `Storno` (an `InvoiceStornoIssued` payload pointing at the
  base exists — modifying a base that has already been legally
  cancelled is malformed; the operator should issue a fresh corrective
  invoice instead).

Each rejection produces a named-reason error message per CLAUDE.md
rule 12 — same shape as `issue_storno::check_base_is_finalized`'s
error texts.

**Why "no MODIFY of a Storno-base" by default.** The storno legally
cancels the base; a subsequent modification against the storno-cancelled
base is the wrong operation (the operator wants a fresh new invoice
or a modification against the **storno's own** number). Surfaced
loudly rather than silently permitted; the accountant question of
"may an operator MODIFY a base that has been stornoed" is filed as
an open question (§8) with default-reject.

**Note: MODIFY against a Storno's own invoice number is permitted by
NAV.** The research file's open question 12 names this case. PR-11
permits it implicitly: a Storno is itself an invoice with its own
`Finalized → SAVED` audit trace, so `issue-modification --references
<storno_invoice_id>` walks the same precondition path as any other
Finalized base. The CLI does not need a separate flag.

### 7. `modification_index` allocator — widened walk

The allocator semantics from ADR-0023 §4 are extended:

**Rule (extended).** The `modification_index` for a new modification
against a base invoice is `max(existing chain indices) + 1`, where
"existing chain indices" walk **BOTH**:

- All `InvoiceStornoIssuedPayload::modification_index` values whose
  `base_invoice_id` equals the new modification's target base.
- All `InvoiceModificationIssuedPayload::modification_index` values
  whose `base_invoice_id` equals the same base.

The same-transaction discipline is preserved. PR-11's
`next_modification_index_in_tx` does TWO `SELECT seq, payload FROM
audit_ledger WHERE kind = ?;` queries inside the modification's own
transaction — one for each event kind — and takes the overall max.
(An alternative single-query `WHERE kind IN (?, ?)` would be tighter
SQL but the two-query shape keeps the per-kind decode loops
homogeneous; if a third chain kind ever appears, the iteration list
extends. CLAUDE.md rule 2.)

**Why widen the walk vs PR-10's storno-only walk.** PR-10's allocator
only considered `InvoiceStornoIssued` entries because no other chain
kind existed. PR-11's MODIFY introduces a second chain kind; under
NAV's uniqueness rule ("`modificationIndex` is unique per
`invoiceReference`", per `docs/research/nav-and-billingo.md`), the
chain index must be globally unique across STORNO + MODIFY entries
against the same base. A storno-only walk would re-issue an index
already used by a prior MODIFY; NAV would reject with
`INVOICE_NUMBER_NOT_UNIQUE`-shape; the rejection would surface only
at submit time, far from the allocator. Widening the walk closes the
failure mode at the allocator.

**Storno's allocator is widened too** — `issue_storno.rs`'s
`next_modification_index_in_tx` is extended to also walk
`InvoiceModificationIssued` entries against the same base. (Surfaced
as a PR-11 sub-edit; the symmetric closure of the same uniqueness
property.) The two allocator functions are intentionally NOT
extracted into a shared `chain_allocator` module yet — that would be
the speculative-abstraction trap (CLAUDE.md rule 2); each function
walks two kinds with a clear name, and a third chain kind appearing
(e.g. ADR-0009 §6's technical-annulment chain, if NAV ever treats
annulment as chain-affecting — which it does not today) is the
extraction trigger.

**Migrated-from-Billingo base invoices.** ADR-0023 §4's
`queryInvoiceChainDigest` path applies symmetrically to MODIFY.
Deferred under the same trigger (ADR-0010 build phase migration
read); F23 (open at end of session 14) extends to MODIFY too. PR-11
does NOT add the `queryInvoiceChainDigest` call; the local-base
allocator path is what lands.

### 8. Open questions

These are **not** changed by this ADR; carried forward for visibility:

- **Modify-of-a-storno-base accountant practice.** Default until
  resolved: reject loudly (§6). The data model supports either; if
  the accountant resolves to permit, the precondition walker drops
  the `Storno`-base rejection branch.
- **Modify-after-modify chain length cap.** NAV does not document a
  ceiling on chain length; common practice tops out at single-digit
  chains for any given base. No cap pinned in PR-11; surfaced as a
  reconciliation-anomaly threshold if a future audit shows chains
  growing without operator awareness.
- **Whether `<modificationIssueDate>` lives inside `<invoiceReference>`
  or as a sibling at a higher level.** Default reading: inside
  `<invoiceReference>` (§1 conflict 1). Verification deferred to first
  NAV-testbed MODIFY POST.
- **Whether MODIFY's full-replace body should keep the base's line
  ids visible** (NAV's `<lineModificationReference>` mechanism). PR-11
  full-replace omits per-line back-pointers; the chain is base-level
  only. First "partial return" PR is the trigger to add per-line
  references.

## Consequences

**What gets easier**

- PR-11 lands without re-litigating naming, allocator, payload, or
  the four-edit count. The pre-flight reading is this ADR plus
  ADR-0023 plus `apps/aberp/src/issue_storno.rs` (the template).
- Both the per-invoice export bundle (ADR-0009 §8) and the audit-
  evidence walker get one consistent chain-walk shape: ULID-keyed
  by `base_invoice_id`, payload-typed, kind-prefix-globbable
  (`invoice.*`).
- A future technical-annulment PR (PR-12 or later) does NOT touch
  the chain allocator at all — `manageAnnulment` does not consume
  chain slots and the `invoice.technical_annulment_requested` audit
  kind is its own surface, not a chain-link kind. The chain-allocator
  is sealed for PR-11.

**What gets harder**

- The detector now reads two substrings, not one. A future emitter
  change that adds an attribute to `<modificationIssueDate>` would
  silently drop MODIFY back to STORNO — closed by the round-trip
  pair-up test added in PR-11 (`apps/aberp/tests/issue_modification_xml_round_trip.rs`).
- The chain allocator now walks two kinds. A third kind (if one
  appears) requires extending both `issue_storno.rs`'s and
  `issue_modification.rs`'s walkers — the F12 closed-set discipline
  applied to the allocator path, not just the event_kind enum. PR-11
  ships this with a comment naming the symmetry.
- The audit-payload schema versioning rule (ADR-0023 §"What we lock
  ourselves into") applies to `InvoiceModificationIssuedPayload` too:
  adding a field is forward-compatible (older readers see the
  pre-extension shape if they ignore unknown fields); removing or
  renaming requires a new `EventKind` variant.

**What we lock ourselves into**

- Subcommand name `aberp issue-modification` and arg names
  (`--references`, `--in`, `--out`, `--db`, `--tenant`, `--series`,
  `--modification-date`). Rename requires an amendment ADR.
- Payload struct name `InvoiceModificationIssuedPayload` + field
  names + the `modification_issue_date: String` shape (NOT
  `time::Date`).
- The detector's reliance on the presence of `<modificationIssueDate>`
  as the MODIFY discriminator. If NAV ever permits a MODIFY without
  this field (unlikely — it is NAV-required per the research file),
  the detector needs a new disambiguator; the failure mode would be
  surfaced by NAV rejecting the body, which is loud.
- Full-replace MODIFY body shape (§4). Per-line delta semantics
  (`<lineModificationReference>`) are deferred. The chain payload
  carries no line-level back-pointers.

## Adversarial review

A hostile NAV inspector + a hostile-engineer review, alternating.
ADR-README bar is three; four surfaced because the MODIFY chain is
the same NAV-inspector surface that ADR-0023 §"Adversarial review"
flagged.

1. **"Your detector reads two substrings and decides MODIFY vs STORNO
   on the presence of `<modificationIssueDate>`. A NAV testbed run
   shows MODIFY bodies validated by NAV that do NOT carry that
   substring (e.g. because NAV's schema permits its absence for a
   sub-case ABERP has not yet exercised). Every such MODIFY you POST
   would be classified as STORNO, and NAV would reject it with
   `INVOICE_OPERATION_MISMATCH`."** The detector's correctness depends
   on the research file's claim that `<modificationIssueDate>` is
   NAV-required for MODIFY. The research-doc citation is the load-
   bearing input. PR-11 commits to the detector + emitter pair:
   the emitter ALWAYS writes `<modificationIssueDate>` for MODIFY
   (driven by the operator-required `--modification-date` arg), and
   the validator pair-up test (added in PR-11) closes the loop.
   The NAV-testbed verification trigger (§1 conflict 1) is the
   external check; until then the loud-fail mode is "NAV rejects at
   submit time" rather than "ABERP silently misclassifies". The chain
   payload AND the on-disk XML carry the modification date, so an
   inspector reading the chain bundle can spot the misclassification
   even if NAV's submit-time check is bypassed. **Accepted with
   trigger named.**

2. **"Your allocator walks two kinds with two SQL queries inside one
   transaction. A future contributor adding a third chain kind will
   add a third SQL query, miss the extension in one of the two
   allocator functions (issue_storno OR issue_modification), and the
   chain will silently allocate a duplicate index against a base
   whose third-kind-chain has already burned that index."** The
   failure mode is real. PR-11 closes it by symmetry: both
   `issue_storno::next_modification_index_in_tx` and
   `issue_modification::next_modification_index_in_tx` are extended
   to walk BOTH kinds. The comment naming the symmetry lives in
   both files. A third chain kind appearing would require updating
   both files — the closed-set discipline F12 already names for the
   event_kind enum, applied to the chain allocator. The natural
   extraction to a shared `chain_allocator::next_index_in_tx` would
   live in `apps/aberp/src/chain_allocator.rs` (new module); §7
   defers this to the third-kind-trigger per CLAUDE.md rule 2.
   **Accepted, trigger named.**

3. **"You permit modify-after-modify by default. A NAV inspector
   reading a chain of seven modifications against the same base
   would argue the operator is using MODIFY as a workflow tool
   rather than as an accounting correction; the accountant
   convention would prefer a single fresh corrective invoice over a
   long chain."** The default-permit posture matches NAV's API
   (which permits) and matches ADR-0023 §7's symmetric storno-of-a-
   storno posture. The accountant question is open (§8); a
   reconciliation-anomaly threshold (chain length > N) is a natural
   future addition but PR-11 does not pin it — soft-asserting "N=3"
   today would be the soft-assertion failure mode CLAUDE.md rule 12
   names. The audit-evidence bundle's chain walker makes the chain
   length visible; an audit catches the policy violation. **Accepted
   — open question, not a code constraint.**

4. **"You loud-reject `MODIFY` against a Storno-base by default. A
   NAV-permissive contributor will look at NAV's API (which permits
   it under some interpretations) and remove the rejection branch
   without re-opening the accountant question. The constraint will
   silently disappear."** The rejection is named in §6 with the
   accountant-question link in §8. The corresponding code in
   `issue_modification::run`'s precondition walker carries the
   inline citation. A future contributor removing the branch would
   need to also remove the inline comment and the §6/§8 cross-ref;
   the PR review surface makes the deletion visible. The named
   error message ("base invoice {} has been legally cancelled by a
   storno — issue a fresh corrective new invoice instead") is the
   operator-visible artifact that, if it stopped firing, would
   surface in an audit-bundle review. **Accepted — the comment +
   inline error message is the load-bearing review surface.**

## Alternatives considered

- **Add MODIFY as a flag on `issue-storno` rather than a new
  subcommand** (`aberp issue-storno --operation modify --modification-date ...`).
  Rejected — same ADR-0023 §"Alternatives considered" reasoning
  applies: the two operations have different preconditions and
  different audit-payload shapes. Forcing them through one CLI flag
  makes the operator surface less clear.

- **Delta-shape modification body** (carry only the changed lines +
  reference the base's other lines by `<lineModificationReference>`).
  Rejected per §4 — full-replace is the simpler discipline and the
  per-invoice export bundle benefits from each modification being
  audit-readable in isolation.

- **Single allocator walking `kind IN ('invoice.storno_issued',
  'invoice.modification_issued')`** rather than two sequential
  queries. Rejected per §7 — the two-query shape keeps the per-kind
  decode loops homogeneous and the extension trigger (third kind)
  visible.

- **Default `--modification-date` to "today" if the operator omits.**
  Rejected — per §1, the modification date is an audit-bearing field
  that an accountant may legitimately set to a backdated value; a
  silent today-default would mask the operator-intent on the audit
  surface. CLAUDE.md rule 12 (fail loud on audit-bearing inputs).

- **Add `EventKind::InvoiceAmended` as a derived event written
  against the base** (mirroring the rejected
  `EventKind::InvoiceStornoed` posture from ADR-0023 §"Alternatives
  considered"). Rejected for the same reason: the base's typestate
  transition (`Finalized → Amended`) is derived from the existence
  of a successful modification's payload pointing at the base; a
  second ledger entry against the base would duplicate the source
  of truth.

- **Store `modification_issue_date` as `time::Date` rather than
  `String`.** Rejected — the audit payload's serialization shape is
  the canonical record; a typed-time wrapper would force serde-with
  adapters for a value the operator already supplies in the
  canonical `YYYY-MM-DD` form. The CLI's pre-parse validation
  (`time::Date::parse`) keeps the loud-fail surface at the operator
  boundary without polluting the payload schema. (The same posture
  ADR-0009's audit payloads use for `ack_status` carrying a
  `String` rather than a typed `AckStatus` enum — the typed
  Rust-side state lives downstream of the audit ledger, not in it.)

## Open questions

Tracked against the next fortnightly adversarial review and the named
external-check items in `docs/research/nav-and-billingo.md`:

- **Modify-of-a-Storno-base accountant practice.** Default-reject
  until accountant resolves (§6, §8).
- **Modify-after-modify chain length cap.** No cap; reconciliation-
  anomaly threshold deferred (§8).
- **`<modificationIssueDate>` position in the NAV v3.0 XSD.** Inside
  `<invoiceReference>` per the research-doc grammatical reading.
  Verification deferred to first NAV-testbed MODIFY POST (§1
  conflict 1).
- **Per-line delta semantics** (`<lineModificationReference>`).
  Deferred to first "partial return" / "partial credit" PR (§4).
- **Migrated-from-Billingo MODIFY chain-digest read.** Same trigger
  as ADR-0023 §4's `queryInvoiceChainDigest` deferral; tracked under
  F23 (extended to MODIFY by §7).

## Follow-on PRs unblocked by this decision

- **PR-11 — Modification chain (code).** Implements the four edits in
  §5 above plus `apps/aberp/src/issue_modification.rs`, the detector
  extension, the validator allowlist extension, the chain-allocator
  widening in BOTH `issue_storno.rs` and `issue_modification.rs`, and
  the matching unit + integration tests.
- **PR-12 or later — Technical annulment.** Distinct surface
  (`aberp request-technical-annulment`), distinct NAV endpoint
  (`manageAnnulment`), distinct ledger entry kind
  (`invoice.technical_annulment_requested` — already named in
  ADR-0009 §2). NOT in scope for PR-11 (§1 re-assert).
- **First per-invoice export-bundle PR (gated on F5 + F10 per session
  12 handoff).** Consumes the MODIFY chain-link payloads via the
  same `base_invoice_id` ULID traversal as STORNO.
- **First "partial return" / "partial credit" PR.** Adds
  `<lineModificationReference>` line-level back-pointers to the
  modification body. The chain payload may gain a
  `line_modification_count: u32` field at that PR's discretion.
- **First NAV-testbed MODIFY POST.** Verifies the
  `<modificationIssueDate>` position assumption (§1 conflict 1).
  Trigger for the next ADR-0024 amendment iff verification reveals a
  mismatch.
