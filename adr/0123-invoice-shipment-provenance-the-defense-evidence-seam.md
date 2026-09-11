# ADR-0123 — Invoice↔shipment provenance: closing the defense evidence seam (F1)

- **Status:** **Accepted** (2026-09-11) — passed adversarial review round 1
  after a FIX-FIRST verdict. The round found a **sixth site the design pass had
  missed**, and it is the worst of them: the `EventKind` schema documentation
  asserts, in three places, a complete invoice↔shipment provenance chain that
  exists at none of its hops. Two design holes (a storno-then-reissue that loses
  the link; a detector that would read as a permanent accusation) are closed
  below. Safe to build, in the slices at the end.
- **Date:** 2026-09-11
- **Deciders:** Design pass + adversarial review round 1, 2026-09-11
  (`docs/_adversarial-adr-0123-invoice-shipment-provenance-round1.md`).
  Ervin's call to close the chain (F1 CLOSE, 2026-09-11).
- **Related:** ADR-0122 §F1 (where this was found), ADR-0064 §6 (the dispatch
  kinds and `mark_shipped`), ADR-0094 (the no-new-`EventKind` blast-radius
  clause), ADR-0029/§2 (the invoice evidence bundle's membership rule),
  ADR-0199 §D6 (`bind_reports_to_dispatch` — the QC side of the same seam),
  S236 / PR-230b (the spawner and the scope reduction that deferred the
  promote), `[[trust-code-not-operator]]`.

## Context

### What is broken

An outgoing `inv_*` invoice carries **no reference to the `dsp_*` dispatch it
bills**, and the link is derivable from neither the ledger nor the tables. For
a NAV audit that is tolerable — NAV's interest begins at the invoice. For a
**defense** evidence trail it is the seam that matters: it is what would
connect an invoice to the work order, the part marks, the export-control
screening and the QC reports that justify it.

### Six sites, all verified on this tree

The first four were found closing ADR-0122; the fifth was found writing this
ADR, and the sixth by the adversarial round. The last two each changed the
shape of the fix.

1. **The spawner returns a DRAFT id.** `BillingInvoiceSpawner::spawn` inserts
   an `invoice_draft` row and returns `Ok(Some(draft.drf_id))`
   (`apps/aberp/src/invoice_draft.rs:658`). `mark_shipped` stores that in
   `mes.dispatch_shipped`'s `spawned_invoice_id`
   (`crates/aberp-dispatch/src/audit.rs:89`) — a field whose own doc comment
   admits "the value is in fact 'spawned-invoice-or-draft id'".

2. **Promotion is a form-fill, not a transaction.** The route comment states
   it: "the SPA pre-fills the form from a GET to `/api/invoice-drafts/:id`,
   then DELETEs the draft after the invoice creation succeeds; cross-pipeline
   atomic promote is deferred to a future PR per the PR-230b scope-reduction"
   (`apps/aberp/src/serve.rs:5188`).

3. **`issue_invoice` records no draft reference.** It fires
   `InvoiceSequenceReserved` + `InvoiceDraftCreated` against a freshly minted
   `inv_*` and never names the `drf_*` any of its numbers came from.

4. **The delete NULLs the only surviving pointer.** `delete_draft_in_tx` calls
   `aberp_dispatch::null_spawned_invoice_id_in_tx` before removing the row
   (`apps/aberp/src/invoice_draft.rs:555`), so the dispatch's
   `spawned_invoice_id` is cleared and the draft row is gone.

5. **The prefill in site 2 does not exist either.** `IssueInvoice.svelte`
   contains no occurrence of `draft` or `drf`. `getInvoiceDraft` has exactly
   one caller in the whole SPA — `draft-delete.ts`, composing the
   delete-confirmation copy. So the draft does not even *populate* the issue
   form: an operator issues an invoice from scratch, and the draft is deleted
   (or orphaned) afterwards.

6. **The schema documentation says the chain already works.** Found by the
   adversarial round, and the worst of the six. `EventKind`'s doc comments are
   this codebase's schema contract — enforced, not merely conventional
   (`export_payload_field_names_match_the_eventkind_docs` pins payload fields
   against them; `crates/aberp-dispatch/src/audit.rs:21` says "the doc comment
   IS the schema contract"). Three of them describe the walk as working:

   - `event_kind.rs:1004` (`DispatchShipped`): "The audit-trail walks **both
     ways** … from the invoice draft's own `InvoiceDraftCreated` entry back to
     the dispatch via the invoice idempotency-key suffix
     (`derive_from(dispatch.dsp_id, "spawn_invoice")`)".
   - `event_kind.rs:1030` (`InvoiceStaged`): "The chain continues at promotion
     time via the operator-issued `InvoiceSequenceReserved` +
     `InvoiceDraftCreated` pair, which references the draft id in their
     idempotency key suffix (`derive_from(draft.drf_id, "issue")`)".
   - `crates/aberp-dispatch/src/audit.rs:62`: the same claim again.

   Every hop is false. **`derive_from` does not exist** — three doc comments
   reference it and nothing defines or calls it. The spawn key is
   `format!("{}:spawn_invoice", inputs.idempotency_key)`
   (`crates/aberp-dispatch/src/repository.rs:790`), built from the *ship
   request's* key, which carries no dispatch id. The "promotion time" pair does
   not exist because promotion does not (site 5). And the draft fires
   `InvoiceStaged`, not `InvoiceDraftCreated`, so even the entity named is the
   wrong one.

   Sites 1–5 are absences: a reader finds nothing and knows there is nothing.
   This one is an assertion — someone auditing the traceability posture the
   documented way concludes the chain is present and stops looking.

Site 5 matters because it removes an option. There is no client-side link
lying around to start recording — **the link does not exist anywhere and has
to be established, not merely preserved.** Site 6 matters because it means the
work is not done when the link exists: the documentation that currently claims
it must be corrected in the same change, or the fix leaves behind a schema
contract describing a mechanism that was never built and now never will be.

### What the draft row already knows

`invoice_draft` (`apps/aberp/src/invoice_draft.rs:84`) carries
`source_dispatch_id`, `source_wo_id`, `partner_id`, `product_id`, `qty`, with
`UNIQUE (tenant_id, source_dispatch_id)`. Everything the invoice needs is
already on that row, written by the spawner inside `mark_shipped`'s
transaction. The row is the one trustworthy statement of "this billing work
came from that shipment" — and today it is deleted without ever being read for
that purpose.

## Decision

**Promote the draft into the invoice in one transaction, and record the
provenance on the INVOICE side, derived from the draft ROW.**

Three properties, each answering one of Ervin's requirements:

| requirement | mechanism |
|---|---|
| an invoice carries a durable reference back to the shipment | `source_dispatch_id` / `source_wo_id` / `source_draft_id` recorded on the invoice at issuance — column **and** audit payload |
| surviving promotion | the promote route is what mints the invoice, so the reference is written in the same transaction, not bolted on afterwards |
| NOT NULLed on delete | the reference lives on the **invoice**, not on the dispatch. `delete_draft_in_tx` clears a pointer on `dispatches`; it cannot reach a column on `invoice`. The draft's disappearance is expected and harmless |

### D1 — `POST /api/invoice-drafts/:id/promote`, one transaction

The route PR-230b deferred. It reads the draft row, mints the invoice through
the existing issue pipeline, records the provenance, and **flips the draft's
state to `Promoted`** (see D5) — all in one transaction, so an invoice that
commits without its provenance, or a draft that changes state without an
invoice, are both unrepresentable.

**The atomicity already exists; promote joins it rather than inventing it.**
`run_single_tx` (`apps/aberp/src/issue_invoice.rs:1155`) opens ONE transaction
on the shared `aberp_db::Handle` writer (`:1222`) and commits at `:1385`, and
S375 deliberately moved the render + XSD-validate + NAV-XML write *inside* that
window, recording why in the code: "A failure here returns `Err` so the tx drops
un-committed → the allocation + audit appends roll back together and no
committed-but-XML-less invoice row survives." The draft read and the state flip
go in the same `tx`.

*Residual, pre-existing and not made worse here:* the reverse of that ordering —
a rollback after the XML is written leaves an orphan file on disk. That is the
trade the existing comment accepts.

**Why a route and not a field on the existing issue form.** The alternative is
`IssueInvoiceRequest` gaining `source_draft_id`, with the SPA passing it. That
needs SPA work anyway (site 5: the form has no draft context to pass), and it
makes the link a **claim in a request body**. A promote route makes it a
**derivation from a row the server reads** — the operator names *which draft*,
and the server decides what that draft says about dispatch and work order. The
operator can be wrong about which draft; they cannot forge what the draft
contains. That is the `[[trust-code-not-operator]]` line.

### D2 — the reference lives on the invoice, additively

- **Column.** `invoice.source_dispatch_id`, `source_wo_id`, `source_draft_id`
  — nullable, additive migration, no backfill (pre-ADR-0123 invoices genuinely
  have no link and must not be made to claim one).
- **Audit payload.** Three `Option<String>` fields on
  `InvoiceDraftCreatedPayload`, `#[serde(default)]`, following the four
  additive fields already on it (`nav_xml_path`, `currency`, `exchange_rate`,
  `rate_source`). **No new `EventKind`** — ADR-0094.

**Serialization shape: explicit `null`, never omitted.** The codebase carries
both precedents — `skip_serializing_if` (`audit_payloads.rs:1943`) and a pin
that `None` must serialize as null (`aberp-dispatch/src/audit.rs:302`). For an
evidence field the second is correct: omitting conflates "this invoice has no
shipment provenance" with "an older binary did not know the field", and those
are exactly the two things an auditor needs told apart.

The cost is three `null` keys on every `InvoiceDraftCreated` payload, Portable
included. **The review confirmed nothing pins that key set**: the three tests
that read the payload (`mark_abandoned_live.rs:147`, `retry_submission_live.rs:172`,
`submit_invoice_live.rs:175`) each assert only that `idempotency_key` is
present, and the struct already carries four additive `#[serde(default)]`
`Option` fields from prior PRs. So this stands on evidence, not on a hedge, and
the fallback the design pass had flagged here is withdrawn.

### D3 — the bypass is DETECTED, not blocked

An operator can still issue an invoice through the ordinary form, leaving a
shipped dispatch with no provenance. **This ADR does not refuse that**, and
the reasoning is that every blocking rule available is a heuristic:

- refusing the plain form outright breaks ordinary non-dispatch invoicing
  (quotes, ad-hoc, service lines);
- refusing when "an outstanding draft exists for this partner" guesses at
  operator intent and will refuse legitimate work.

A wrong refusal on a money path is worse than a missing link. So instead:
`GET /api/shipment-invoice-provenance` folds the ledger and reports, for every
`mes.dispatch_shipped`, whether an invoice carries its `source_dispatch_id`.
Same read-side, no-new-kind shape ADR-0121 used for NIST coverage; it makes the
hole **countable** rather than invisible.

**It is named for what it measures, not for what is wrong, and a non-zero count
is expected.** `mes.dispatch_shipped` fires on real shipment only, so a warranty
replacement, a free sample and a consignment movement are all genuine shipped
dispatches that will never have an invoice. Under a name like
"shipments-*without*-invoice-provenance" those read as defects in perpetuity,
and a report whose baseline is permanently non-zero is one people stop opening.
The report states an absence; it does not allege a fault.

A suppression / acknowledgement mechanism is **deliberately not built**: a
"mark this one as fine" flag on an evidence report is exactly the affordance
that needs its own argument, and building it alongside the report would smuggle
that argument in. Named as deferred.

### D4 — edition scope: Defense-gated behaviour, shared plumbing

The **prod-invoice line is frozen** and lives in `ABERP.git`; this repository's
Portable edition must not change shape. So:

- the promote **route** is Defense-only (`storefront_polling_allowed`-shaped
  gate, the `qc_reporting_allowed` pattern), refusing on Portable with a
  reason;
- the **columns** are additive and unused on Portable (an empty nullable
  column is inert);
- the **payload fields** are the one place Portable is visibly touched: three
  `null` keys appear on every `InvoiceDraftCreated` entry. Per D2 that is the
  honest shape, and it changes no *existing* entry — the chain hashes each
  entry as written, and `#[serde(default)]` means old entries still decode.
  The review confirmed no gate, test or golden pins that key set.

### D5 — promote FLIPS the draft's state; it does not delete it

The design pass had promote delete the draft, matching today's
`delete_draft_in_tx`. The review found that loses the link through an **ordinary
correction path**:

1. dispatch ships → draft created → promote → invoice A carries the link, draft
   gone;
2. invoice A is stornoed (ADR-0023 — a routine correction, not an exotic case);
3. a corrected invoice B is issued for the same shipment;
4. **no draft is left to promote**, so B goes through the plain form and carries
   no provenance.

The shipment's *current* invoice then has no link — exactly the state this ADR
exists to eliminate, reached without anything going wrong.

So `invoice_draft` gains a `state` column (`Staged` → `Promoted`) and promote
flips it. The row is the shipment's standing billing-provenance record and
should outlive one invoice attempt. `UNIQUE (tenant_id, source_dispatch_id)`
still holds — one row per dispatch, reused rather than duplicated.

Carried consequences, all in slice 2:
- the operator DELETE route keeps its meaning (discarding an unwanted draft) and
  **refuses a `Promoted` row** — deleting the provenance record of an issued
  invoice is not an operator-level action;
- `listInvoiceDrafts` filters to `Staged` by default, so promoted rows stop
  appearing as work to do.

**This also settles Q3**, which asked the same question from the other side.

### D6 — a storno does NOT copy the provenance; it inherits it by one hop

A storno already carries `base_invoice_id` (`apps/aberp/src/issue_storno.rs:691`,
and it is one of `BundleMembershipProbe`'s four id fields), so its provenance is
one declared hop away: storno → base invoice → `source_dispatch_id`.

It does **not** get its own copy. Two places to record the same fact is two
places to disagree, and the hop already exists — the same "one declared hop, no
closure" discipline ADR-0122 §D1 settled for the QC slice. Stated explicitly
because the implementation instinct is to copy the field.

Note this does not cover a *fresh replacement* invoice after a storno: that is
not a storno and carries no `base_invoice_id`. D5 is what covers it.

### D7 — promote's idempotency key is derived from the draft id, and carries no meaning

`issue_invoice` is idempotency-keyed and replays rather than double-minting
(`AllocateOutcome::Replay`), so promote must supply a key that makes a
double-click a replay. The natural stable value is the **draft id** — which is
what the phantom `derive_from(draft.drf_id, "issue")` in site 6 was reaching
for. Build that derivation for real, typed.

**The key is for idempotency only.** The provenance is the typed column and
payload field, never a substring of a key. Site 6's comments are corrected to
say so: an idempotency-key suffix as a traceability channel means parsing a
string whose job is something else, and it breaks silently the moment the key
format changes.

### D8 — the three false schema comments are corrected in this change

Site 6's comments (`event_kind.rs:1004`, `event_kind.rs:1030`,
`crates/aberp-dispatch/src/audit.rs:62`) are corrected to describe what exists:
the forward pointer (`spawned_invoice_id`, and what it actually holds), the
typed provenance this ADR adds, and — until slice 2 lands — the honest absence.
`derive_from` is removed from all three.

This is **not** a follow-up. A build that closes the seam while leaving the
schema contract claiming it was never open is half a fix, and the half that
remains is the half an auditor reads.

## Consequences

- A Defense invoice minted through promote can be joined to its dispatch, and
  through `mes.dispatch_shipped` → `qcr.report_attached_to_shipment` to the QC
  reports, the part marks and the export-control decisions. That is the seam
  ADR-0122 §F1 said was cut.
- ADR-0122's shipment bundle and the invoice bundle stay separate artifacts,
  because this link is forward-looking: invoices issued before it exists have
  no provenance and must not pretend otherwise. Merging the two bundles is a
  later decision, on a later corpus.
- `BundleMembershipProbe` (`export_invoice_bundle.rs:157`) is **not** extended
  here. Adding `source_dispatch_id` to it would sweep dispatch entries into an
  invoice bundle by a *transitive* rule the flat probe cannot express — the
  exact distinction ADR-0122 §D1 drew. Out of scope, named rather than assumed.
- The `dispatches.spawned_invoice_id` column keeps its current meaning
  ("spawned-invoice-or-draft id") and keeps being NULLed on delete. This ADR
  does not repair it; it stops depending on it.

## Adversarial review

**Round 1 (2026-09-11) — FIX-FIRST, applied.** Full write-up:
`docs/_adversarial-adr-0123-invoice-shipment-provenance-round1.md`. Checked
against the tree, not against this ADR's own text.

- **Site 6 (new, blocking)** — the `EventKind` schema docs assert the whole
  chain, in three places, and every hop is false. Sites 1–5 are absences; this
  one is an assertion, which is worse. Closed by **D8**, in this change rather
  than as a follow-up.
- **S2 (blocking)** — promote-then-delete loses the link through storno and
  re-issue, an ordinary correction path. Closed by **D5** (state flip), which
  also settles Q3.
- **S4** — the detector would have read as a permanent accusation, because
  warranty replacements, samples and consignment movements are genuine shipped
  dispatches with no invoice. Closed by renaming it for what it measures and
  stating that a non-zero baseline is expected.
- **S3** — storno provenance decided explicitly: inherited by one hop through
  `base_invoice_id`, never copied (**D6**).
- **S6b** — promote's idempotency key derived from the draft id, with the key
  explicitly carrying no traceability meaning (**D7**).

Two surfaces cleared on evidence and simplified the ADR rather than complicating
it:

- **S1 — can promote be one transaction?** Yes, and the pattern already exists:
  `run_single_tx` opens one transaction and S375 deliberately put the NAV-XML
  write inside it. D1 now cites this instead of asserting it.
- **S5 — does anything pin the payload key set?** No. So D2's explicit-`null`
  choice stands on evidence and D4's flagged fallback is **withdrawn** — a
  "flagged alternative" nobody needs is a decision left ajar.

Four properties held: the invoice-side placement (`delete_draft_in_tx` has no
path to a column on `invoice`, so "NOT NULLed on delete" holds by construction);
derivation from the row rather than the request body; no backfill; and leaving
`BundleMembershipProbe` alone.

## Alternatives considered

- **`IssueInvoiceRequest` gains `source_draft_id`.** Rejected as the primary
  mechanism (D1): it makes the link a request-body claim, and needs the same
  SPA work as promote. Worth keeping in mind as a *fallback* if the review
  finds promote cannot be one transaction (surface 1).
- **Server-side inference** — match an invoice to a draft by partner + product
  + qty. Rejected outright: a heuristic join, written into an append-only
  ledger, that is wrong exactly when two similar shipments are in flight.
- **Repair `dispatches.spawned_invoice_id` instead.** Rejected: it fails
  Ervin's "NOT NULLed on delete" requirement by construction — it is a pointer
  on the dispatch, and the draft delete's whole job is to clear it.
- **Backfill existing invoices.** Rejected. The data to backfill from was
  deleted with the drafts. A reconstructed link is a guess, and a guess in an
  evidence trail is worse than an honest absence.

## Open questions

- **Q1 — does promote replace the plain form for dispatch work, or sit beside
  it?** This ADR says beside, with D3's detector. Flagged because it is the
  decision that determines whether the seam is *closed* or merely *closeable*.
- **Q2 — should the shipment bundle (ADR-0122) gain the invoice id** once an
  invoice carries the link? It becomes derivable in the reverse direction for
  the first time. Deliberately not built here.
- ~~**Q3 — retention.**~~ **RESOLVED by D5**: the draft is state-flipped, not
  deleted. The review reached the same answer from the storno-reissue direction,
  which is the stronger argument for it.

## Build slices

Each slice runs the full local gate suite and integrates to local main before
the next starts.

1. **Slice 1 — the reference and the state column, with nothing writing them.**
   The additive migration (`invoice.source_dispatch_id` / `source_wo_id` /
   `source_draft_id`, and `invoice_draft.state` defaulting to `Staged`), the
   three `Option` payload fields with round-trip pins, and the read helper that
   resolves an invoice's provenance. Inert by construction: nothing populates
   any of it, so the slice is provably behaviour-preserving.
2. **Slice 2 — the promote route.** `POST /api/invoice-drafts/:id/promote`,
   Defense-gated, inside `run_single_tx`'s existing transaction, provenance
   derived from the draft row, draft state-flipped (D5), idempotency key derived
   from the draft id (D7). Carries D5's two consequences: DELETE refuses a
   `Promoted` row, and the list filters to `Staged`.
   End-to-end: ship a dispatch, promote its draft, assert the invoice carries
   the dispatch id in **both** the column and the chain; then delete everything
   deletable and assert the invoice's provenance survives — the revert-proof for
   Ervin's "NOT NULLed on delete".
   Also pinned: a storno-then-reissue keeps the shipment's current invoice
   linked (the D5 revert-proof — delete the state flip and this reds).
3. **Slice 3 — D8, the schema-doc correction.** The three false comments
   corrected, `derive_from` removed from all three. Docs-only, but landed as its
   own slice so the correction is reviewable on its own and cannot be lost
   inside a feature diff. *May land before slice 2* — it is true either way, and
   earlier is better for anyone reading the contract meanwhile.
4. **Slice 4 — the detector.** `GET /api/shipment-invoice-provenance` as a
   read-side ledger fold: no new `EventKind`, no firing site, no schema.
5. **Slice 5 — the SPA promote affordance**, replacing the flow site 5 showed
   was never built.
6. **Slice 6 — docs.** ADR-0122 §F1 flipped to closed; the backlog F1 entry
   updated; README if a row moves.
