# Adversarial review — ADR-0123 round 1 (2026-09-11)

**Verdict: FIX-FIRST.** The decision (promote in one transaction, provenance on
the invoice side, derived from the draft row) survives — and surface 1, the one
that could have killed it, resolves in its favour on evidence. But the review
found **a sixth site that the ADR's Context did not have**, and it is worse than
the five it did: the `EventKind` schema documentation asserts a complete
invoice↔shipment provenance chain, in three places, that exists at none of its
hops. Two design holes also need closing before any code.

Everything below was checked against the tree at `e383473`.

---

## S1 (resolves FOR the decision) — promote CAN be one transaction

**Claim under test.** ADR §Adversarial surface 1: is there a step in
`issue_invoice` that cannot run inside the caller's transaction — particularly
the filesystem NAV-XML write, which no transaction rolls back?

**Measured.** The pipeline already does exactly this, deliberately.
`run_single_tx` (`apps/aberp/src/issue_invoice.rs:1155`) opens ONE transaction
on the shared `aberp_db::Handle` writer (`:1222`), and S375 moved the
render + XSD-validate + NAV-XML write to **inside** that window, before
`tx.commit()` (`:1385`), with the reasoning recorded in the code:

> "A failure here returns `Err` so the tx drops un-committed → the allocation +
> audit appends roll back together and no committed-but-XML-less invoice row
> survives."

So promote does not need a new atomicity story; it needs to join one that
already exists. The draft read and the draft state flip go inside the same
`tx`. **No change to the ADR's D1 is required** — but D1 should cite this
rather than assert it, because a reader will otherwise ask the same question.

**Residual, pre-existing and out of scope:** the reverse of the S375 ordering.
A tx that rolls back *after* the XML is written leaves an orphan file on disk.
That is the shape the existing comment accepts; promote neither worsens nor
fixes it.

---

## S6 (blocking, NEW — the ADR did not have this) — the schema docs assert a chain that does not exist

`EventKind`'s doc comments are this codebase's **schema contract** — that is not
an interpretation, it is enforced: `export_payload_field_names_match_the_eventkind_docs`
pins payload field names against these very comments, and
`crates/aberp-dispatch/src/audit.rs:21` states "the doc comment IS the schema
contract".

Those comments describe the invoice↔shipment walk as **already working**, in
three places:

| site | claim |
|---|---|
| `event_kind.rs:1004` (`DispatchShipped`) | "The audit-trail walks **both ways**: from dispatch to invoice via this payload's `spawned_invoice_id`; from the invoice draft's own `InvoiceDraftCreated` entry back to the dispatch via the invoice idempotency-key suffix (`derive_from(dispatch.dsp_id, "spawn_invoice")`)" |
| `event_kind.rs:1030` (`InvoiceStaged`) | "The chain continues at promotion time via the operator-issued `InvoiceSequenceReserved` + `InvoiceDraftCreated` pair, **which references the draft id in their idempotency key suffix** (`derive_from(draft.drf_id, "issue")`)" |
| `crates/aberp-dispatch/src/audit.rs:62` | the same `derive_from(dispatch.dsp_id, "spawn_invoice")` claim |

**Every hop of that is false:**

- **`derive_from` does not exist.** `grep -rn "derive_from" --include=*.rs` over
  the whole tree returns exactly those three doc comments and no definition,
  no call site.
- **The spawn key is not derived from `dsp_id`.** The real value is
  `format!("{}:spawn_invoice", inputs.idempotency_key)`
  (`crates/aberp-dispatch/src/repository.rs:790`) — the *ship request's* key,
  which carries no dispatch id.
- **The "promotion time" pair does not exist**, because promotion does not
  exist (ADR §Context site 5).
- **The draft fires `InvoiceStaged`, not `InvoiceDraftCreated`**, so even the
  entity named in the first claim is the wrong one.

**Why this is the worst of the six.** The other five are *absences* — a reader
inspecting the code finds nothing and knows there is nothing. This one is an
*assertion*: someone auditing the traceability posture by reading the schema
contract, which is the documented way to read it here, concludes the chain is
present and walks away satisfied. It is the same failure class ADR-0122's F2
had (a doc claiming a property the code only conditionally has), one notch
worse because nothing about it is conditional.

**Required.** Add as Context site 6, and make correcting all three comments
part of the work — not a follow-up. A build that closes the seam while leaving
the docs claiming it was never open is half a fix.

**Do NOT resurrect the mechanism they describe.** An idempotency-key *suffix*
as the provenance channel means parsing a string whose job is idempotency, and
it would break the moment the key format changes. The ADR's typed field is the
right answer; the comments should be corrected to describe it, not the phantom.

---

## S2 (blocking) — storno-then-reissue loses the link, because promote deletes the draft

**Claim under test.** ADR §Adversarial surface 2: does
`UNIQUE (tenant_id, source_dispatch_id)` make promote unrepeatable in a case
that legitimately repeats?

**Measured.** Yes, and the constraint is not the reason — the **delete** is.
ADR §D1 has promote delete the draft, matching today's
`delete_draft_in_tx`. So:

1. dispatch ships → draft created → promote → invoice A carries the link, draft
   gone;
2. invoice A is stornoed (a real, ordinary correction path — ADR-0023);
3. a corrected invoice B must be issued for the same shipment;
4. **there is no draft left to promote**, so B goes through the plain form and
   carries no provenance.

The shipment's *current* invoice then has no link, which is precisely the state
this ADR exists to eliminate — reachable through an ordinary correction, not an
exotic one.

**Required fix, and it also answers the ADR's own Q3.** Promote **flips the
draft's state** (add a `state` column: `Staged` → `Promoted`) instead of
deleting it. The row is the shipment's standing billing-provenance record, and
it should outlive one invoice attempt. Consequences to carry:

- `UNIQUE (tenant_id, source_dispatch_id)` still holds — one row per dispatch,
  reused, not duplicated;
- the operator DELETE route keeps its current meaning (an operator discarding a
  draft they do not want) and must refuse, or be reconsidered, for a
  `Promoted` row;
- `listInvoiceDrafts` needs a state filter so promoted rows stop appearing as
  work to do.

This is strictly more information than deleting, and it costs one nullable
column.

---

## S3 (fix in the same pass) — storno provenance is derivable; do not duplicate it

**Measured.** A storno already carries `base_invoice_id`
(`apps/aberp/src/issue_storno.rs:691` and the bundle probe's field set), so a
storno's provenance is **one declared hop** away: storno → base invoice →
`source_dispatch_id`.

**Decide it explicitly, in the ADR:** the storno does NOT get its own
`source_dispatch_id` copy. Copying it would create two places to disagree, and
the hop already exists. This is the same "one declared hop, no closure"
discipline ADR-0122 §D1 settled for the QC slice — and stating it matters,
because the obvious implementation instinct is to copy the field.

Note this does **not** cover S2's case: a fresh replacement invoice after a
storno is not a storno and carries no `base_invoice_id`. S2's fix is what
covers it.

---

## S5 (clears) — nothing pins the payload key set

**Claim under test.** Does any gate, test or golden pin the exact key set of an
`InvoiceDraftCreated` payload, such that three new `null` keys would red it?

**Measured.** No. The three tests that read the payload
(`mark_abandoned_live.rs:147`, `retry_submission_live.rs:172`,
`submit_invoice_live.rs:175`) each assert the presence of `idempotency_key` and
nothing about the key set. The struct already carries four additive
`#[serde(default)]` `Option` fields from prior PRs, which is the precedent.

**So D4's flagged fallback is not needed** — explicit `null` (ADR §D2) can
stand on its evidence rather than on a hedge. Delete the hedge; a "flagged
alternative" nobody needs is a decision left ajar.

---

## S4 (fix in the same pass) — the detector must not read as an accusation

**Measured.** `mes.dispatch_shipped` fires on real shipment only, so a
warranty replacement, a free sample, or a consignment movement is a genuine
shipped dispatch with no invoice, forever. Under D3's name —
"shipments-without-invoice-provenance" — those read as defects in perpetuity,
and a report whose baseline is permanently non-zero is one people stop opening.

**Required.** Name the report for what it measures rather than for what is
wrong: it reports shipments with **no recorded invoice provenance**, which is a
fact, not a fault. State in the ADR that a non-zero count is expected and
enumerate why. A suppression/acknowledgement mechanism is a separate decision —
name it as deferred rather than building it, since a suppression flag on an
evidence report is exactly the kind of affordance that needs its own argument.

---

## S6b / A6 (fix in the same pass) — promote's idempotency key

`issue_invoice` is idempotency-keyed (`IdempotencyKey`, `issue_invoice.rs:662`)
and replays rather than double-mints (`AllocateOutcome::Replay`). Promote must
supply a key that makes a double-click a replay, and the only stable, natural
value is the **draft id** — which is what the phantom
`derive_from(draft.drf_id, "issue")` was reaching for.

Build it as a real, typed derivation, and say in the ADR that the key is for
**idempotency only** — the provenance is the typed field, never the key string.
That keeps S6's correction honest: the docs stop describing a traceability
mechanism, and the key stops being asked to carry meaning it cannot.

---

## Confirmed sound — pressed and held

- **Provenance on the invoice side.** `delete_draft_in_tx` reaches
  `dispatches.spawned_invoice_id` and the `invoice_draft` row; it has no path to
  a column on `invoice`. Ervin's "NOT NULLed on delete" holds by construction,
  not by discipline.
- **Derivation from the row, not the request.** The draft row carries
  `source_dispatch_id` / `source_wo_id` written by the spawner inside
  `mark_shipped`'s transaction. The operator names a draft; the server reads
  what it says. Correct.
- **No backfill.** The data to backfill from was deleted with the drafts. An
  honest absence beats a reconstructed guess in an evidence trail.
- **`BundleMembershipProbe` left alone.** Adding `source_dispatch_id` to a flat
  any-id-equality probe would sweep dispatch entries into invoice bundles by a
  transitive rule the probe cannot express — ADR-0122 §D1's distinction, applied
  correctly.

## Verdict

**FIX-FIRST.** Add site 6 and make the three doc corrections part of the work;
close S2 by flipping the draft's state instead of deleting it (which also
settles Q3); decide S3 and S4 explicitly in the ADR; build a real derivation for
promote's idempotency key; drop D4's now-unnecessary hedge. D1's atomicity, the
invoice-side placement, the row-derivation and the no-backfill posture need no
change.
